use crate::proc::{
    self, ProcError, ProcessId, claude_transcript_for_excluding, list_processes_matching,
    process_cwd, process_open_session_transcript, process_tty, uuid_from_transcript_filename,
};
use crate::status::AgentState;
use crate::trace::{AgentKind, BindingSource};
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Claude,
    CodexCli,
    CodexAppServer,
}

impl Tool {
    pub fn as_trace_kind(self) -> AgentKind {
        match self {
            Tool::Claude => AgentKind::Claude,
            Tool::CodexCli => AgentKind::CodexCli,
            Tool::CodexAppServer => AgentKind::CodexAppServer,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Agent {
    pub pid: ProcessId,
    pub tool: Tool,
    pub session_id: Uuid,
    pub transcript_path: PathBuf,
    /// Process working directory at discovery time. `None` when libproc
    /// couldn't read it — rare, and typically for transient or
    /// permission-restricted processes.
    pub cwd: Option<PathBuf>,
    pub tty: Option<PathBuf>,
    /// Name of the host macOS application running this agent — the
    /// terminal (Terminal.app, iTerm, Warp, etc.) for TTY-attached agents,
    /// or the GUI host for app-server agents. `None` when no `.app`
    /// ancestor is reachable from the agent's parent chain.
    pub host_app: Option<String>,
    pub subagent_pids: Vec<ProcessId>,
    pub state: AgentState,
    pub last_lifecycle_at: Option<DateTime<Utc>>,
    /// Latest observed prompt-token count from the transcript. `None`
    /// when no usage event has been parsed yet.
    pub context_tokens: Option<u32>,
    /// Model context window in tokens. Codex surfaces this on every
    /// `token_count` event; for Claude the parser resolves it from
    /// the model id and emits it here too.
    pub context_max: Option<u32>,
    /// Model id reported by the transcript (e.g. `claude-opus-4-7`,
    /// `claude-sonnet-4-6`). `None` until the first assistant turn
    /// has been parsed.
    pub model: Option<String>,
    /// Git branch the conversation is on. Pulled from Claude's
    /// `gitBranch` field on each line; absent for Codex.
    pub git_branch: Option<String>,
    /// AI-derived session title — Claude's `ai-title` / `slug` field
    /// renamed into something more readable. Useful for
    /// distinguishing concurrent sessions at a glance.
    pub session_title: Option<String>,
    /// Short label describing the most recent tool invocation the
    /// assistant made (e.g. `$ git status`, `Reading src/main.rs`).
    /// Updated every time an assistant message contains a tool_use.
    pub current_action: Option<String>,
    /// Previous `current_action`, kept so the tooltip can show both
    /// "doing now" and "just did" without losing context as the
    /// agent moves between tools quickly.
    pub last_action: Option<String>,
    /// Provenance of the transcript binding for this agent. Surfaced
    /// in the `AgentDiscovered` trace event so the harness can assert
    /// that, e.g., Claude with a session record was bound via the
    /// record path rather than the mtime fallback.
    pub binding_source: BindingSource,
}

#[derive(Debug, thiserror::Error)]
pub enum DiscoverError {
    #[error("process inspection failed: {0}")]
    Proc(#[from] ProcError),
}

/// Snapshot of running agent processes that have already opened their session
/// transcript. A process that has launched but not yet opened a transcript is
/// returned on a subsequent tick.
///
/// Discovers Claude + Codex CLI/app-server processes that satisfy:
///   - basename matches one of the known agent binary names
///   - first-class processes are TTY-attached (Codex CLI, Claude) or are the
///     Codex app-server (which has no TTY but is identified by its argv).
///   - a transcript file under `~/.claude/projects/` or `~/.codex/sessions/`
///     is currently open / resolvable.
///
/// TTY-less Claude processes are treated as subagents: a second pass walks
/// each one up the parent chain and appends its pid to the first first-class
/// Claude ancestor's `subagent_pids`.
pub fn discover() -> Result<Vec<Agent>, DiscoverError> {
    // Claude's CLI distribution is the self-contained Node binary `claude.exe`;
    // the macOS Claude.app bundles it as `MacOS/claude`. Codex ships an
    // architecture-suffixed static binary, plus a thin shell launcher that
    // execs under argv[0]="codex". Accept all the basenames we've seen.
    let raw = list_processes_matching(&[
        "claude",
        "claude.exe",
        "codex",
        "codex-aarch64-apple-darwin",
        "codex-x86_64-apple-darwin",
    ])?;
    let mut out: Vec<Agent> = Vec::new();
    let mut subagent_candidates: Vec<ProcessId> = Vec::new();
    // Tracks Claude transcripts already claimed in this pass, keyed by
    // project dir. Prevents multiple Claude PIDs sharing a cwd from
    // collapsing onto a single .jsonl via the mtime fallback (the UI
    // then shows N duplicate tiles pointing at the same session).
    let mut claimed_transcripts: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();

    for (pid, argv) in raw {
        let Some(tool) = classify_proc(&argv) else {
            continue;
        };
        let tty = process_tty(pid);
        let is_first_class = matches!(tool, Tool::CodexAppServer) || tty.is_some();
        if !is_first_class {
            // TTY-less Claude is almost certainly a subagent spawned by a
            // first-class Claude via the Task tool. Hold the pid for the
            // second-pass rollup below.
            if matches!(tool, Tool::Claude) {
                subagent_candidates.push(pid);
            }
            continue;
        }
        if let Some(agent) = build_agent(pid, tool, tty, &claimed_transcripts) {
            if matches!(agent.tool, Tool::Claude)
                && let Some(dir) = agent.transcript_path.parent()
            {
                claimed_transcripts
                    .entry(dir.to_path_buf())
                    .or_default()
                    .insert(agent.transcript_path.clone());
            }
            out.push(agent);
        }
    }

    // Second pass: roll each subagent up under its nearest first-class
    // Claude ancestor. Multiple subagents typically share long stretches
    // of ancestry (they descend from the same `claude` parent), so cache
    // `process_parent` lookups within the pass — each one is a sysctl.
    let mut parent_cache: HashMap<ProcessId, Option<ProcessId>> = HashMap::new();
    for sub in subagent_candidates {
        let mut cur = parent_of(sub, &mut parent_cache);
        while let Some(p) = cur {
            if let Some(parent) = out
                .iter_mut()
                .find(|a| a.pid == p && matches!(a.tool, Tool::Claude))
            {
                parent.subagent_pids.push(sub);
                break;
            }
            cur = parent_of(p, &mut parent_cache);
        }
    }

    Ok(out)
}

fn parent_of(
    pid: ProcessId,
    cache: &mut HashMap<ProcessId, Option<ProcessId>>,
) -> Option<ProcessId> {
    *cache
        .entry(pid)
        .or_insert_with(|| proc::process_parent(pid))
}

/// Classify a process's argv into a `Tool` variant. `argv[0]` is the
/// kernel's exec_path; the process's own argv[0] is at index 1; its
/// first user-visible flag is at index 2.
fn classify_proc(argv: &[String]) -> Option<Tool> {
    let basename_at = |i: usize| {
        argv.get(i)
            .map(|s| std::path::Path::new(s).file_name())
            .and_then(|n| n.and_then(|s| s.to_str()))
            .unwrap_or_default()
    };
    let exec_basename = basename_at(0);
    let proc_argv0_basename = basename_at(1);
    let proc_argv1 = argv.get(2).map(String::as_str).unwrap_or("");

    let is_claude =
        matches!(exec_basename, "claude" | "claude.exe") || proc_argv0_basename == "claude";
    let is_codex = exec_basename.starts_with("codex") || proc_argv0_basename == "codex";

    if is_claude {
        Some(Tool::Claude)
    } else if is_codex && proc_argv1 == "app-server" {
        Some(Tool::CodexAppServer)
    } else if is_codex {
        Some(Tool::CodexCli)
    } else {
        None
    }
}

/// Build a complete `Agent` for a first-class agent process (one that
/// owns a tty, or is the Codex app-server). Returns `None` when the
/// transcript can't be located yet — discovery retries on the next tick.
fn build_agent(
    pid: ProcessId,
    tool: Tool,
    tty: Option<PathBuf>,
    claimed_transcripts: &HashMap<PathBuf, HashSet<PathBuf>>,
) -> Option<Agent> {
    let (transcript_path, session_id, binding_source) = match tool {
        // Claude opens-writes-closes its transcript per line, so the FD
        // path rarely yields a hit. Try the session record first (gives us
        // the session id directly), then argv/env, then mtime fallback.
        // The claimed map ensures the mtime fallback hands a distinct .jsonl
        // to each Claude PID when several share a project dir.
        Tool::Claude => {
            if let Some((path, id, src)) = claude_transcript_for_excluding(pid, claimed_transcripts)
            {
                (path, id, src)
            } else {
                let path = process_open_session_transcript(pid)?;
                let id = uuid_from_transcript_filename(&path)?;
                (path, id, BindingSource::OpenFd)
            }
        }
        Tool::CodexCli | Tool::CodexAppServer => {
            let path = process_open_session_transcript(pid)?;
            let id = uuid_from_transcript_filename(&path)?;
            (path, id, BindingSource::OpenFd)
        }
    };
    let cwd = process_cwd(pid);
    let host_app = find_host_app(pid);

    Some(Agent {
        pid,
        tool,
        session_id,
        transcript_path,
        cwd,
        tty,
        host_app,
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
        binding_source,
    })
}

/// Upper bound on host-app parent walks. Pathological cycles are
/// impossible (Darwin process tree is a DAG with ppid eventually
/// reaching launchd), but bound to defend against future bugs.
const HOST_APP_MAX_HOPS: u32 = 32;

/// Walk the agent's parent chain looking for the first process that runs
/// from inside a macOS `.app` bundle. That's the terminal (or GUI app)
/// hosting the agent — Terminal.app, iTerm, Warp, VS Code, etc.
fn find_host_app(pid: ProcessId) -> Option<String> {
    proc::walk_parents(pid, HOST_APP_MAX_HOPS, proc::process_app_name)
}
