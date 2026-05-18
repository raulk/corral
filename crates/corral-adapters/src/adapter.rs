use crate::generic::GenericAdapter;
use corral_core::proc::ProcessId;
use corral_core::trace::{self, FocusResult, FocusStrategy, TraceEvent};
use objc2_app_kit::NSRunningApplication;
use objc2_foundation::NSString;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct ParentApp {
    pub pid: ProcessId,
    pub bundle_id: String,
}

#[derive(Debug, Clone)]
pub struct FocusContext {
    pub cli_pid: ProcessId,
    pub cli_tty: Option<PathBuf>,
    pub parent_app: Option<ParentApp>,
    /// The agent's working directory. `None` when libproc couldn't
    /// read it. Used by adapters whose terminal emulator does not
    /// expose per-tab tty/pid via scripting but does surface the cwd
    /// in the visible tab title (e.g. Ghostty).
    pub cwd: Option<PathBuf>,
    /// Correlation id paired with the `FocusRequested` trace event so
    /// the harness can match a request to its eventual `FocusDispatched`.
    pub request_id: u64,
}

/// What an adapter learned while focusing. `strategy` is the per-adapter
/// path that succeeded (Ghostty: tty/pid/cwd; iTerm: tty; etc.).
/// `focused_target_id` is the terminal's own surface id when the adapter
/// can read it back (e.g. `id of terminal t` in Ghostty's sdef); `None`
/// otherwise.
#[derive(Debug, Clone, Default)]
pub struct FocusOutcome {
    pub strategy: Option<FocusStrategy>,
    pub focused_target_id: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("permission denied (Accessibility or Automation)")]
    PermissionDenied,
    #[error("adapter target not found: {0}")]
    NotFound(String),
    #[error("adapter unavailable: {0}")]
    Unavailable(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub trait TerminalAdapter: Send + Sync {
    fn name(&self) -> &'static str;
    fn bundle_id(&self) -> Option<&'static str>;
    fn matches(&self, bundle_id: &str) -> bool;
    fn focus(&self, ctx: &FocusContext) -> Result<FocusOutcome, AdapterError>;
}

/// Iterate `adapters`, pick the first that claims the bundle id, call its
/// `focus`. Falls back to [`GenericAdapter`] for unknown apps or permission
/// failures only. Target misses from a matched adapter are real misses and
/// must surface to the caller.
///
/// Emits a `FocusDispatched` trace event on every terminating path so the
/// harness can correlate the request and the outcome.
pub fn dispatch(
    adapters: &[Box<dyn TerminalAdapter>],
    ctx: FocusContext,
) -> Result<FocusOutcome, AdapterError> {
    dispatch_with_generic(adapters, ctx, &GenericAdapter)
}

fn dispatch_with_generic(
    adapters: &[Box<dyn TerminalAdapter>],
    ctx: FocusContext,
    generic: &dyn TerminalAdapter,
) -> Result<FocusOutcome, AdapterError> {
    let request_id = ctx.request_id;
    let dispatched = dispatch_inner(adapters, ctx, generic);
    emit_focus_dispatched(request_id, &dispatched);
    dispatched.map(FocusOutcome::from)
}

fn dispatch_inner(
    adapters: &[Box<dyn TerminalAdapter>],
    ctx: FocusContext,
    generic: &dyn TerminalAdapter,
) -> Result<DispatchedFocus, AdapterError> {
    let Some(parent_app) = ctx.parent_app.as_ref() else {
        return dispatch_by_terminal_identity(adapters, &ctx);
    };

    for a in adapters {
        if a.matches(&parent_app.bundle_id) {
            match a.focus(&ctx) {
                Ok(outcome) => return Ok(named_outcome(a.name(), outcome)),
                Err(e) if e.is_permission_denied() => {
                    tracing::info!(
                        adapter = a.name(),
                        bundle = parent_app.bundle_id.as_str(),
                        "adapter permission denied, falling back to generic",
                    );
                    return generic
                        .focus(&ctx)
                        .map(|outcome| named_outcome(generic.name(), outcome));
                }
                Err(e) => return Err(e),
            }
        }
    }
    generic
        .focus(&ctx)
        .map(|outcome| named_outcome(generic.name(), outcome))
}

fn dispatch_by_terminal_identity(
    adapters: &[Box<dyn TerminalAdapter>],
    ctx: &FocusContext,
) -> Result<DispatchedFocus, AdapterError> {
    let mut saw_running_adapter = false;
    let mut last_err = None;
    for adapter in adapters {
        let Some(bundle_id) = adapter.bundle_id() else {
            continue;
        };
        let Some(parent_app) = running_app_for_bundle(bundle_id) else {
            continue;
        };
        saw_running_adapter = true;
        let mut ctx = ctx.clone();
        ctx.parent_app = Some(parent_app);
        match adapter.focus(&ctx) {
            Ok(outcome) => return Ok(named_outcome(adapter.name(), outcome)),
            Err(e @ (AdapterError::NotFound(_) | AdapterError::Unavailable(_))) => {
                last_err = Some(e)
            }
            Err(e) if e.is_permission_denied() => {
                last_err = Some(e);
            }
            Err(e) => return Err(e),
        }
    }

    if let Some(e) = last_err {
        return Err(e);
    }
    if saw_running_adapter {
        Err(AdapterError::NotFound(
            "no running terminal adapter matched the agent tty or process".into(),
        ))
    } else {
        Err(AdapterError::Unavailable(
            "no GUI parent and no known terminal app is running".into(),
        ))
    }
}

/// Result returned from the internal dispatch path: the outcome plus the
/// name of the adapter that produced it. The public `dispatch` strips the
/// adapter name back off before returning to keep the caller-facing type
/// stable.
struct DispatchedFocus {
    adapter: &'static str,
    outcome: FocusOutcome,
}

fn named_outcome(adapter: &'static str, outcome: FocusOutcome) -> DispatchedFocus {
    DispatchedFocus { adapter, outcome }
}

fn emit_focus_dispatched(request_id: u64, result: &Result<DispatchedFocus, AdapterError>) {
    let (adapter, strategy, focus_result, target_id) = match result {
        Ok(d) => (
            Some(d.adapter.to_owned()),
            d.outcome.strategy,
            FocusResult::Ok,
            d.outcome.focused_target_id.clone(),
        ),
        Err(AdapterError::NotFound(_)) => (None, None, FocusResult::NotFound, None),
        Err(AdapterError::Unavailable(_)) => (None, None, FocusResult::Unavailable, None),
        Err(AdapterError::PermissionDenied) => (None, None, FocusResult::PermissionDenied, None),
        // I/O errors that are clearly permission-related already map to
        // PermissionDenied via the adapters' own errors; anything left
        // is genuinely "couldn't talk to the OS", so report it as
        // unavailable rather than coining a new wire variant.
        Err(AdapterError::Io(_)) => (None, None, FocusResult::Unavailable, None),
    };
    trace::emit(TraceEvent::FocusDispatched {
        request_id,
        adapter,
        strategy,
        result: focus_result,
        focused_target_id: target_id,
    });
}

// Public dispatch returns only the outcome — adapter name leaks via the
// trace event, not the return type — so wrap the inner DispatchedFocus.
impl From<DispatchedFocus> for FocusOutcome {
    fn from(d: DispatchedFocus) -> Self {
        d.outcome
    }
}

fn running_app_for_bundle(bundle_id: &str) -> Option<ParentApp> {
    let bundle = NSString::from_str(bundle_id);
    let apps = NSRunningApplication::runningApplicationsWithBundleIdentifier(&bundle);
    let app = apps.firstObject()?;
    Some(ParentApp {
        pid: ProcessId(app.processIdentifier()),
        bundle_id: bundle_id.to_string(),
    })
}

impl AdapterError {
    fn is_permission_denied(&self) -> bool {
        matches!(self, AdapterError::PermissionDenied)
            || matches!(self, AdapterError::Io(e) if e.kind() == std::io::ErrorKind::PermissionDenied)
    }
}

pub fn default_adapters() -> Vec<Box<dyn TerminalAdapter>> {
    vec![
        Box::new(crate::iterm2::Iterm2Adapter),
        Box::new(crate::terminal_app::TerminalAppAdapter),
        Box::new(crate::ghostty::GhosttyAdapter),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct StubAdapter {
        bundle_id: &'static str,
        result: fn() -> Result<FocusOutcome, AdapterError>,
    }

    impl TerminalAdapter for StubAdapter {
        fn name(&self) -> &'static str {
            "stub"
        }

        fn bundle_id(&self) -> Option<&'static str> {
            Some(self.bundle_id)
        }

        fn matches(&self, bundle_id: &str) -> bool {
            bundle_id == self.bundle_id
        }

        fn focus(&self, _ctx: &FocusContext) -> Result<FocusOutcome, AdapterError> {
            (self.result)()
        }
    }

    struct CountingGeneric {
        calls: Arc<AtomicUsize>,
    }

    impl TerminalAdapter for CountingGeneric {
        fn name(&self) -> &'static str {
            "generic"
        }

        fn bundle_id(&self) -> Option<&'static str> {
            None
        }

        fn matches(&self, _bundle_id: &str) -> bool {
            true
        }

        fn focus(&self, _ctx: &FocusContext) -> Result<FocusOutcome, AdapterError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(FocusOutcome::default())
        }
    }

    fn ctx() -> FocusContext {
        FocusContext {
            cli_pid: ProcessId(42),
            cli_tty: None,
            parent_app: Some(ParentApp {
                pid: ProcessId(7),
                bundle_id: "com.example.Terminal".into(),
            }),
            cwd: None,
            request_id: 0,
        }
    }

    #[test]
    fn matched_not_found_does_not_fall_back_to_generic() {
        let adapters: Vec<Box<dyn TerminalAdapter>> = vec![Box::new(StubAdapter {
            bundle_id: "com.example.Terminal",
            result: || Err(AdapterError::NotFound("missing tty".into())),
        })];
        let calls = Arc::new(AtomicUsize::new(0));
        let generic = CountingGeneric {
            calls: calls.clone(),
        };

        let err = dispatch_with_generic(&adapters, ctx(), &generic).unwrap_err();

        assert!(matches!(err, AdapterError::NotFound(_)));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn matched_permission_denied_falls_back_to_generic() {
        let adapters: Vec<Box<dyn TerminalAdapter>> = vec![Box::new(StubAdapter {
            bundle_id: "com.example.Terminal",
            result: || Err(AdapterError::PermissionDenied),
        })];
        let calls = Arc::new(AtomicUsize::new(0));
        let generic = CountingGeneric {
            calls: calls.clone(),
        };

        dispatch_with_generic(&adapters, ctx(), &generic).unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
