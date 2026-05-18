//! kqueue-backed process-exit corral.
//!
//! Maintains an `EVFILT_PROC | NOTE_EXIT | EV_ONESHOT` registration per
//! tracked PID. When a watched process exits, the kernel delivers an event
//! immediately (no polling), which we forward to the registry as
//! `SystemEvent::ProcessExited(key)`. New PIDs are added/removed dynamically
//! via a crossbeam channel; the loop wakes via `EVFILT_USER | NOTE_TRIGGER`
//! so registration latency is essentially zero.

use crate::proc::{ProcessId, ProcessKey};
use crossbeam_channel::{Receiver, Sender, TryRecvError, unbounded};
use nix::sys::event::{EventFilter, EventFlag, FilterFlag, KEvent, Kqueue};
use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd};
use std::thread::{self, JoinHandle};

#[derive(Debug, Clone, Copy)]
pub enum KqueueCommand {
    Watch(ProcessKey),
    Unwatch(ProcessKey),
}

// Arbitrary non-pid identifier for the wakeup filter. Any value that won't
// collide with a real pid works; we use 1 (init) since we never watch pid 1.
const WAKEUP_IDENT: libc::uintptr_t = 1;

/// Spawn the kqueue thread. Returns a sender that the registry uses to push
/// watch/unwatch commands, plus join handles for the loop thread and the
/// bridge thread. Dropping the sender ends both threads.
pub fn spawn(on_exit: Sender<ProcessKey>) -> (Sender<KqueueCommand>, [JoinHandle<()>; 2]) {
    // External commands land on cmd_tx (returned to caller). The bridge
    // forwards them to fwd_tx and fires the EVFILT_USER wake; the loop
    // drains fwd_rx via try_iter() after each kevent wake.
    let (cmd_tx, cmd_rx) = unbounded::<KqueueCommand>();
    let (fwd_tx, fwd_rx) = unbounded::<KqueueCommand>();

    let kq = Kqueue::new().expect("create kqueue fd");
    let kq_fd = kq.as_fd().as_raw_fd();

    // Register the wakeup filter once.
    let wake_add = KEvent::new(
        WAKEUP_IDENT,
        EventFilter::EVFILT_USER,
        EventFlag::EV_ADD | EventFlag::EV_CLEAR,
        FilterFlag::empty(),
        0,
        0,
    );
    kq.kevent(&[wake_add], &mut [], None)
        .expect("register EVFILT_USER");

    // Bridge: forwards commands onto fwd_tx and triggers the EVFILT_USER
    // wake so the main loop unblocks and applies them. When cmd_tx is
    // dropped, send a final wake so the loop unblocks and observes the
    // disconnect; otherwise it would sit in kevent() forever.
    let bridge = thread::Builder::new()
        .name("corral-kq-bridge".into())
        .spawn(move || {
            let result = loop {
                let cmd = match cmd_rx.recv() {
                    Ok(c) => c,
                    Err(_) => break Ok(()),
                };
                if fwd_tx.send(cmd).is_err() {
                    break Err("loop thread gone");
                }
                let trigger = libc::kevent {
                    ident: WAKEUP_IDENT,
                    filter: libc::EVFILT_USER,
                    flags: 0,
                    fflags: libc::NOTE_TRIGGER,
                    data: 0,
                    udata: std::ptr::null_mut(),
                };
                // Retry on EINTR so a signal can't drop our wake. Any
                // other error means the loop thread's kqueue is gone
                // — log and shut the bridge down so the next sender
                // sees the disconnect.
                let mut hard_err: Option<std::io::Error> = None;
                loop {
                    let rc = unsafe {
                        libc::kevent(
                            kq_fd,
                            &trigger,
                            1,
                            std::ptr::null_mut(),
                            0,
                            std::ptr::null(),
                        )
                    };
                    if rc != -1 {
                        break;
                    }
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    hard_err = Some(err);
                    break;
                }
                if let Some(err) = hard_err {
                    tracing::warn!(error = %err, "kqueue wake failed; shutting bridge down");
                    break Err("kevent wake failed");
                }
            };

            // Send one last wake so the loop unblocks and observes the
            // closed fwd channel; without this it would sit forever in
            // `kq.kevent` after the registry drops the command sender.
            let trigger = libc::kevent {
                ident: WAKEUP_IDENT,
                filter: libc::EVFILT_USER,
                flags: 0,
                fflags: libc::NOTE_TRIGGER,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            drop(fwd_tx);
            unsafe {
                libc::kevent(
                    kq_fd,
                    &trigger,
                    1,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null(),
                );
            }
            if let Err(reason) = result {
                tracing::debug!(reason, "kqueue bridge exiting");
            }
        })
        .expect("spawn kq bridge thread");

    let loop_thread = thread::Builder::new()
        .name("corral-kqueue".into())
        .spawn(move || run_loop(kq, fwd_rx, on_exit))
        .expect("spawn kqueue loop thread");

    (cmd_tx, [bridge, loop_thread])
}

fn run_loop(kq: Kqueue, pending: Receiver<KqueueCommand>, on_exit: Sender<ProcessKey>) {
    let mut events = vec![empty_kevent(); 64];
    let mut registrations = Registrations::default();
    loop {
        // Block until at least one event. With no PIDs registered (only the
        // EVFILT_USER), this blocks until the bridge thread triggers us.
        let n = match kq.kevent(&[], &mut events, None) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(error = %e, "kqueue kevent failed");
                continue;
            }
        };

        // Apply pending watch/unwatch commands. When the bridge drops
        // its sender (because the registry dropped cmd_tx), shut down
        // cleanly so callers can join us.
        loop {
            match pending.try_recv() {
                Ok(cmd) => {
                    if !apply_change(&kq, cmd, &on_exit, &mut registrations) {
                        return;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        // Emit ProcessExited for any NOTE_EXIT events.
        for ev in &events[..n] {
            let Ok(filter) = ev.filter() else { continue };
            if filter != EventFilter::EVFILT_PROC {
                continue;
            }
            if !ev.fflags().contains(FilterFlag::NOTE_EXIT) {
                continue;
            }
            let pid = ProcessId(ev.ident() as i32);
            let token = ev.udata();
            let Some(key) = registrations.remove_event(pid, token) else {
                continue;
            };
            if on_exit.send(key).is_err() {
                return; // registry hung up
            }
        }
    }
}

#[derive(Default)]
struct Registrations {
    by_pid: HashMap<ProcessId, Registration>,
    by_token: HashMap<libc::intptr_t, ProcessKey>,
    next_token: libc::intptr_t,
}

#[derive(Debug, Clone, Copy)]
struct Registration {
    key: ProcessKey,
    token: libc::intptr_t,
}

impl Registrations {
    fn add(&mut self, key: ProcessKey) -> Registration {
        self.next_token = self.next_token.saturating_add(1).max(1);
        let registration = Registration {
            key,
            token: self.next_token,
        };
        self.by_pid.insert(key.pid, registration);
        self.by_token.insert(registration.token, key);
        registration
    }

    fn remove_command(&mut self, key: ProcessKey) -> Option<Registration> {
        let registration = *self.by_pid.get(&key.pid)?;
        if registration.key != key {
            return None;
        }
        self.by_pid.remove(&key.pid);
        self.by_token.remove(&registration.token);
        Some(registration)
    }

    fn remove_event(&mut self, pid: ProcessId, token: libc::intptr_t) -> Option<ProcessKey> {
        let key = self.by_token.remove(&token)?;
        if key.pid == pid
            && let Some(registration) = self.by_pid.get(&pid)
            && registration.token == token
        {
            self.by_pid.remove(&pid);
        }
        Some(key)
    }
}

/// Apply a single kqueue change. Returns false if the on_exit channel
/// has hung up (callers should shut the loop down). ESRCH on a Watch
/// is mapped to a synthetic ProcessExited: a pid that vanished
/// between discovery and our NOTE_EXIT registration is functionally
/// the same as one that exited just after.
fn apply_change(
    kq: &Kqueue,
    cmd: KqueueCommand,
    on_exit: &Sender<ProcessKey>,
    registrations: &mut Registrations,
) -> bool {
    let registration = match cmd {
        KqueueCommand::Watch(key) => Some(registrations.add(key)),
        KqueueCommand::Unwatch(key) => registrations.remove_command(key),
    };
    let Some(registration) = registration else {
        return true;
    };
    let change = cmd_to_kevent(cmd, registration.token);
    if let Err(e) = kq.kevent(&[change], &mut [], None) {
        if matches!(cmd, KqueueCommand::Watch(_)) {
            registrations.remove_command(registration.key);
        }
        if let KqueueCommand::Watch(key) = cmd
            && e == nix::Error::ESRCH
        {
            if on_exit.send(key).is_err() {
                return false;
            }
        } else {
            tracing::debug!(error = %e, ?cmd, "kqueue change application failed");
        }
    }
    true
}

fn empty_kevent() -> KEvent {
    KEvent::new(
        0,
        EventFilter::EVFILT_USER,
        EventFlag::empty(),
        FilterFlag::empty(),
        0,
        0,
    )
}

fn cmd_to_kevent(cmd: KqueueCommand, token: libc::intptr_t) -> KEvent {
    match cmd {
        KqueueCommand::Watch(key) => KEvent::new(
            key.pid.0 as libc::uintptr_t,
            EventFilter::EVFILT_PROC,
            EventFlag::EV_ADD | EventFlag::EV_ONESHOT,
            FilterFlag::NOTE_EXIT,
            0,
            token,
        ),
        KqueueCommand::Unwatch(key) => KEvent::new(
            key.pid.0 as libc::uintptr_t,
            EventFilter::EVFILT_PROC,
            EventFlag::EV_DELETE,
            FilterFlag::empty(),
            0,
            token,
        ),
    }
}
