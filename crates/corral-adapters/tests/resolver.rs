//! Integration check for the parent-app resolver.
//!
//! Picks a known live Claude pid (any TTY-attached Claude on the dev
//! machine) and confirms `resolve_parent_app` walks up to a real bundle
//! identifier. Skipped if no Claude is running so the test suite stays
//! deterministic on machines without Claude installed.

use corral_adapters::resolve_parent_app;
use corral_core::agent::discover;

#[test]
fn resolves_parent_for_first_claude_agent() {
    let agents = match discover() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("skipping: discover failed: {e}");
            return;
        }
    };
    // Find any TTY-attached agent whose ancestry resolves to a real
    // bundle. Headless agents (Codex app-server in particular) descend
    // from plain processes (e.g. zed-helper), so their parent chain
    // legitimately resolves to None — those agents aren't useful for
    // testing the resolver's happy path.
    let resolved = agents
        .iter()
        .filter(|a| a.tty.is_some())
        .find_map(|a| resolve_parent_app(a.pid).map(|r| (a.pid, r)));
    let Some((agent_pid, (parent_pid, bundle_id))) = resolved else {
        eprintln!("skipping: no TTY-attached agent has a discoverable GUI parent");
        return;
    };
    assert!(!bundle_id.is_empty(), "bundle id must not be empty");
    assert_ne!(
        parent_pid.0, agent_pid.0,
        "parent pid must differ from agent pid"
    );
    eprintln!(
        "resolved: agent_pid={} parent_pid={} bundle_id={bundle_id}",
        agent_pid, parent_pid
    );
}
