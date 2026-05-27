//! Live agent state. Drains `SystemEvent`s coming from driver threads, applies
//! transitions, and emits `RegistryEvent`s for the UI to consume.
//!
//! Driver threads (discovery tick, FSEvents, idle recheck, future kqueue)
//! produce `SystemEvent`s on a crossbeam channel. The registry thread owns
//! all mutable state; the UI is reduced to a fold over `RegistryEvent`s.

use crate::agent::{Agent, Tool, discover};
use crate::kqueue::KqueueCommand;
use crate::proc::{
    ClaudeSessionRecord, ProcessId, ProcessKey, claude_session_record_for, process_start_time,
};
use crate::status::{AgentState, compute_state};
use crate::trace::{self, ExitSource, LifecycleKind, TraceEvent};
use crate::transcript::{
    EventCategory, LifecycleEvent, TranscriptParser, claude::ClaudeParser, codex::CodexParser,
};
use chrono::{DateTime, Utc};
use crossbeam_channel::{Receiver, Sender};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug)]
pub enum SystemEvent {
    DiscoveryTick,
    ProcessExited(ProcessKey),
    TranscriptChanged(PathBuf),
    IdleRecheckTick,
    /// Snapshot request from the control socket. The registry replies
    /// with a clone of every live `Agent` it currently tracks. Handled
    /// inline (no batching) so callers get a synchronous reply.
    Snapshot {
        reply: Sender<Vec<Agent>>,
    },
}

#[derive(Debug, Clone)]
pub enum RegistryEvent {
    Added(Agent),
    StateChanged {
        pid: ProcessId,
        new_state: AgentState,
        last_lifecycle_at: Option<DateTime<Utc>>,
    },
    SubagentCountChanged {
        pid: ProcessId,
        count: usize,
    },
    ContextChanged {
        pid: ProcessId,
        tokens: u32,
        /// Model context window when the transcript carries it; `None`
        /// for Claude (consumer falls back to a default cap).
        max: Option<u32>,
    },
    /// Loose metadata observed from transcripts: which model is in
    /// use, the git branch the conversation is on, the conversation's
    /// AI-derived title, and the most-recently-invoked tool. Each
    /// field carries the *current* value so consumers don't need to
    /// reconcile partial deltas.
    MetadataChanged {
        pid: ProcessId,
        model: Option<String>,
        git_branch: Option<String>,
        session_title: Option<String>,
        current_action: Option<String>,
        last_action: Option<String>,
    },
    Removed(ProcessId),
}

#[derive(Default)]
struct MetadataPatch {
    model: Option<String>,
    git_branch: Option<String>,
    session_title: Option<String>,
    current_action: Option<Option<String>>,
}

struct Entry {
    agent: Agent,
    offset: u64,
    last_lifecycle: Option<LifecycleEvent>,
    last_write_at: Option<DateTime<Utc>>,
    session_state: Option<AgentState>,
}

pub struct Registry {
    entries: HashMap<ProcessKey, Entry>,
    /// Reverse index from transcript path to the process keys reading it.
    /// FSEvents notifies us by path; without this, every TranscriptChanged
    /// would linearly scan `entries`. Multiple pids can map to the same
    /// transcript during a session rebind window, hence the Vec value.
    by_transcript: HashMap<PathBuf, Vec<ProcessKey>>,
    out: Sender<RegistryEvent>,
    kqueue: Sender<KqueueCommand>,
    parsers: Parsers,
}

struct Parsers {
    claude: ClaudeParser,
    codex: CodexParser,
}

impl Registry {
    pub fn new(out: Sender<RegistryEvent>, kqueue: Sender<KqueueCommand>) -> Self {
        Self {
            entries: HashMap::new(),
            by_transcript: HashMap::new(),
            out,
            kqueue,
            parsers: Parsers {
                claude: ClaudeParser,
                codex: CodexParser,
            },
        }
    }

    fn index_transcript(&mut self, key: ProcessKey, path: PathBuf) {
        self.by_transcript.entry(path).or_default().push(key);
    }

    fn unindex_transcript(&mut self, key: ProcessKey, path: &std::path::Path) {
        if let Some(list) = self.by_transcript.get_mut(path) {
            list.retain(|&k| k != key);
            if list.is_empty() {
                self.by_transcript.remove(path);
            }
        }
    }

    /// Send a `RegistryEvent` to the UI. The receiver may legitimately
    /// be dropped (app shutdown), so a closed channel is not an error.
    fn emit(&self, ev: RegistryEvent) {
        let _ = self.out.send(ev);
    }

    /// Blocking loop: drain `events`, mutate the registry, push `RegistryEvent`s
    /// out. Returns when the `events` sender is dropped. After each blocking
    /// `recv` we drain any additional events available without blocking and
    /// coalesce duplicate `TranscriptChanged(path)` events so a burst of
    /// fs-notifications for the same file only triggers one reparse.
    pub fn run(&mut self, events: Receiver<SystemEvent>) {
        use std::collections::HashSet;
        while let Ok(first) = events.recv() {
            let mut paths: HashSet<PathBuf> = HashSet::new();
            let mut discovery = false;
            let mut idle_recheck = false;
            let mut exits: Vec<ProcessKey> = Vec::new();
            self.absorb_event(
                first,
                &mut paths,
                &mut discovery,
                &mut idle_recheck,
                &mut exits,
            );
            while let Ok(more) = events.try_recv() {
                self.absorb_event(
                    more,
                    &mut paths,
                    &mut discovery,
                    &mut idle_recheck,
                    &mut exits,
                );
            }
            // Discovery first (rebinds + new pids), then per-key exits,
            // then transcript reparses, then the idle-recheck sweep.
            if discovery {
                self.reconcile_discovery();
            }
            for key in exits {
                self.remove_key(key, ExitSource::Kqueue);
            }
            for path in paths {
                self.reparse_path(&path);
            }
            if idle_recheck {
                self.recompute_all();
            }
        }
    }

    fn absorb_event(
        &mut self,
        ev: SystemEvent,
        paths: &mut std::collections::HashSet<PathBuf>,
        discovery: &mut bool,
        idle_recheck: &mut bool,
        exits: &mut Vec<ProcessKey>,
    ) {
        match ev {
            SystemEvent::DiscoveryTick => *discovery = true,
            SystemEvent::IdleRecheckTick => *idle_recheck = true,
            SystemEvent::ProcessExited(key) => exits.push(key),
            SystemEvent::TranscriptChanged(path) => {
                paths.insert(path);
            }
            // The reply is best-effort: a control client that gave up on the
            // response will have dropped the receiver, which `send` then
            // reports as an error we can safely ignore.
            SystemEvent::Snapshot { reply } => {
                let agents: Vec<Agent> = self.entries.values().map(|e| e.agent.clone()).collect();
                let _ = reply.send(agents);
            }
        }
    }

    fn reconcile_discovery(&mut self) {
        let snapshot = match discover() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "discover() failed");
                return;
            }
        };
        self.reconcile_with_snapshot(snapshot);
    }

    /// Reconcile the registry against a discovery snapshot. Split from
    /// `reconcile_discovery` so unit tests can drive it with a
    /// hand-built `Vec<Agent>` instead of calling the live `discover()`.
    pub(crate) fn reconcile_with_snapshot(&mut self, snapshot: Vec<Agent>) {
        trace::emit(TraceEvent::DiscoveryPassStarted);
        let agent_count = snapshot.len();
        let mut alive: std::collections::HashSet<ProcessKey> = std::collections::HashSet::new();
        for agent in snapshot {
            let pid = agent.pid;
            let key = ProcessKey::for_pid_and_session(pid, agent.session_id);
            alive.insert(key);
            if !self.entries.contains_key(&key) {
                self.remove_other_keys_for_pid(pid, key);
            }
            match self.entries.get_mut(&key) {
                Some(existing) if existing.agent.transcript_path == agent.transcript_path => {
                    // Same session as before: only the subagent rollup
                    // can have changed (the process tree shifts on every
                    // Task-tool invocation). Still reparse below before
                    // moving to the next agent: FSEvents can miss appends
                    // from long-lived Codex writers, and discovery is the
                    // periodic backstop for those transcript updates.
                    let new_count = agent.subagent_pids.len();
                    if existing.agent.subagent_pids.len() != new_count {
                        existing.agent.subagent_pids = agent.subagent_pids;
                        trace::emit(TraceEvent::SubagentRollupChanged {
                            pid: pid.0,
                            count: new_count,
                        });
                        self.emit(RegistryEvent::SubagentCountChanged {
                            pid,
                            count: new_count,
                        });
                    }
                    self.reparse_key(key);
                    self.apply_claude_session_status(key);
                    continue;
                }
                Some(existing) => {
                    // Same process, different transcript: the user
                    // swapped sessions mid-flight (`/clear`, `/resume`,
                    // opened a new conversation in the same Claude).
                    // Rebind to the new file and wipe the parse state
                    // so the next `reparse_key` seeds from byte 0 of
                    // the new transcript instead of staying stuck on
                    // the old, idle one.
                    tracing::info!(
                        pid = %pid,
                        old = ?existing.agent.transcript_path,
                        new = ?agent.transcript_path,
                        "session switched; rebinding transcript"
                    );
                    let old_path = existing.agent.transcript_path.clone();
                    let new_path = agent.transcript_path.clone();
                    let new_session_id = agent.session_id;
                    existing.agent = agent;
                    existing.offset = 0;
                    existing.last_lifecycle = None;
                    existing.last_write_at = None;
                    existing.session_state = None;
                    self.unindex_transcript(key, &old_path);
                    self.index_transcript(key, new_path.clone());
                    trace::emit(TraceEvent::AgentRebound {
                        pid: pid.0,
                        old_transcript: old_path,
                        new_transcript: new_path,
                        new_session_id,
                    });
                    // Re-emit AgentDiscovered for the new binding so
                    // consumers (harness, future IPC subscribers) get
                    // the full agent record without having to stitch
                    // it together from AgentRebound + a separate
                    // snapshot lookup.
                    if let Some(e) = self.entries.get(&key) {
                        self.emit_agent_discovered(&e.agent);
                    }
                }
                None => {
                    self.emit_agent_discovered(&agent);
                    let path = agent.transcript_path.clone();
                    self.entries.insert(
                        key,
                        Entry {
                            agent,
                            offset: 0,
                            last_lifecycle: None,
                            last_write_at: None,
                            session_state: None,
                        },
                    );
                    self.index_transcript(key, path);
                    // Ask kqueue to watch this pid for NOTE_EXIT. The
                    // sender may have closed if the kqueue thread
                    // crashed; in that case we fall back to the 2s
                    // discovery tick for exit detection.
                    let _ = self.kqueue.send(KqueueCommand::Watch(key));
                }
            }
            // Seed (or re-seed) by parsing the bound transcript from
            // byte 0 so we don't misclassify it as "Active forever".
            self.reparse_key(key);
            self.apply_claude_session_status(key);
            // Re-emit the agent so consumers re-key the tile against
            // the (possibly new) transcript / session id.
            if let Some(e) = self.entries.get(&key) {
                self.emit(RegistryEvent::Added(e.agent.clone()));
            }
        }
        // Anything that vanished from the snapshot is gone.
        let stale: Vec<ProcessKey> = self
            .entries
            .keys()
            .copied()
            .filter(|key| !alive.contains(key))
            .collect();
        for key in stale {
            self.remove_key(key, ExitSource::DiscoveryReconcile);
        }
        trace::emit(TraceEvent::DiscoveryPassCompleted { agent_count });
    }

    fn apply_claude_session_status(&mut self, key: ProcessKey) {
        let Some(entry) = self.entries.get_mut(&key) else {
            return;
        };
        if !matches!(entry.agent.tool, Tool::Claude) {
            return;
        }
        let Some(rec) = claude_session_record_for(entry.agent.pid) else {
            entry.session_state = None;
            self.emit_state_if_changed(key, Utc::now());
            return;
        };
        let Some(session_state) = claude_session_state(&rec) else {
            entry.session_state = None;
            self.emit_state_if_changed(key, Utc::now());
            return;
        };
        let at = rec
            .updated_at_ms
            .and_then(DateTime::<Utc>::from_timestamp_millis)
            .unwrap_or_else(Utc::now);
        entry.session_state = Some(session_state);
        entry.last_write_at = Some(at);
        entry.agent.last_lifecycle_at = Some(at);
        if session_state == AgentState::AwaitingUser {
            let action = rec
                .waiting_for
                .as_deref()
                .map(waiting_action_label)
                .unwrap_or("Waiting");
            if entry.agent.current_action.as_deref() != Some(action) {
                entry.agent.last_action = entry.agent.current_action.take();
                entry.agent.current_action = Some(action.into());
                let _ = self.out.send(RegistryEvent::MetadataChanged {
                    pid: entry.agent.pid,
                    model: entry.agent.model.clone(),
                    git_branch: entry.agent.git_branch.clone(),
                    session_title: entry.agent.session_title.clone(),
                    current_action: entry.agent.current_action.clone(),
                    last_action: entry.agent.last_action.clone(),
                });
            }
        } else if entry
            .agent
            .current_action
            .as_deref()
            .is_some_and(is_session_wait_action)
        {
            entry.agent.last_action = entry.agent.current_action.take();
            let _ = self.out.send(RegistryEvent::MetadataChanged {
                pid: entry.agent.pid,
                model: entry.agent.model.clone(),
                git_branch: entry.agent.git_branch.clone(),
                session_title: entry.agent.session_title.clone(),
                current_action: entry.agent.current_action.clone(),
                last_action: entry.agent.last_action.clone(),
            });
        }
        self.emit_state_if_changed(key, Utc::now());
    }

    fn emit_agent_discovered(&self, agent: &Agent) {
        let process_start_ms = process_start_time(agent.pid).map(|pst| {
            let dur = std::time::Duration::from_secs(pst.sec as u64)
                + std::time::Duration::from_micros(pst.usec as u64);
            dur.as_millis() as i64
        });
        trace::emit(TraceEvent::AgentDiscovered {
            pid: agent.pid.0,
            process_start_ms,
            agent: agent.tool.as_trace_kind(),
            transcript: agent.transcript_path.clone(),
            session_id: agent.session_id,
            cwd: agent.cwd.clone(),
            binding_source: agent.binding_source,
        });
    }

    fn reparse_path(&mut self, path: &std::path::Path) {
        // O(1) via the reverse index — common case is one process key per path.
        let Some(keys) = self.by_transcript.get(path).cloned() else {
            return;
        };
        for key in keys {
            self.reparse_key(key);
        }
    }

    fn reparse_key(&mut self, key: ProcessKey) {
        let Some(entry) = self.entries.get_mut(&key) else {
            return;
        };
        let pid = entry.agent.pid;
        let path = entry.agent.transcript_path.clone();
        let parser: &dyn TranscriptParser = match entry.agent.tool {
            Tool::Claude => &self.parsers.claude,
            Tool::CodexCli | Tool::CodexAppServer => &self.parsers.codex,
        };
        let prev_offset = entry.offset;
        let (events, new_offset) = match parser.parse_incremental_with_context(
            &path,
            entry.offset,
            entry.last_lifecycle.as_ref(),
        ) {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(error = %e, ?path, "parse_incremental failed");
                return;
            }
        };
        let observed_write_at = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .map(DateTime::<Utc>::from)
            .unwrap_or_else(Utc::now);
        // If the parser re-seeded to 0 because the file was truncated
        // below `prev_offset`, the prior `last_lifecycle` references
        // events that no longer exist on disk. Clear it so the next
        // pass classifies state from fresh evidence.
        if new_offset < prev_offset {
            entry.last_lifecycle = None;
        }
        entry.offset = new_offset;

        // Fan out events by `EventCategory`. State-driving lifecycle
        // events feed `compute_state`; context updates and metadata
        // updates mutate the agent record directly and emit their own
        // `RegistryEvent`s. Routing through the enum's `category()`
        // method centralises the assignment so a new variant must
        // declare its bucket in one place.
        let mut latest_lifecycle: Option<LifecycleEvent> = None;
        let mut latest_context: Option<(u32, Option<u32>)> = None;
        let mut latest_metadata: Option<MetadataPatch> = None;
        for ev in &events {
            match (ev.category(), ev) {
                (EventCategory::Context, LifecycleEvent::ContextUpdated { tokens, max, .. }) => {
                    latest_context = Some((*tokens, *max));
                }
                (
                    EventCategory::Metadata,
                    LifecycleEvent::MetadataUpdated {
                        model,
                        git_branch,
                        session_title,
                        current_action,
                        ..
                    },
                ) => {
                    let patch = latest_metadata.get_or_insert_with(MetadataPatch::default);
                    if let Some(v) = model {
                        patch.model = Some(v.clone());
                    }
                    if let Some(v) = git_branch {
                        patch.git_branch = Some(v.clone());
                    }
                    if let Some(v) = session_title {
                        patch.session_title = Some(v.clone());
                    }
                    if let Some(v) = current_action {
                        patch.current_action = Some(Some(v.clone()));
                    }
                }
                (EventCategory::Metadata, LifecycleEvent::CurrentActionCleared { .. }) => {
                    latest_metadata
                        .get_or_insert_with(MetadataPatch::default)
                        .current_action = Some(None);
                }
                (EventCategory::Lifecycle, other) => {
                    latest_lifecycle = Some(other.clone());
                }
                // Defensive: if a future variant's category() and
                // pattern disagree, skip rather than misroute.
                _ => {}
            }
        }

        if let Some(last) = latest_lifecycle.as_ref() {
            entry.last_lifecycle = Some(last.clone());
            entry.last_write_at = Some(observed_write_at);
            entry.agent.last_lifecycle_at = Some(last.at());
        } else if new_offset > prev_offset {
            // No lifecycle events this pass, but the file did grow —
            // refresh the write clock from the file's own mtime so
            // idle detection follows observed disk activity rather
            // than transcript event timestamps.
            entry.last_write_at = Some(observed_write_at);
        }

        if let Some((tokens, max)) = latest_context {
            let changed = entry.agent.context_tokens != Some(tokens)
                || (max.is_some() && entry.agent.context_max != max);
            entry.agent.context_tokens = Some(tokens);
            if max.is_some() {
                entry.agent.context_max = max;
            }
            if changed {
                let _ = self.out.send(RegistryEvent::ContextChanged {
                    pid,
                    tokens,
                    max: entry.agent.context_max,
                });
            }
        }

        let mut metadata_changed = false;
        if let Some(patch) = latest_metadata {
            let mut changed = false;
            changed |= update_field(&mut entry.agent.model, patch.model.as_ref());
            changed |= update_field(&mut entry.agent.git_branch, patch.git_branch.as_ref());
            changed |= update_field(&mut entry.agent.session_title, patch.session_title.as_ref());
            if let Some(v) = patch.current_action
                && entry.agent.current_action != v
            {
                // Rotate: the agent's *previous* current_action
                // becomes `last_action` so the tooltip can show
                // both "now doing X" and "just did Y".
                entry.agent.last_action = entry.agent.current_action.take();
                entry.agent.current_action = v;
                changed = true;
            }
            if changed {
                let _ = self.out.send(RegistryEvent::MetadataChanged {
                    pid,
                    model: entry.agent.model.clone(),
                    git_branch: entry.agent.git_branch.clone(),
                    session_title: entry.agent.session_title.clone(),
                    current_action: entry.agent.current_action.clone(),
                    last_action: entry.agent.last_action.clone(),
                });
                metadata_changed = true;
            }
        }

        let lifecycle = latest_lifecycle
            .as_ref()
            .and_then(|ev| LifecycleKind::try_from(ev).ok());
        trace::emit(TraceEvent::TranscriptParsed {
            pid: pid.0,
            lifecycle,
            metadata_changed,
        });

        self.emit_state_if_changed(key, Utc::now());
    }

    fn recompute_all(&mut self) {
        let now = Utc::now();
        let keys: Vec<ProcessKey> = self.entries.keys().copied().collect();
        for key in keys {
            self.emit_state_if_changed(key, now);
        }
    }

    fn emit_state_if_changed(&mut self, key: ProcessKey, now: DateTime<Utc>) {
        let Some(entry) = self.entries.get_mut(&key) else {
            return;
        };
        let pid = entry.agent.pid;
        let new = entry.session_state.unwrap_or_else(|| {
            compute_state(
                entry.last_lifecycle.as_ref(),
                entry.last_write_at,
                now,
                true,
            )
        });
        if entry.agent.state != new {
            entry.agent.state = new;
            let last_lifecycle_at = entry.agent.last_lifecycle_at;
            self.emit(RegistryEvent::StateChanged {
                pid,
                new_state: new,
                last_lifecycle_at,
            });
        }
    }

    fn remove_key(&mut self, key: ProcessKey, via: ExitSource) {
        if let Some(entry) = self.entries.remove(&key) {
            let pid = entry.agent.pid;
            let path = entry.agent.transcript_path.clone();
            self.unindex_transcript(key, &path);
            // Defensive: kqueue auto-clears `EV_ONESHOT` registrations on
            // NOTE_EXIT, so an Unwatch here is a no-op in the common case.
            // It matters only if a process was added but exited before we
            // ever heard about it, or if a non-exit removal path fires.
            let _ = self.kqueue.send(KqueueCommand::Unwatch(key));
            trace::emit(TraceEvent::ProcessExited { pid: pid.0, via });
            self.emit(RegistryEvent::Removed(pid));
        }
    }

    fn remove_other_keys_for_pid(&mut self, pid: ProcessId, keep: ProcessKey) {
        let stale: Vec<ProcessKey> = self
            .entries
            .keys()
            .copied()
            .filter(|key| key.pid == pid && *key != keep)
            .collect();
        for key in stale {
            self.remove_key(key, ExitSource::DiscoveryReconcile);
        }
    }
}

fn claude_session_state(rec: &ClaudeSessionRecord) -> Option<AgentState> {
    match rec.status.as_deref() {
        Some("waiting") => Some(AgentState::AwaitingUser),
        Some("busy") => Some(AgentState::Active),
        Some("idle") => Some(AgentState::Idle),
        _ => None,
    }
}

fn waiting_action_label(waiting_for: &str) -> &'static str {
    if waiting_for.contains("permission prompt") {
        "Permission prompt"
    } else if waiting_for.contains("AskUserQuestion") {
        "Asking question"
    } else {
        "Waiting"
    }
}

fn is_session_wait_action(action: &str) -> bool {
    matches!(action, "Waiting" | "Permission prompt" | "Asking question")
}

/// Overwrite `slot` with a clone of `new` when both are `Some` and differ;
/// returns whether the slot changed. `None` inputs are ignored.
fn update_field<T: Clone + PartialEq>(slot: &mut Option<T>, new: Option<&T>) -> bool {
    let Some(v) = new else { return false };
    if slot.as_ref() == Some(v) {
        return false;
    }
    *slot = Some(v.clone());
    true
}

#[cfg(test)]
mod discovery_bracket_tests {
    use super::*;
    use crate::trace::{
        AgentKind, BindingSource, ParsedLine,
        testing::{with_sink, without_sink},
    };
    use crossbeam_channel::unbounded;
    use std::io::Write;
    use uuid::Uuid;

    fn make_agent(pid: i32, transcript: std::path::PathBuf, session_id: Uuid) -> Agent {
        Agent {
            pid: ProcessId(pid),
            tool: Tool::Claude,
            session_id,
            transcript_path: transcript,
            cwd: None,
            tty: None,
            host_app: None,
            subagent_pids: Vec::new(),
            state: AgentState::Active,
            last_lifecycle_at: None,
            context_tokens: None,
            context_max: None,
            model: None,
            git_branch: None,
            session_title: None,
            current_action: None,
            last_action: None,
            binding_source: BindingSource::SessionRecord,
        }
    }

    fn make_session_record(status: Option<&str>, waiting_for: Option<&str>) -> ClaudeSessionRecord {
        ClaudeSessionRecord {
            pid: ProcessId(123),
            session_id: Uuid::new_v4(),
            cwd: std::path::PathBuf::from("/tmp"),
            status: status.map(str::to_owned),
            waiting_for: waiting_for.map(str::to_owned),
            started_at_ms: Some(1_779_879_012_962),
            updated_at_ms: Some(1_779_880_048_961),
        }
    }

    #[test]
    fn claude_session_status_drives_state() {
        assert_eq!(
            claude_session_state(&make_session_record(
                Some("waiting"),
                Some("permission prompt")
            )),
            Some(AgentState::AwaitingUser)
        );
        assert_eq!(
            claude_session_state(&make_session_record(Some("busy"), None)),
            Some(AgentState::Active)
        );
        assert_eq!(
            claude_session_state(&make_session_record(Some("idle"), None)),
            Some(AgentState::Idle)
        );
    }

    #[test]
    fn unknown_claude_session_status_falls_back_to_transcript_state() {
        assert_eq!(claude_session_state(&make_session_record(None, None)), None);
        assert_eq!(
            claude_session_state(&make_session_record(Some("new-future-status"), None)),
            None
        );
    }

    #[test]
    fn claude_waiting_reason_labels_known_prompts() {
        assert_eq!(
            waiting_action_label("permission prompt"),
            "Permission prompt"
        );
        assert_eq!(
            waiting_action_label("approve AskUserQuestion"),
            "Asking question"
        );
        assert_eq!(waiting_action_label("other"), "Waiting");
    }

    #[test]
    fn discovery_pass_brackets_per_pid_events() {
        // Real reparse needs a real file; an empty .jsonl produces a
        // `TranscriptParsed { lifecycle: None }` event inside the bracket.
        let tmp = tempfile::tempdir().unwrap();
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();
        let path_a = tmp.path().join(format!("{id_a}.jsonl"));
        let path_b = tmp.path().join(format!("{id_b}.jsonl"));
        std::fs::File::create(&path_a)
            .unwrap()
            .write_all(b"")
            .unwrap();
        std::fs::File::create(&path_b)
            .unwrap()
            .write_all(b"")
            .unwrap();

        let snapshot = vec![make_agent(123, path_a, id_a), make_agent(456, path_b, id_b)];

        let (out_tx, _out_rx) = unbounded();
        let (kq_tx, _kq_rx) = unbounded();
        let mut reg = Registry::new(out_tx, kq_tx);

        let (lines, _) = with_sink(|| reg.reconcile_with_snapshot(snapshot));
        let events: Vec<TraceEvent> = lines
            .iter()
            .map(|l| serde_json::from_str::<ParsedLine>(l).expect("parse").event)
            .collect();

        assert!(!events.is_empty());
        assert!(matches!(
            events.first(),
            Some(TraceEvent::DiscoveryPassStarted)
        ));
        assert!(matches!(
            events.last(),
            Some(TraceEvent::DiscoveryPassCompleted { agent_count: 2 }),
        ));

        let started = events
            .iter()
            .filter(|e| matches!(e, TraceEvent::DiscoveryPassStarted))
            .count();
        let completed = events
            .iter()
            .filter(|e| matches!(e, TraceEvent::DiscoveryPassCompleted { .. }))
            .count();
        assert_eq!(started, 1, "exactly one start event");
        assert_eq!(completed, 1, "exactly one completed event");

        let discovered: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                TraceEvent::AgentDiscovered {
                    agent,
                    binding_source,
                    ..
                } => Some((*agent, *binding_source)),
                _ => None,
            })
            .collect();
        assert_eq!(discovered.len(), 2);
        assert!(
            discovered
                .iter()
                .all(|(a, s)| *a == AgentKind::Claude && *s == BindingSource::SessionRecord),
        );
    }

    #[test]
    fn discovery_tick_reparses_existing_transcript() {
        without_sink(|| {
            let tmp = tempfile::tempdir().unwrap();
            let id = Uuid::new_v4();
            let path = tmp.path().join(format!("{id}.jsonl"));
            std::fs::File::create(&path).unwrap();

            let mut agent = make_agent(123, path.clone(), id);
            agent.tool = Tool::CodexCli;
            let snapshot = vec![agent];
            let (out_tx, out_rx) = unbounded();
            let (kq_tx, _kq_rx) = unbounded();
            let mut reg = Registry::new(out_tx, kq_tx);

            reg.reconcile_with_snapshot(snapshot.clone());
            out_rx.try_iter().for_each(drop);

            std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(
                br#"{"timestamp":"2026-05-18T10:55:24.911Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":4308176,"cached_input_tokens":4012032,"output_tokens":11381,"reasoning_output_tokens":3482,"total_tokens":4319557},"model_context_window":258400}}}"#,
            )
            .unwrap();
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(b"\n")
                .unwrap();

            reg.reconcile_with_snapshot(snapshot);

            assert!(
                out_rx.try_iter().any(|ev| matches!(
                    ev,
                    RegistryEvent::ContextChanged {
                        pid: ProcessId(123),
                        tokens: 4_308_176,
                        max: Some(258_400),
                    }
                )),
                "same-transcript discovery tick must backstop missed fs events"
            );
        });
    }

    #[test]
    fn fresh_write_uses_file_mtime_not_old_codex_event_time_for_idle() {
        without_sink(|| {
            let tmp = tempfile::tempdir().unwrap();
            let id = Uuid::new_v4();
            let path = tmp.path().join(format!("{id}.jsonl"));
            std::fs::write(
            &path,
            concat!(
                r#"{"timestamp":"2020-01-01T00:00:00Z","type":"event_msg","payload":{"type":"task_started"}}"#,
                "\n",
                r#"{"timestamp":"2020-01-01T00:00:01Z","type":"event_msg","payload":{"type":"task_complete"}}"#,
                "\n",
            ),
        )
        .unwrap();

            let mut agent = make_agent(123, path.clone(), id);
            agent.tool = Tool::CodexCli;
            let snapshot = vec![agent];
            let (out_tx, out_rx) = unbounded();
            let (kq_tx, _kq_rx) = unbounded();
            let mut reg = Registry::new(out_tx, kq_tx);

            reg.reconcile_with_snapshot(snapshot);
            let events: Vec<_> = out_rx.try_iter().collect();

            assert!(
                events.iter().any(|ev| matches!(
                    ev,
                    RegistryEvent::StateChanged {
                        pid: ProcessId(123),
                        new_state: AgentState::NeedsInput,
                        ..
                    }
                )),
                "freshly written transcript must not idle from old event timestamps: {events:?}"
            );
        });
    }

    #[test]
    fn ask_user_answer_clears_stale_current_action() {
        without_sink(|| {
            let tmp = tempfile::tempdir().unwrap();
            let id = Uuid::new_v4();
            let path = tmp.path().join(format!("{id}.jsonl"));
            std::fs::write(
            &path,
            concat!(
                r#"{"type":"assistant","timestamp":"2026-05-18T10:59:21.744Z","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","id":"toolu_x","name":"AskUserQuestion","input":{}}]}}"#,
                "\n"
            ),
        )
        .unwrap();

            let snapshot = vec![make_agent(123, path.clone(), id)];
            let (out_tx, out_rx) = unbounded();
            let (kq_tx, _kq_rx) = unbounded();
            let mut reg = Registry::new(out_tx, kq_tx);

            reg.reconcile_with_snapshot(snapshot.clone());
            out_rx.try_iter().for_each(drop);

            std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(
                concat!(
                    r#"{"type":"user","timestamp":"2026-05-18T10:59:46.865Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_x","content":"User has answered your questions."}]},"toolUseResult":{"questions":[{"question":"choose","options":[{"label":"A","description":"A"}],"multiSelect":false}],"answers":{"choose":"A"}}}"#,
                    "\n"
                )
                .as_bytes(),
            )
            .unwrap();

            reg.reconcile_with_snapshot(snapshot);
            let events: Vec<_> = out_rx.try_iter().collect();

            assert!(
                events.iter().any(|ev| matches!(
                    ev,
                    RegistryEvent::StateChanged {
                        pid: ProcessId(123),
                        new_state,
                        ..
                    } if *new_state != AgentState::AwaitingUser
                )),
                "answer should move the tile out of AwaitingUser: {events:?}"
            );
            assert!(
                events.iter().any(|ev| matches!(
                    ev,
                    RegistryEvent::MetadataChanged {
                        pid: ProcessId(123),
                        current_action: None,
                        last_action: Some(action),
                        ..
                    } if action == "Asking question"
                )),
                "answer should clear stale current_action: {events:?}"
            );
        });
    }
}
