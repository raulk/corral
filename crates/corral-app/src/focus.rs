//! Tile-click dispatch onto the background executor.
//!
//! The osascript adapters can take ~100ms in the slow path (cold AppleScript
//! permission prompt). We never want that on the foreground/render thread,
//! so the public entry point spawns onto `BackgroundExecutor` and returns.

use corral_adapters::{FocusContext, ParentApp, default_adapters, dispatch, resolve_parent_app};
use corral_core::proc::ProcessId;
use corral_core::trace::{self, TraceEvent};
use gpui::App;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

pub fn focus_agent_async(
    cx: &App,
    cli_pid: ProcessId,
    cli_tty: Option<PathBuf>,
    cwd: Option<PathBuf>,
) {
    let request_id = next_request_id();
    cx.background_executor()
        .spawn(async move {
            focus_agent(request_id, cli_pid, cli_tty, cwd);
        })
        .detach();
}

/// Monotonic request id paired with each `FocusRequested` /
/// `FocusDispatched` event so the harness can correlate them.
fn next_request_id() -> u64 {
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
    cli_tty: Option<PathBuf>,
    cwd: Option<PathBuf>,
) {
    focus_agent(request_id, cli_pid, cli_tty, cwd);
}

fn focus_agent(
    request_id: u64,
    cli_pid: ProcessId,
    cli_tty: Option<PathBuf>,
    cwd: Option<PathBuf>,
) {
    let parent_app =
        resolve_parent_app(cli_pid).map(|(pid, bundle_id)| ParentApp { pid, bundle_id });
    let ctx = FocusContext {
        cli_pid,
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
    if let Err(e) = dispatch(&adapters, ctx) {
        tracing::warn!(error = %e, pid = %cli_pid, "focus dispatch failed");
    }
}
