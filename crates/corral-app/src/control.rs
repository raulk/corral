//! Unix-socket control server.
//!
//! Lets an external process (today: the integration harness) drive
//! the live corral without spawning a second process that would
//! redo discovery from scratch. Newline-delimited JSON request/response.
//!
//! The protocol is a thin wrapper over the registry's existing inbound
//! channel: `discover-now` enqueues a `SystemEvent::DiscoveryTick`,
//! `snapshot` round-trips a `SystemEvent::Snapshot`, `focus` resolves
//! the agent by pid from a snapshot and dispatches the same code path
//! the UI tile uses. `shutdown` calls `std::process::exit(0)` — there
//! is no soft-shutdown path for GPUI from a background thread in 0.2.2.

use crossbeam_channel::{Sender, bounded};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use uuid::Uuid;

use corral_core::agent::Agent;
use corral_core::proc::ProcessId;
use corral_core::registry::SystemEvent;
use corral_core::status::AgentState;
use corral_core::trace::{AgentKind, BindingSource};

const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
enum Request {
    Focus { pid: i32, request_id: u64 },
    DiscoverNow,
    Snapshot,
    Shutdown,
}

/// One response shape covers all ops: `ok` plus optional payload.
/// Snapshot replies fill `agents`; failures fill `error`.
#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<AgentSnapshot>>,
}

impl Response {
    fn ok() -> Self {
        Self {
            ok: true,
            error: None,
            agents: None,
        }
    }

    fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(msg.into()),
            agents: None,
        }
    }

    fn snapshot(agents: Vec<AgentSnapshot>) -> Self {
        Self {
            ok: true,
            error: None,
            agents: Some(agents),
        }
    }
}

/// Per-agent record returned in a snapshot response. Surfaces the
/// fields the harness asserts against: identity (pid/session id),
/// binding provenance, lifecycle state, plus the loose metadata the
/// transcript parser fills in (model id, git branch, current/last
/// action, context token counts).
#[derive(Debug, Serialize, Deserialize)]
pub struct AgentSnapshot {
    pub pid: i32,
    pub agent: AgentKind,
    pub state: AgentState,
    pub session_id: Uuid,
    pub transcript: PathBuf,
    pub cwd: Option<PathBuf>,
    pub tty: Option<PathBuf>,
    pub binding_source: BindingSource,
    pub model: Option<String>,
    pub git_branch: Option<String>,
    pub session_title: Option<String>,
    pub current_action: Option<String>,
    pub last_action: Option<String>,
    pub context_tokens: Option<u32>,
    pub context_max: Option<u32>,
}

fn snapshot_of(a: &Agent) -> AgentSnapshot {
    AgentSnapshot {
        pid: a.pid.0,
        agent: a.tool.as_trace_kind(),
        state: a.state,
        session_id: a.session_id,
        transcript: a.transcript_path.clone(),
        cwd: a.cwd.clone(),
        tty: a.tty.clone(),
        binding_source: a.binding_source,
        model: a.model.clone(),
        git_branch: a.git_branch.clone(),
        session_title: a.session_title.clone(),
        current_action: a.current_action.clone(),
        last_action: a.last_action.clone(),
        context_tokens: a.context_tokens,
        context_max: a.context_max,
    }
}

/// Handle to a running control server. Dropping the handle stops the
/// listener (the accept thread observes the shutdown flag and a sentinel
/// connect that unblocks `accept`) and removes the socket file.
pub struct ControlServer {
    socket_path: PathBuf,
    shutdown: Arc<AtomicBool>,
    listener_thread: Option<JoinHandle<()>>,
}

impl ControlServer {
    // Only consumed by tests today; main.rs holds the path elsewhere.
    #[allow(dead_code)]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Sentinel connect: `UnixListener::accept` is blocking and there's
        // no portable poll-with-cancel. Connecting unblocks it; the loop
        // then sees `shutdown` set and returns.
        let _ = UnixStream::connect(&self.socket_path);
        if let Some(t) = self.listener_thread.take() {
            let _ = t.join();
        }
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// Default socket path. `XDG_RUNTIME_DIR/corral/control.sock` if the
/// var is set (rare on macOS); otherwise `$TMPDIR/corral.control.sock`.
/// Override with `CORRAL_CONTROL_SOCKET=/path/to/sock` (the harness
/// uses this to put the socket in its own tempdir).
pub fn default_socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("CORRAL_CONTROL_SOCKET") {
        return PathBuf::from(p);
    }
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(xdg).join("corral").join("control.sock");
    }
    let tmp = std::env::var("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"));
    tmp.join("corral.control.sock")
}

/// Bind the socket and start the accept loop. The caller is expected to
/// keep the returned `ControlServer` alive for the lifetime of the
/// process; dropping it tears the server down.
pub fn spawn(socket_path: PathBuf, sys_tx: Sender<SystemEvent>) -> std::io::Result<ControlServer> {
    cleanup_stale(&socket_path)?;
    if let Some(parent) = socket_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(&socket_path)?;
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_for_loop = shutdown.clone();
    let socket_path_for_loop = socket_path.clone();
    let listener_thread = thread::Builder::new()
        .name("corral-control".into())
        .spawn(move || {
            accept_loop(listener, sys_tx, shutdown_for_loop, socket_path_for_loop);
        })
        .expect("spawn control thread");
    Ok(ControlServer {
        socket_path,
        shutdown,
        listener_thread: Some(listener_thread),
    })
}

/// Remove a stale socket left behind by a prior crash. Connect-probe
/// first: if something on the other end answers, refuse to start (a
/// second corral is already running on this path).
fn cleanup_stale(path: &Path) -> std::io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    match UnixStream::connect(path) {
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            format!("control socket {path:?} is in use by another corral"),
        )),
        Err(_) => std::fs::remove_file(path),
    }
}

fn accept_loop(
    listener: UnixListener,
    sys_tx: Sender<SystemEvent>,
    shutdown: Arc<AtomicBool>,
    socket_path: PathBuf,
) {
    for stream in listener.incoming() {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "control: accept failed");
                continue;
            }
        };
        let sys_tx = sys_tx.clone();
        let sp = socket_path.clone();
        thread::Builder::new()
            .name("corral-control-conn".into())
            .spawn(move || handle_connection(stream, sys_tx, sp))
            .expect("spawn control connection thread");
    }
}

fn handle_connection(stream: UnixStream, sys_tx: Sender<SystemEvent>, socket_path: PathBuf) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone unix stream"));
    let mut writer = stream;
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return,
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, "control: read failed");
                return;
            }
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (response, post) = match serde_json::from_str::<Request>(trimmed) {
            Ok(req) => handle_request(req, &sys_tx),
            Err(e) => (
                Response::err(format!("malformed request: {e}")),
                PostAction::None,
            ),
        };
        if !write_response(&mut writer, &response) {
            return;
        }
        if matches!(post, PostAction::Shutdown) {
            let _ = std::fs::remove_file(&socket_path);
            // GPUI 0.2.2 lacks a cross-thread quit hook; per-line flushing
            // of the trace sink keeps test assertions valid across exit.
            std::process::exit(0);
        }
    }
}

fn write_response(stream: &mut UnixStream, response: &Response) -> bool {
    let payload = match serde_json::to_string(response) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "control: response serialize failed");
            return false;
        }
    };
    if stream.write_all(payload.as_bytes()).is_err() {
        return false;
    }
    if stream.write_all(b"\n").is_err() {
        return false;
    }
    stream.flush().is_ok()
}

enum PostAction {
    None,
    Shutdown,
}

fn handle_request(req: Request, sys_tx: &Sender<SystemEvent>) -> (Response, PostAction) {
    match req {
        Request::Focus { pid, request_id } => match handle_focus(sys_tx, pid, request_id) {
            Ok(()) => (Response::ok(), PostAction::None),
            Err(e) => (Response::err(e), PostAction::None),
        },
        Request::DiscoverNow => {
            if sys_tx.send(SystemEvent::DiscoveryTick).is_ok() {
                (Response::ok(), PostAction::None)
            } else {
                (Response::err("registry channel closed"), PostAction::None)
            }
        }
        Request::Snapshot => match take_snapshot(sys_tx) {
            Ok(agents) => (Response::snapshot(agents), PostAction::None),
            Err(e) => (Response::err(e), PostAction::None),
        },
        Request::Shutdown => (Response::ok(), PostAction::Shutdown),
    }
}

fn take_snapshot(sys_tx: &Sender<SystemEvent>) -> Result<Vec<AgentSnapshot>, String> {
    let (reply_tx, reply_rx) = bounded(1);
    sys_tx
        .send(SystemEvent::Snapshot { reply: reply_tx })
        .map_err(|_| "registry channel closed".to_string())?;
    let agents = reply_rx
        .recv_timeout(SNAPSHOT_TIMEOUT)
        .map_err(|_| "snapshot timed out".to_string())?;
    Ok(agents.iter().map(snapshot_of).collect())
}

fn handle_focus(sys_tx: &Sender<SystemEvent>, pid: i32, request_id: u64) -> Result<(), String> {
    let (reply_tx, reply_rx) = bounded(1);
    sys_tx
        .send(SystemEvent::Snapshot { reply: reply_tx })
        .map_err(|_| "registry channel closed".to_string())?;
    let agents = reply_rx
        .recv_timeout(SNAPSHOT_TIMEOUT)
        .map_err(|_| "snapshot timed out".to_string())?;
    let agent = agents
        .iter()
        .find(|a| a.pid == ProcessId(pid))
        .ok_or_else(|| format!("no agent for pid {pid}"))?;
    crate::focus::focus_for_request(request_id, agent.pid, agent.tty.clone(), agent.cwd.clone());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::unbounded;
    use tempfile::tempdir;

    fn send_request(stream: &mut UnixStream, req: &str) -> Response {
        stream.write_all(req.as_bytes()).unwrap();
        stream.write_all(b"\n").unwrap();
        stream.flush().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).expect("response is valid json")
    }

    #[test]
    fn discover_now_pushes_tick() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ctl.sock");
        let (sys_tx, sys_rx) = unbounded::<SystemEvent>();
        let server = spawn(path.clone(), sys_tx).expect("spawn server");

        let mut stream = UnixStream::connect(server.socket_path()).unwrap();
        let resp = send_request(&mut stream, r#"{"op":"discover-now"}"#);
        assert!(resp.ok);
        assert!(resp.error.is_none());

        match sys_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            SystemEvent::DiscoveryTick => {}
            other => panic!("expected DiscoveryTick, got {other:?}"),
        }
    }

    #[test]
    fn snapshot_round_trips_empty_registry() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ctl.sock");
        let (sys_tx, sys_rx) = unbounded::<SystemEvent>();

        // Stub registry: answers a single Snapshot request and quits.
        let registry = thread::spawn(move || {
            let ev = sys_rx.recv().unwrap();
            if let SystemEvent::Snapshot { reply } = ev {
                reply.send(Vec::new()).unwrap();
            }
        });

        let server = spawn(path.clone(), sys_tx).expect("spawn server");
        let mut stream = UnixStream::connect(server.socket_path()).unwrap();
        let resp = send_request(&mut stream, r#"{"op":"snapshot"}"#);
        assert!(resp.ok);
        assert_eq!(resp.agents.as_ref().map(Vec::len), Some(0));
        registry.join().unwrap();
    }

    #[test]
    fn focus_returns_error_when_pid_not_known() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ctl.sock");
        let (sys_tx, sys_rx) = unbounded::<SystemEvent>();

        let registry = thread::spawn(move || {
            let ev = sys_rx.recv().unwrap();
            if let SystemEvent::Snapshot { reply } = ev {
                reply.send(Vec::new()).unwrap();
            }
        });

        let server = spawn(path.clone(), sys_tx).expect("spawn server");
        let mut stream = UnixStream::connect(server.socket_path()).unwrap();
        let resp = send_request(&mut stream, r#"{"op":"focus","pid":99999,"request_id":1}"#);
        assert!(!resp.ok);
        assert!(
            resp.error
                .as_deref()
                .unwrap_or("")
                .contains("no agent for pid")
        );
        registry.join().unwrap();
    }

    #[test]
    fn malformed_request_returns_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ctl.sock");
        let (sys_tx, _sys_rx) = unbounded::<SystemEvent>();
        let server = spawn(path.clone(), sys_tx).expect("spawn server");
        let mut stream = UnixStream::connect(server.socket_path()).unwrap();
        let resp = send_request(&mut stream, "not even json");
        assert!(!resp.ok);
        assert!(
            resp.error
                .as_deref()
                .unwrap_or("")
                .starts_with("malformed request:"),
        );
    }

    #[test]
    fn stale_socket_is_unlinked_on_boot() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ctl.sock");
        // Plant a "stale" socket file by writing a regular file at the path —
        // simulates a previous crash that left a leftover. Connect-probe will
        // fail (regular files don't speak the protocol), so spawn unlinks it.
        std::fs::write(&path, b"leftover").unwrap();
        let (sys_tx, _sys_rx) = unbounded::<SystemEvent>();
        let server = spawn(path.clone(), sys_tx).expect("must unlink stale file and bind");
        let mut stream = UnixStream::connect(server.socket_path()).unwrap();
        // Confirm the socket actually accepts a request now.
        stream.write_all(b"{\"op\":\"discover-now\"}\n").unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut buf = String::new();
        reader.read_line(&mut buf).unwrap();
        let resp: Response = serde_json::from_str(&buf).unwrap();
        assert!(resp.ok);
    }

    #[test]
    fn drop_removes_socket_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ctl.sock");
        let (sys_tx, _sys_rx) = unbounded::<SystemEvent>();
        {
            let _server = spawn(path.clone(), sys_tx).expect("spawn");
            assert!(path.exists(), "socket file must exist while server is up");
        }
        assert!(!path.exists(), "drop must remove the socket file");
    }

    #[test]
    fn socket_permissions_are_user_only() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ctl.sock");
        let (sys_tx, _sys_rx) = unbounded::<SystemEvent>();
        let server = spawn(path.clone(), sys_tx).expect("spawn");
        let meta = std::fs::metadata(server.socket_path()).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "socket should be user-read/write only");
    }
}
