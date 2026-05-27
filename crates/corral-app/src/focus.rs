//! Tile-click dispatch onto the background executor.
//!
//! The osascript adapters can take ~100ms in the slow path (cold AppleScript
//! permission prompt). We never want that on the foreground/render thread,
//! so the public entry point spawns onto `BackgroundExecutor` and returns.

use corral_adapters::{FocusContext, ParentApp, default_adapters, dispatch, resolve_parent_app};
use corral_core::agent::Tool;
use corral_core::proc::ProcessId;
use corral_core::trace::{self, TraceEvent};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic request id paired with each `FocusRequested` /
/// `FocusDispatched` event so the harness can correlate them.
pub fn next_request_id() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Synchronously dispatch focus for an agent. The control socket calls
/// this from its connection thread so the `FocusRequested` /
/// `FocusDispatched` events have landed in the trace before the socket
/// replies, letting the harness correlate without an extra wait.
pub fn focus_for_request(
    request_id: u64,
    cli_pid: ProcessId,
    tool: Tool,
    cli_tty: Option<PathBuf>,
    cwd: Option<PathBuf>,
) -> Result<(), String> {
    focus_agent(request_id, cli_pid, tool, cli_tty, cwd).map_err(|e| {
        tracing::warn!(error = %e, pid = %cli_pid, "focus dispatch failed");
        e.to_string()
    })
}

fn focus_agent(
    request_id: u64,
    cli_pid: ProcessId,
    tool: Tool,
    cli_tty: Option<PathBuf>,
    cwd: Option<PathBuf>,
) -> Result<(), corral_adapters::AdapterError> {
    let parent_app =
        resolve_parent_app(cli_pid).map(|(pid, bundle_id)| ParentApp { pid, bundle_id });
    let ctx = FocusContext {
        cli_pid,
        tool,
        cli_tty,
        parent_app,
        cwd,
        request_id,
    };
    trace::emit(TraceEvent::FocusRequested {
        pid: cli_pid.0,
        request_id,
    });
    let adapters = default_adapters();
    dispatch(&adapters, ctx).map(|_| ())
}
