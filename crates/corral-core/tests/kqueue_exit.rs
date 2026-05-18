//! Integration test for the kqueue process-exit corral.
//!
//! Spawns a short-lived child via `std::process::Command`, registers its pid
//! with the kqueue thread, kills it, and asserts we get the exit event back
//! within 100 ms (well below the 2s discovery-tick fallback).

use corral_core::kqueue::{KqueueCommand, spawn};
use corral_core::proc::{ProcessId, ProcessKey};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn test_key(pid: ProcessId) -> ProcessKey {
    ProcessKey::with_session(pid, uuid::Uuid::nil())
}

#[test]
fn kqueue_emits_exit_for_killed_child() {
    let (on_exit_tx, on_exit_rx) = crossbeam_channel::unbounded::<ProcessKey>();
    let (cmd_tx, _threads) = spawn(on_exit_tx);

    // Long-running child we control. `sleep 60` is fine; we kill it ourselves.
    let mut child = Command::new("sleep")
        .arg("60")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sleep");

    let pid = ProcessId(child.id() as i32);
    let key = test_key(pid);
    cmd_tx.send(KqueueCommand::Watch(key)).expect("send watch");

    // Give the bridge + kqueue thread a moment to register the watch.
    std::thread::sleep(Duration::from_millis(50));

    let kill_at = Instant::now();
    child.kill().expect("kill child");

    let observed = on_exit_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("kqueue did not emit ProcessExited within 2s");
    let latency = kill_at.elapsed();

    assert_eq!(observed, key, "got exit event for wrong process key");
    assert!(
        latency < Duration::from_millis(250),
        "exit event arrived too slowly: {latency:?}",
    );

    // Reap so we don't leak a defunct child.
    let _ = child.wait();
}

#[test]
fn kqueue_threads_exit_when_cmd_tx_dropped() {
    use std::sync::mpsc;

    let (on_exit_tx, _on_exit_rx) = crossbeam_channel::unbounded::<ProcessKey>();
    let (cmd_tx, threads) = spawn(on_exit_tx);

    // Drop the command sender so the bridge's recv returns Err and the
    // shutdown wake propagates to the loop.
    drop(cmd_tx);

    // Join both threads on a separate worker so a hang shows up as a
    // recv_timeout rather than locking up the test process forever.
    let (done_tx, done_rx) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        for t in threads {
            let _ = t.join();
        }
        let _ = done_tx.send(());
    });

    done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("kqueue threads did not exit within 2s of cmd_tx drop");
}
