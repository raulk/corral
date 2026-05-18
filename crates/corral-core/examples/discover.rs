//! Live smoke check for `agent::discover()` with per-candidate diagnostics.
//!
//! Run with:
//!     cargo run -p corral-core --example discover

use corral_core::prelude::*;

fn main() {
    println!("=== raw candidates ===");
    let raw = list_processes_matching(&["claude", "claude.exe", "codex"]).expect("list");
    for (pid, argv) in &raw {
        let exec = argv.first().cloned().unwrap_or_default();
        let argv0 = argv.get(1).cloned().unwrap_or_default();
        let tty = process_tty(*pid);
        let cwd = process_cwd(*pid);
        let env = process_args_env(*pid).ok().map(|e| {
            let claude_sid = e.env.get("CLAUDE_CODE_SESSION_ID").cloned();
            (claude_sid,)
        });
        let claude_trans = claude_transcript_for(*pid);
        let fd_trans = process_open_session_transcript(*pid);
        println!(
            "pid={pid} exec={exec} argv0={argv0} tty={tty} cwd={cwd} claude_sid_env={sid:?} claude_trans={ct:?} fd_trans={ft:?}",
            tty = tty
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "-".into()),
            cwd = cwd
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "-".into()),
            sid = env.map(|(s,)| s).unwrap_or(None),
            ct = claude_trans,
            ft = fd_trans,
        );
    }
    println!();
    println!("=== discover() result ===");
    let agents = discover().expect("discover");
    println!("found {} agent(s)", agents.len());
    for a in &agents {
        println!(
            "  pid={pid} tool={tool:?} session={sid} cwd={cwd} tty={tty} transcript={tp}",
            pid = a.pid,
            tool = a.tool,
            sid = a.session_id,
            cwd = a
                .cwd
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "-".into()),
            tty = a
                .tty
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "-".into()),
            tp = a.transcript_path.display(),
        );
    }
}
