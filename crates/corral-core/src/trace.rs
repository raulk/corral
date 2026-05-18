//! Structured trace events emitted by the corral for harness assertions.
//!
//! Two streams run in parallel: the existing `tracing::fmt` layer keeps
//! emitting human-readable logs to stderr, while this module emits a
//! versioned JSONL stream to a separately installed sink. The harness
//! reads the JSONL stream; the schema is the contract.
//!
//! Calls live at the same sites as today's `tracing::info!` *for the
//! events the harness asserts on*. Anything purely informational stays
//! on `tracing` and does not flow through here.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::transcript::LifecycleEvent;

/// Wire-format version. Bumped only when the schema changes in a way the
/// harness must adapt to. Adding optional fields is backwards-compatible
/// and does not require a bump.
pub const SCHEMA_VERSION: &str = "v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentKind {
    Claude,
    CodexCli,
    CodexAppServer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BindingSource {
    /// `~/.claude/sessions/<pid>.json` — Claude's own session record.
    SessionRecord,
    /// `--session-id` / `--resume` argv or `CLAUDE_CODE_SESSION_ID` env.
    ArgvEnv,
    /// Newest `<uuid>.jsonl` in the project dir (last-resort fallback).
    MtimeFallback,
    /// `proc_pidfdinfo`: an open file descriptor at discovery time.
    /// Used for Codex and rarely for Claude.
    OpenFd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FocusStrategy {
    Tty,
    Pid,
    Cwd,
    Generic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FocusResult {
    Ok,
    NotFound,
    Unavailable,
    PermissionDenied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExitSource {
    Kqueue,
    DiscoveryReconcile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LifecycleKind {
    TurnStarted,
    TurnEnded,
    AwaitingUser,
}

impl TryFrom<&LifecycleEvent> for LifecycleKind {
    type Error = ();
    fn try_from(ev: &LifecycleEvent) -> Result<Self, Self::Error> {
        Ok(match ev {
            LifecycleEvent::TurnStarted { .. } => Self::TurnStarted,
            LifecycleEvent::TurnEnded { .. } => Self::TurnEnded,
            LifecycleEvent::AwaitingUser { .. } => Self::AwaitingUser,
            LifecycleEvent::ContextUpdated { .. }
            | LifecycleEvent::MetadataUpdated { .. }
            | LifecycleEvent::CurrentActionCleared { .. } => return Err(()),
        })
    }
}

/// Versioned trace event. Variants serialize with an internal `kind` tag
/// in kebab-case (e.g. `"discovery-pass-started"`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TraceEvent {
    DiscoveryPassStarted,
    DiscoveryPassCompleted {
        agent_count: usize,
    },
    AgentDiscovered {
        pid: i32,
        /// Process start time in milliseconds since Unix epoch. `None`
        /// when the kernel's `kinfo_proc` slot is unreadable.
        process_start_ms: Option<i64>,
        agent: AgentKind,
        transcript: PathBuf,
        session_id: Uuid,
        cwd: Option<PathBuf>,
        binding_source: BindingSource,
    },
    AgentRebound {
        pid: i32,
        old_transcript: PathBuf,
        new_transcript: PathBuf,
        new_session_id: Uuid,
    },
    SubagentRollupChanged {
        pid: i32,
        count: usize,
    },
    ProcessExited {
        pid: i32,
        via: ExitSource,
    },
    TranscriptParsed {
        pid: i32,
        lifecycle: Option<LifecycleKind>,
        metadata_changed: bool,
    },
    GhosttyCapsProbed {
        has_tty: bool,
        has_pid: bool,
        has_cwd: bool,
    },
    FocusRequested {
        pid: i32,
        request_id: u64,
    },
    FocusDispatched {
        request_id: u64,
        /// Name of the adapter that produced the outcome. `None` when no
        /// adapter ran successfully (no matching parent app, no terminal
        /// adapter installed for the running app, etc.).
        adapter: Option<String>,
        strategy: Option<FocusStrategy>,
        result: FocusResult,
        /// Terminal-side id of the surface the adapter focused. Filled
        /// in by adapters that can read it back (Ghostty: `id of t`);
        /// `None` otherwise.
        focused_target_id: Option<String>,
    },
}

/// Envelope serialized to JSONL. `schema` brands the line; `ts` timestamps
/// it as an RFC3339 UTC instant; `event` is flattened so the `kind` tag
/// sits at the top level next to schema/ts.
#[derive(Debug, Serialize)]
pub struct TraceLine<'a> {
    pub schema: &'static str,
    pub ts: DateTime<Utc>,
    #[serde(flatten)]
    pub event: &'a TraceEvent,
}

/// Owned read-back envelope used by the harness side and by tests. Pairs
/// each parsed `TraceEvent` with its envelope metadata so consumers can
/// assert on schema/ts when needed.
#[derive(Debug, Deserialize)]
pub struct ParsedLine {
    pub schema: String,
    pub ts: DateTime<Utc>,
    #[serde(flatten)]
    pub event: TraceEvent,
}

/// Sink that consumes one serialized JSONL record at a time. Implementors
/// must be safe to call from multiple threads concurrently — emit happens
/// from the registry, discovery, and adapter threads.
pub trait TraceSink: Send + Sync {
    /// Write one JSONL line. The string does *not* include a trailing
    /// newline; the sink appends one (the harness reads line-by-line).
    fn emit_line(&self, line: &str);
}

type SharedSink = Arc<dyn TraceSink + Send + Sync>;

static SINK: RwLock<Option<SharedSink>> = RwLock::new(None);

/// Install the process-wide trace sink. Subsequent `emit` calls write
/// through `sink`. Replacing an existing sink is allowed; the previous
/// one is dropped.
pub fn install_sink(sink: SharedSink) {
    *SINK.write().expect("trace sink lock poisoned") = Some(sink);
}

/// Remove any installed sink. Useful for tests and for graceful shutdown.
pub fn clear_sink() {
    *SINK.write().expect("trace sink lock poisoned") = None;
}

/// Emit `event` to the currently installed sink, if any. Cheap when no
/// sink is installed: a single read-lock acquisition and an `Option`
/// check. Serialization only runs when a sink is present.
pub fn emit(event: TraceEvent) {
    let sink = match SINK.read() {
        Ok(guard) => guard.clone(),
        Err(_) => return,
    };
    let Some(sink) = sink else { return };
    let line = TraceLine {
        schema: SCHEMA_VERSION,
        ts: Utc::now(),
        event: &event,
    };
    match serde_json::to_string(&line) {
        Ok(s) => sink.emit_line(&s),
        Err(e) => tracing::warn!(error = %e, "trace serialization failed"),
    }
}

/// Test-only helpers. Crate-private so other modules can drive the
/// global sink under the shared test-mode mutex.
#[cfg(test)]
pub(crate) mod testing {
    use super::{TraceSink, clear_sink, install_sink};
    use std::sync::{Arc, Mutex};

    /// Serialize tests that touch the global sink. Held across the
    /// install/emit/clear sequence so two tests can't see each other's
    /// emissions.
    pub(crate) static TEST_GUARD: Mutex<()> = Mutex::new(());

    #[derive(Default)]
    pub(crate) struct VecSink(pub(crate) Mutex<Vec<String>>);

    impl TraceSink for VecSink {
        fn emit_line(&self, line: &str) {
            self.0.lock().expect("vec sink lock").push(line.to_owned());
        }
    }

    /// Run `f` with a freshly installed VecSink. Returns the lines the
    /// sink received in order plus `f`'s return value.
    pub(crate) fn with_sink<F, R>(f: F) -> (Vec<String>, R)
    where
        F: FnOnce() -> R,
    {
        let _guard = TEST_GUARD.lock().unwrap_or_else(|p| p.into_inner());
        let sink = Arc::new(VecSink::default());
        install_sink(sink.clone());
        let r = f();
        clear_sink();
        let lines = std::mem::take(&mut *sink.0.lock().unwrap());
        (lines, r)
    }

    /// Run `f` while holding the same guard used by `with_sink`, with
    /// no sink installed. Tests that emit trace events but do not assert
    /// on them use this to avoid polluting another test's captured sink.
    pub(crate) fn without_sink<F, R>(f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let _guard = TEST_GUARD.lock().unwrap_or_else(|p| p.into_inner());
        clear_sink();
        let r = f();
        clear_sink();
        r
    }
}

#[cfg(test)]
mod tests {
    use super::testing::with_sink;
    use super::*;

    fn parse_lines(lines: &[String]) -> Vec<ParsedLine> {
        lines
            .iter()
            .map(|l| serde_json::from_str::<ParsedLine>(l).expect("parse trace line"))
            .collect()
    }

    #[test]
    fn envelope_carries_schema_and_event() {
        let (lines, _) = with_sink(|| emit(TraceEvent::DiscoveryPassStarted));
        assert_eq!(lines.len(), 1);
        let parsed = parse_lines(&lines);
        assert_eq!(parsed[0].schema, SCHEMA_VERSION);
        assert!(matches!(parsed[0].event, TraceEvent::DiscoveryPassStarted));
    }

    #[test]
    fn round_trip_every_variant() {
        let pid = 12345;
        let session_id = Uuid::nil();
        let transcript = PathBuf::from("/tmp/transcript.jsonl");

        let (lines, _) = with_sink(|| {
            emit(TraceEvent::DiscoveryPassStarted);
            emit(TraceEvent::DiscoveryPassCompleted { agent_count: 2 });
            emit(TraceEvent::AgentDiscovered {
                pid,
                process_start_ms: Some(1_700_000_000_000),
                agent: AgentKind::Claude,
                transcript: transcript.clone(),
                session_id,
                cwd: Some(PathBuf::from("/Users/me/proj")),
                binding_source: BindingSource::SessionRecord,
            });
            emit(TraceEvent::AgentRebound {
                pid,
                old_transcript: transcript.clone(),
                new_transcript: PathBuf::from("/tmp/new.jsonl"),
                new_session_id: session_id,
            });
            emit(TraceEvent::SubagentRollupChanged { pid, count: 3 });
            emit(TraceEvent::ProcessExited {
                pid,
                via: ExitSource::Kqueue,
            });
            emit(TraceEvent::TranscriptParsed {
                pid,
                lifecycle: Some(LifecycleKind::TurnStarted),
                metadata_changed: true,
            });
            emit(TraceEvent::GhosttyCapsProbed {
                has_tty: true,
                has_pid: false,
                has_cwd: true,
            });
            emit(TraceEvent::FocusRequested { pid, request_id: 7 });
            emit(TraceEvent::FocusDispatched {
                request_id: 7,
                adapter: Some("ghostty".into()),
                strategy: Some(FocusStrategy::Tty),
                result: FocusResult::Ok,
                focused_target_id: Some("42".into()),
            });
        });

        let parsed = parse_lines(&lines);
        for line in &parsed {
            assert_eq!(line.schema, SCHEMA_VERSION);
        }

        assert!(matches!(parsed[0].event, TraceEvent::DiscoveryPassStarted));
        assert!(matches!(
            parsed[1].event,
            TraceEvent::DiscoveryPassCompleted { agent_count: 2 }
        ));
        match &parsed[2].event {
            TraceEvent::AgentDiscovered {
                pid: p,
                agent,
                binding_source,
                ..
            } => {
                assert_eq!(*p, pid);
                assert_eq!(*agent, AgentKind::Claude);
                assert_eq!(*binding_source, BindingSource::SessionRecord);
            }
            other => panic!("expected agent-discovered, got {other:?}"),
        }
        assert!(matches!(parsed[3].event, TraceEvent::AgentRebound { .. }));
        assert!(matches!(
            parsed[4].event,
            TraceEvent::SubagentRollupChanged { count: 3, .. }
        ));
        assert!(matches!(
            parsed[5].event,
            TraceEvent::ProcessExited {
                via: ExitSource::Kqueue,
                ..
            }
        ));
        assert!(matches!(
            parsed[6].event,
            TraceEvent::TranscriptParsed {
                lifecycle: Some(LifecycleKind::TurnStarted),
                metadata_changed: true,
                ..
            }
        ));
        assert!(matches!(
            parsed[7].event,
            TraceEvent::GhosttyCapsProbed {
                has_tty: true,
                has_pid: false,
                has_cwd: true,
            }
        ));
        assert!(matches!(
            parsed[8].event,
            TraceEvent::FocusRequested { request_id: 7, .. }
        ));
        match &parsed[9].event {
            TraceEvent::FocusDispatched {
                request_id,
                adapter,
                strategy,
                result,
                focused_target_id,
            } => {
                assert_eq!(*request_id, 7);
                assert_eq!(adapter.as_deref(), Some("ghostty"));
                assert_eq!(*strategy, Some(FocusStrategy::Tty));
                assert_eq!(*result, FocusResult::Ok);
                assert_eq!(focused_target_id.as_deref(), Some("42"));
            }
            other => panic!("expected focus-dispatched, got {other:?}"),
        }
    }

    #[test]
    fn emit_is_noop_without_sink() {
        testing::without_sink(|| emit(TraceEvent::DiscoveryPassStarted));
    }
}
