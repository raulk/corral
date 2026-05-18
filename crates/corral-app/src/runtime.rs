//! Bootstrap for the background plumbing that produces `RegistryEvent`s.
//!
//! Holds the join handles + FSEvents debouncer so they live for the lifetime
//! of the app; dropping `Runtime` stops the driver threads.

use corral_core::fsevents;
use corral_core::kqueue::{self, KqueueCommand};
use corral_core::proc::{self, ProcessKey};
use corral_core::registry::{Registry, RegistryEvent, SystemEvent};
use crossbeam_channel::{Receiver, Sender, unbounded};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const DISCOVERY_INTERVAL: Duration = Duration::from_secs(2);
const IDLE_RECHECK_INTERVAL: Duration = Duration::from_secs(15);
const FSEVENTS_DEBOUNCE: Duration = Duration::from_millis(50);

pub struct Runtime {
    /// Outbound registry events. Use `take_events()` to consume the
    /// channel exactly once; cloning leaves an extra read endpoint
    /// alive forever and prevents the registry's send loop from
    /// observing disconnect on shutdown.
    events: Option<Receiver<RegistryEvent>>,
    /// Set to true to signal every driver thread to stop. Threads
    /// check this between sleeps / sends.
    shutdown: Arc<AtomicBool>,
    /// Inbound channel to the registry — discovery ticks, transcript
    /// notifications, control-socket snapshot requests, etc. Exposed
    /// through `Runtime::sys_tx` so the control socket can submit
    /// requests after bootstrap.
    sys_tx: Sender<SystemEvent>,
    /// kqueue command channel — dropping it triggers the kqueue
    /// bridge + loop shutdown via the wake path.
    kq_cmd_tx: Option<Sender<KqueueCommand>>,
    /// Held to keep the FSEvents debouncer alive; dropping it ends
    /// the FSEvents source.
    fsevents: Option<notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>>,
    /// All driver thread join handles. `Drop` signals shutdown then
    /// joins each one so the OS notifies the registry of a clean
    /// teardown before the process exits.
    threads: Vec<JoinHandle<()>>,
}

impl Runtime {
    /// Clone of the inbound system-event channel. The control socket uses
    /// this to push `DiscoveryTick` and `Snapshot` requests into the
    /// registry; tests use it to seed events directly.
    pub fn sys_tx(&self) -> Sender<SystemEvent> {
        self.sys_tx.clone()
    }
}

impl Runtime {
    /// Consume the outbound registry-event receiver. Panics if called
    /// twice; the call site is the strip's event pump, which only
    /// happens once at app startup.
    pub fn take_events(&mut self) -> Receiver<RegistryEvent> {
        self.events
            .take()
            .expect("Runtime::take_events called more than once")
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // Phase 1: flip the flag so the tickers + pumps observe shutdown
        // on their next iteration.
        self.shutdown.store(true, Ordering::SeqCst);
        // Phase 2: tear down the source channels so blocked recv() and
        // kqueue bridges all unblock.
        self.kq_cmd_tx.take();
        self.fsevents.take();
        // Phase 3: join everything we spawned. Best-effort: a thread
        // that's wedged shouldn't hold the process up forever.
        let pending = std::mem::take(&mut self.threads);
        let (done_tx, done_rx) = crossbeam_channel::bounded::<()>(0);
        let join_thread = thread::spawn(move || {
            for t in pending {
                let _ = t.join();
            }
            let _ = done_tx.send(());
        });
        if done_rx.recv_timeout(Duration::from_secs(2)).is_err() {
            tracing::warn!("runtime: driver threads did not exit within 2s");
        }
        drop(join_thread);
    }
}

pub fn bootstrap() -> Runtime {
    let (sys_tx, sys_rx) = unbounded::<SystemEvent>();
    let (out_tx, out_rx) = unbounded::<RegistryEvent>();
    let shutdown = Arc::new(AtomicBool::new(false));

    // kqueue thread emits ProcessExited directly into the SystemEvent stream
    // via the bridge we wire below.
    let (kq_exit_tx, kq_exit_rx) = unbounded::<ProcessKey>();
    let (kq_cmd_tx, kq_threads) = kqueue::spawn(kq_exit_tx);
    let kq_pump = spawn_kqueue_pump(kq_exit_rx, sys_tx.clone());

    let discovery = spawn_discovery_tick(sys_tx.clone(), shutdown.clone());
    let idle = spawn_idle_recheck_tick(sys_tx.clone(), shutdown.clone());
    let (debouncer, fs_pump) = spawn_fsevents(sys_tx.clone());
    let reg = spawn_registry(sys_rx, out_tx, kq_cmd_tx.clone());

    let mut threads = Vec::with_capacity(8);
    threads.push(discovery);
    threads.push(idle);
    threads.push(fs_pump);
    threads.push(kq_pump);
    threads.extend(kq_threads);
    threads.push(reg);

    Runtime {
        events: Some(out_rx),
        shutdown,
        sys_tx,
        kq_cmd_tx: Some(kq_cmd_tx),
        fsevents: Some(debouncer),
        threads,
    }
}

fn spawn_kqueue_pump(rx: Receiver<ProcessKey>, tx: Sender<SystemEvent>) -> JoinHandle<()> {
    thread::Builder::new()
        .name("corral-kq-pump".into())
        .spawn(move || {
            while let Ok(key) = rx.recv() {
                if tx.send(SystemEvent::ProcessExited(key)).is_err() {
                    break;
                }
            }
        })
        .expect("spawn kqueue pump thread")
}

/// Spawn a thread that emits `event_kind` once per `interval` until
/// the receiver is dropped or `shutdown` flips to true. When
/// `kick_immediately`, the first event fires without waiting a full
/// interval. Sleep is broken into 100ms chunks so shutdown latency
/// stays small even on long intervals.
fn spawn_ticker<F>(
    name: &'static str,
    interval: Duration,
    kick_immediately: bool,
    tx: Sender<SystemEvent>,
    shutdown: Arc<AtomicBool>,
    make_event: F,
) -> JoinHandle<()>
where
    F: Fn() -> SystemEvent + Send + 'static,
{
    thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            if kick_immediately && tx.send(make_event()).is_err() {
                return;
            }
            let chunk = Duration::from_millis(100);
            while !shutdown.load(Ordering::Relaxed) {
                // Slice the sleep so a long IDLE_RECHECK_INTERVAL still
                // observes shutdown quickly.
                let mut remaining = interval;
                while remaining > Duration::ZERO {
                    if shutdown.load(Ordering::Relaxed) {
                        return;
                    }
                    let step = remaining.min(chunk);
                    thread::sleep(step);
                    remaining -= step;
                }
                if shutdown.load(Ordering::Relaxed) || tx.send(make_event()).is_err() {
                    return;
                }
            }
        })
        .unwrap_or_else(|e| panic!("spawn {name} thread: {e}"))
}

fn spawn_discovery_tick(tx: Sender<SystemEvent>, shutdown: Arc<AtomicBool>) -> JoinHandle<()> {
    spawn_ticker(
        "corral-discovery",
        DISCOVERY_INTERVAL,
        true,
        tx,
        shutdown,
        || SystemEvent::DiscoveryTick,
    )
}

fn spawn_idle_recheck_tick(tx: Sender<SystemEvent>, shutdown: Arc<AtomicBool>) -> JoinHandle<()> {
    spawn_ticker(
        "corral-idle",
        IDLE_RECHECK_INTERVAL,
        false,
        tx,
        shutdown,
        || SystemEvent::IdleRecheckTick,
    )
}

fn spawn_fsevents(
    tx: Sender<SystemEvent>,
) -> (
    notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>,
    JoinHandle<()>,
) {
    let (fs_tx, fs_rx) = unbounded::<PathBuf>();
    let roots: Vec<PathBuf> = [
        proc::transcripts_root_claude(),
        proc::transcripts_root_codex(),
    ]
    .into_iter()
    .flatten()
    .collect();
    let debouncer = fsevents::spawn_transcript_watcher_into(&roots, FSEVENTS_DEBOUNCE, fs_tx)
        .expect("spawn fsevents corral");

    let pump = thread::Builder::new()
        .name("corral-fsevents-pump".into())
        .spawn(move || {
            while let Ok(path) = fs_rx.recv() {
                if tx.send(SystemEvent::TranscriptChanged(path)).is_err() {
                    break;
                }
            }
        })
        .expect("spawn fsevents pump");

    (debouncer, pump)
}

fn spawn_registry(
    sys_rx: Receiver<SystemEvent>,
    out_tx: Sender<RegistryEvent>,
    kq_cmd_tx: Sender<KqueueCommand>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("corral-registry".into())
        .spawn(move || {
            let mut reg = Registry::new(out_tx, kq_cmd_tx);
            reg.run(sys_rx);
        })
        .expect("spawn registry thread")
}
