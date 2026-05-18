//! Resolve the GUI parent application of an agent's CLI process.
//!
//! Walks the process tree upward via libproc and asks
//! `NSRunningApplication.runningApplicationWithProcessIdentifier` for each
//! ancestor. The first ancestor that yields a non-empty bundle identifier
//! is the hosting application (typically the terminal emulator).

use corral_core::proc::{ProcessId, walk_parents};
use objc2_app_kit::NSRunningApplication;

const MAX_PARENT_HOPS: u32 = 12;

pub fn resolve_parent_app(cli_pid: ProcessId) -> Option<(ProcessId, String)> {
    walk_parents(cli_pid, MAX_PARENT_HOPS, |p| {
        let app = NSRunningApplication::runningApplicationWithProcessIdentifier(p.0)?;
        let bid = app.bundleIdentifier()?.to_string();
        if bid.is_empty() { None } else { Some((p, bid)) }
    })
}
