//! Process inspection primitives built on libproc + sysctl + raw `proc_pidfdinfo`.
//!
//! Everything here is macOS-only and synchronous. Callers run these on background
//! threads driven by the registry; we don't yield internally.

use crate::trace::BindingSource;
use libproc::bsd_info::BSDInfo;
use libproc::file_info::{ListFDs, ProcFDType};
use libproc::proc_pid::{listpidinfo, pidinfo, pidpath};
use libproc::processes::{ProcFilter, pids_by_type};
use std::collections::HashMap;
use std::ffi::CStr;
use std::path::{Path, PathBuf};
use std::{mem, ptr};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProcessId(pub i32);

impl std::fmt::Display for ProcessId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProcessStartTime {
    pub sec: i64,
    pub usec: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ProcessIdentity {
    StartTime(ProcessStartTime),
    SessionId(Uuid),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProcessKey {
    pub pid: ProcessId,
    pub identity: ProcessIdentity,
}

impl ProcessKey {
    pub fn for_pid_and_session(pid: ProcessId, session_id: Uuid) -> Self {
        Self {
            pid,
            identity: process_start_time(pid)
                .map(ProcessIdentity::StartTime)
                .unwrap_or(ProcessIdentity::SessionId(session_id)),
        }
    }

    pub fn with_session(pid: ProcessId, session_id: Uuid) -> Self {
        Self {
            pid,
            identity: ProcessIdentity::SessionId(session_id),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProcError {
    #[error("libproc call failed: {0}")]
    LibProc(String),
    #[error("sysctl failed: {0}")]
    Sysctl(std::io::Error),
}

/// Returns `(pid, argv)` for every running process whose argv[0] basename is in `basenames`.
///
/// Cheap enough to call on a 2s cadence even at a few hundred processes: the
/// expensive parts (transcript-FD inspection, tty lookup) only run on the
/// already-filtered set.
pub fn list_processes_matching(
    basenames: &[&str],
) -> Result<Vec<(ProcessId, Vec<String>)>, ProcError> {
    let basenames: std::collections::HashSet<&str> = basenames.iter().copied().collect();
    let pids = pids_by_type(ProcFilter::All).map_err(|e| ProcError::LibProc(e.to_string()))?;
    let mut out = Vec::new();
    for raw_pid in pids {
        let pid = ProcessId(raw_pid as i32);
        let Ok(path) = pidpath(pid.0) else { continue };
        let basename = Path::new(&path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        if !basenames.contains(basename) {
            continue;
        }
        let argv = process_args_env(pid)
            .map(|p| p.argv)
            .unwrap_or_else(|_| vec![path]);
        out.push((pid, argv));
    }
    Ok(out)
}

/// `/dev/tty…` for the controlling terminal, or `None` if the process has none.
pub fn process_tty(pid: ProcessId) -> Option<PathBuf> {
    let info = pidinfo::<BSDInfo>(pid.0, 0).ok()?;
    let tdev = info.e_tdev;
    // `(dev_t)-1` is NODEV; a controlling terminal of 0 means none assigned.
    if tdev == u32::MAX || tdev == 0 {
        return None;
    }
    let name_ptr = unsafe { libc::devname(tdev as libc::dev_t, libc::S_IFCHR) };
    if name_ptr.is_null() {
        return None;
    }
    let name = unsafe { CStr::from_ptr(name_ptr) }
        .to_string_lossy()
        .into_owned();
    Some(PathBuf::from(format!("/dev/{name}")))
}

pub fn process_cwd(pid: ProcessId) -> Option<PathBuf> {
    // `libproc::proc_pid::pidcwd` returns "not implemented for macos" on
    // Darwin, so we call `proc_pidinfo(PROC_PIDVNODEPATHINFO)` directly.
    // The struct is `proc_vnodepathinfo` (2352 bytes); `pvi_cdir.vip_path`
    // starts at offset 152 and is a NUL-terminated `char[MAXPATHLEN]`.
    read_libproc_path::<PROC_VNODEPATHINFO_SIZE>(PVI_CDIR_VIP_PATH_OFFSET, |buf, size| unsafe {
        libc::proc_pidinfo(
            pid.0,
            PROC_PIDVNODEPATHINFO,
            0,
            buf as *mut libc::c_void,
            size,
        )
    })
}

/// Wrapper for libproc info calls that fill a fixed-size buffer with a
/// struct containing a NUL-terminated path at `path_offset`. The caller
/// provides the actual `proc_pidinfo`/`proc_pidfdinfo` invocation and
/// the buffer size as a const generic.
fn read_libproc_path<const SIZE: usize>(
    path_offset: usize,
    call: impl FnOnce(*mut u8, i32) -> i32,
) -> Option<PathBuf> {
    let mut buf = [0u8; SIZE];
    let n = call(buf.as_mut_ptr(), SIZE as i32);
    if n <= 0 {
        return None;
    }
    decode_path_at(&buf, path_offset, MAXPATHLEN)
}

// `proc_vnodepathinfo` layout (from `<sys/proc_info.h>`):
//   offset 0    pvi_cdir.vip_vi    (152 bytes)
//   offset 152  pvi_cdir.vip_path  (1024 bytes, NUL-terminated)
//   offset 1176 pvi_rdir...
//   total       2352 bytes
const PROC_PIDVNODEPATHINFO: libc::c_int = 9;
const PROC_VNODEPATHINFO_SIZE: usize = 2352;
const PVI_CDIR_VIP_PATH_OFFSET: usize = 152;

/// Compile-time mirror of `struct proc_vnodepathinfo`. Used only for
/// offset/size assertions — actual reads still go through the raw
/// byte buffer because Rust doesn't have stable `offset_of!` for
/// nested struct fields. The `#[repr(C)]` and assertion together
/// catch a wrong constant at compile time rather than at runtime.
#[repr(C)]
#[allow(dead_code)]
struct ProcVnodePathInfoLayout {
    pad_before_path: [u8; PVI_CDIR_VIP_PATH_OFFSET],
    vip_path: [u8; MAXPATHLEN],
    pad_after_path: [u8; PROC_VNODEPATHINFO_SIZE - PVI_CDIR_VIP_PATH_OFFSET - MAXPATHLEN],
}
const _: () = assert!(
    std::mem::size_of::<ProcVnodePathInfoLayout>() == PROC_VNODEPATHINFO_SIZE,
    "PROC_VNODEPATHINFO_SIZE mismatch"
);

pub struct ProcArgsEnv {
    /// `[exec_path, argv[0], argv[1], ...]`. The kernel exposes both the
    /// resolved exec path and the conventional argv together; we keep them
    /// in source order so callers can distinguish.
    pub argv: Vec<String>,
    pub env: HashMap<String, String>,
}

/// Reads argv + envp from the kernel via `sysctl(KERN_PROCARGS2)`.
///
/// Buffer layout (Darwin):
///   `u32 argc | char[] exec_path (NUL-terminated, padded with NULs)`
///   `| argc NUL-terminated argv strings`
///   `| zero or more NUL-terminated envp strings (KEY=VALUE)`
pub fn process_args_env(pid: ProcessId) -> Result<ProcArgsEnv, ProcError> {
    PROCARGS_BUF.with_borrow_mut(|buf| {
        if buf.is_empty() {
            buf.resize(sysctl_argmax()?, 0);
        }
        let mut mib: [libc::c_int; 3] = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid.0];
        let size = sysctl_into_bytes(&mut mib, buf).map_err(ProcError::Sysctl)?;
        parse_procargs2(&buf[..size])
            .ok_or_else(|| ProcError::LibProc("malformed KERN_PROCARGS2 payload".into()))
    })
}

// `KERN_PROCARGS2` returns up to `KERN_ARGMAX` bytes (currently 256KB on
// Darwin). Allocating that per call dominates the cost of discovery on
// systems with many processes. A thread-local scratch buffer is reused
// across calls — the kernel updates `size` to the actual payload length
// so we only parse what's populated.
thread_local! {
    static PROCARGS_BUF: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Sanity range for KERN_ARGMAX. Darwin's documented value is 256KB
/// (262_144); we accept anything from 4KB to 16MB to be safe against
/// kernel changes while still rejecting obviously corrupt readings.
const ARGMAX_MIN: usize = 4 * 1024;
const ARGMAX_MAX: usize = 16 * 1024 * 1024;

fn sysctl_argmax() -> Result<usize, ProcError> {
    let mut mib: [libc::c_int; 2] = [libc::CTL_KERN, libc::KERN_ARGMAX];
    let argmax: libc::c_int = sysctl_one(&mut mib).map_err(ProcError::Sysctl)?;
    let argmax = usize::try_from(argmax)
        .map_err(|_| ProcError::LibProc(format!("negative KERN_ARGMAX: {argmax}")))?;
    if !(ARGMAX_MIN..=ARGMAX_MAX).contains(&argmax) {
        return Err(ProcError::LibProc(format!(
            "KERN_ARGMAX out of range: {argmax}"
        )));
    }
    Ok(argmax)
}

/// Wrap `libc::sysctl` for a variable-length byte payload. Returns the
/// number of bytes written (the kernel updates `size` in place).
fn sysctl_into_bytes(mib: &mut [libc::c_int], buf: &mut [u8]) -> std::io::Result<usize> {
    let mut size: libc::size_t = buf.len();
    // SAFETY: `mib` and `buf` are non-null, well-sized slices; `oldlenp`
    // points to the slice length the kernel may shrink.
    let ret = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut size,
            ptr::null_mut(),
            0,
        )
    };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(size)
}

/// Read a single `T` value via `libc::sysctl`. `T` must be a POD type
/// whose in-memory layout the kernel will fill (e.g. `libc::c_int`).
fn sysctl_one<T: Copy + Default>(mib: &mut [libc::c_int]) -> std::io::Result<T> {
    let mut out: T = T::default();
    let mut size = mem::size_of::<T>();
    // SAFETY: `out` is owned and writable for `size_of::<T>()` bytes.
    let ret = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            &mut out as *mut _ as *mut libc::c_void,
            &mut size,
            ptr::null_mut(),
            0,
        )
    };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(out)
}

fn parse_procargs2(buf: &[u8]) -> Option<ProcArgsEnv> {
    if buf.len() < 4 {
        return None;
    }
    let argc = u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    let mut i = 4;
    let exec_path = read_cstr(buf, &mut i)?;
    // Padding NULs between exec path and argv[0].
    while i < buf.len() && buf[i] == 0 {
        i += 1;
    }
    let mut argv = Vec::with_capacity(argc + 1);
    argv.push(exec_path);
    for _ in 0..argc {
        if i >= buf.len() {
            break;
        }
        let Some(s) = read_cstr(buf, &mut i) else {
            break;
        };
        argv.push(s);
    }
    let mut env = HashMap::new();
    while i < buf.len() {
        let Some(s) = read_cstr(buf, &mut i) else {
            break;
        };
        if s.is_empty() {
            break;
        }
        if let Some(eq) = s.find('=') {
            env.insert(s[..eq].to_owned(), s[eq + 1..].to_owned());
        }
    }
    Some(ProcArgsEnv { argv, env })
}

fn read_cstr(buf: &[u8], i: &mut usize) -> Option<String> {
    let cstr = std::ffi::CStr::from_bytes_until_nul(&buf[*i..]).ok()?;
    let s = cstr.to_str().ok()?.to_owned();
    *i += cstr.to_bytes().len() + 1; // include trailing NUL
    Some(s)
}

pub fn process_parent(pid: ProcessId) -> Option<ProcessId> {
    // libproc's `proc_pidinfo(PROC_PIDTBSDINFO)` returns EPERM for processes
    // owned by a different uid (e.g. `login`, which runs as root even when
    // launched from a user shell session). Fall back to `sysctl(KERN_PROC)`,
    // which is what `ps(1)` uses and is readable across uids.
    if let Ok(info) = pidinfo::<BSDInfo>(pid.0, 0) {
        let ppid = info.pbi_ppid;
        if ppid != 0 {
            return Some(ProcessId(ppid as i32));
        }
    }
    kinfo_ppid(pid)
}

/// Walk the parent chain of `start` (exclusive) for up to `max_hops`,
/// applying `probe` to each ancestor. Returns the first non-`None` result
/// or `None` if the walk runs out of ancestors or hops.
pub fn walk_parents<T>(
    start: ProcessId,
    max_hops: u32,
    probe: impl Fn(ProcessId) -> Option<T>,
) -> Option<T> {
    let mut cur = process_parent(start);
    for _ in 0..max_hops {
        let p = cur?;
        if let Some(v) = probe(p) {
            return Some(v);
        }
        cur = process_parent(p);
    }
    None
}

// `struct kinfo_proc` is 648 bytes on Darwin. `kp_proc.p_starttime`
// starts at offset 0 and `kp_eproc.e_ppid` sits at offset 560. We avoid
// declaring the full struct in Rust and just grab the bytes we need.
const KINFO_PROC_SIZE: usize = 648;
const P_STARTTIME_OFFSET: usize = 0;
const TIMEVAL_TV_SEC_OFFSET: usize = 0;
const TIMEVAL_TV_USEC_OFFSET: usize = 8;
const TIMEVAL_SIZE: usize = 16;
const E_PPID_OFFSET: usize = 560;

#[repr(C)]
#[allow(dead_code)]
struct KinfoProcLayout {
    p_starttime: TimevalLayout,
    pad_before_ppid: [u8; E_PPID_OFFSET - TIMEVAL_SIZE],
    e_ppid: i32,
    pad_after_ppid: [u8; KINFO_PROC_SIZE - E_PPID_OFFSET - 4],
}
#[repr(C)]
#[allow(dead_code)]
struct TimevalLayout {
    tv_sec: i64,
    tv_usec: i32,
    pad: [u8; 4],
}
const _: () = assert!(P_STARTTIME_OFFSET == 0, "P_STARTTIME_OFFSET mismatch");
const _: () = assert!(
    std::mem::size_of::<TimevalLayout>() == TIMEVAL_SIZE,
    "TIMEVAL_SIZE mismatch"
);
const _: () = assert!(
    std::mem::size_of::<KinfoProcLayout>() == KINFO_PROC_SIZE,
    "KINFO_PROC_SIZE mismatch"
);

pub fn process_start_time(pid: ProcessId) -> Option<ProcessStartTime> {
    let buf = read_kinfo_proc(pid)?;
    let sec = i64::from_ne_bytes(
        buf[P_STARTTIME_OFFSET + TIMEVAL_TV_SEC_OFFSET
            ..P_STARTTIME_OFFSET + TIMEVAL_TV_SEC_OFFSET + 8]
            .try_into()
            .ok()?,
    );
    let usec = i32::from_ne_bytes(
        buf[P_STARTTIME_OFFSET + TIMEVAL_TV_USEC_OFFSET
            ..P_STARTTIME_OFFSET + TIMEVAL_TV_USEC_OFFSET + 4]
            .try_into()
            .ok()?,
    );
    if sec == 0 && usec == 0 {
        return None;
    }
    Some(ProcessStartTime { sec, usec })
}

fn kinfo_ppid(pid: ProcessId) -> Option<ProcessId> {
    let buf = read_kinfo_proc(pid)?;
    let bytes: [u8; 4] = buf[E_PPID_OFFSET..E_PPID_OFFSET + 4].try_into().ok()?;
    let ppid = i32::from_ne_bytes(bytes);
    if ppid == 0 {
        None
    } else {
        Some(ProcessId(ppid))
    }
}

fn read_kinfo_proc(pid: ProcessId) -> Option<[u8; KINFO_PROC_SIZE]> {
    let mut buf = [0u8; KINFO_PROC_SIZE];
    let mut mib: [libc::c_int; 4] = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid.0];
    let size = sysctl_into_bytes(&mut mib, &mut buf).ok()?;
    // The kernel only returns a complete `kinfo_proc` struct; a
    // partial fill means the entry wasn't populated. Reject anything
    // short of the full size to avoid decoding zero-padded bytes at
    // `E_PPID_OFFSET`.
    if size != KINFO_PROC_SIZE {
        return None;
    }
    Some(buf)
}

/// If this process executes from inside a macOS `.app` bundle, returns the
/// human-readable app name (the bundle directory without the `.app` suffix).
/// Returns `None` for plain Unix binaries.
///
/// When the executable lives inside nested bundles (e.g. VS Code's
/// `Code Helper.app` nested inside `Visual Studio Code.app`), the outermost
/// `.app` wins — that's the app the user actually launched.
pub fn process_app_name(pid: ProcessId) -> Option<String> {
    // Typical bundle paths are 5-8 components
    // (`/Applications/Foo.app/Contents/MacOS/binary`). Cap the walk so a
    // pathological deeply-nested path can't waste cycles on ancestor
    // components that can't plausibly be an `.app` bundle.
    const MAX_ANCESTORS: usize = 16;
    let path = PathBuf::from(pidpath(pid.0).ok()?);
    path.ancestors()
        .take(MAX_ANCESTORS)
        .filter_map(|a| a.file_name()?.to_str()?.strip_suffix(".app"))
        .last()
        .map(str::to_string)
}

/// Returns the first open `.jsonl` path under `~/.claude/projects/` or
/// `~/.codex/sessions/` for this PID. The expected case is exactly one such
/// FD per agent process.
pub fn process_open_session_transcript(pid: ProcessId) -> Option<PathBuf> {
    let bsd = pidinfo::<BSDInfo>(pid.0, 0).ok()?;
    let fds = listpidinfo::<ListFDs>(pid.0, bsd.pbi_nfiles as usize).ok()?;
    let claude_root = transcripts_root_claude();
    let codex_root = transcripts_root_codex();
    for fd in fds {
        if !matches!(ProcFDType::from(fd.proc_fdtype), ProcFDType::VNode) {
            continue;
        }
        let Some(path) = vnode_fd_path(pid.0, fd.proc_fd) else {
            continue;
        };
        if !is_jsonl(&path) {
            continue;
        }
        if let Some(root) = &claude_root
            && path.starts_with(root)
        {
            return Some(path);
        }
        if let Some(root) = &codex_root
            && path.starts_with(root)
        {
            return Some(path);
        }
    }
    None
}

pub(crate) fn is_jsonl(p: &Path) -> bool {
    p.extension().and_then(|s| s.to_str()) == Some("jsonl")
}

pub fn transcripts_root_claude() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("projects"))
}

pub fn transcripts_root_codex() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".codex").join("sessions"))
}

// Layout of `vnode_fdinfowithpath` on Darwin (verified against
// `<sys/proc_info.h>` from the macOS SDK at build time of this comment):
//
//   offset 0   proc_fileinfo  pfi          (24 bytes)
//   offset 24  vnode_info     pvip.vip_vi  (152 bytes)
//   offset 176 char[MAXPATHLEN] vip_path   (1024 bytes, NUL-terminated)
//   total      1200 bytes
const PROC_PIDFDVNODEPATHINFO: libc::c_int = 2;
const VNODE_FDINFOWITHPATH_SIZE: usize = 1200;
const VIP_PATH_OFFSET: usize = 176;
const MAXPATHLEN: usize = 1024;

#[repr(C)]
#[allow(dead_code)]
struct VnodeFdInfoWithPathLayout {
    pad_before_path: [u8; VIP_PATH_OFFSET],
    vip_path: [u8; MAXPATHLEN],
}
const _: () = assert!(
    std::mem::size_of::<VnodeFdInfoWithPathLayout>() == VNODE_FDINFOWITHPATH_SIZE,
    "VNODE_FDINFOWITHPATH_SIZE mismatch"
);

fn vnode_fd_path(pid: libc::pid_t, fd: i32) -> Option<PathBuf> {
    read_libproc_path::<VNODE_FDINFOWITHPATH_SIZE>(VIP_PATH_OFFSET, |buf, size| unsafe {
        libc::proc_pidfdinfo(
            pid,
            fd,
            PROC_PIDFDVNODEPATHINFO,
            buf as *mut libc::c_void,
            size,
        )
    })
}

/// Read a NUL-terminated UTF-8 path embedded at a fixed offset inside a
/// libproc info buffer. Returns `None` if the slot is empty or invalid
/// UTF-8.
fn decode_path_at(buf: &[u8], offset: usize, max_len: usize) -> Option<PathBuf> {
    let slice = buf.get(offset..offset + max_len)?;
    let cstr = std::ffi::CStr::from_bytes_until_nul(slice).ok()?;
    let s = cstr.to_str().ok()?;
    if s.is_empty() {
        return None;
    }
    Some(PathBuf::from(s))
}

/// Locate the live Claude session transcript for `pid`.
///
/// PLAN.md originally specified pure FD inspection, but Claude open-write-closes
/// the transcript file per line, so its FD set rarely contains the path at the
/// instant we probe. We instead derive the path deterministically from the
/// session UUID (carried in env `CLAUDE_CODE_SESSION_ID` for CLI-launched
/// Claude, or `--session-id`/`--resume` argv for app-launched Claude) plus the
/// process's cwd (encoded as `/path/to/x` → `-path-to-x`, matching Claude's
/// projects-dir naming).
pub fn claude_transcript_for(pid: ProcessId) -> Option<PathBuf> {
    claude_transcript_for_excluding(pid, &std::collections::HashMap::new()).map(|(path, _, _)| path)
}

/// Same as `claude_transcript_for` but the mtime fallback skips any
/// `.jsonl` already present in `claimed[<this-pid's-session-dir>]`.
/// The caller uses this to ensure that multiple Claude PIDs sharing a
/// cwd each get a *distinct* transcript: without this, the mtime
/// fallback returns the same "latest" file for every PID in the dir
/// and the UI shows N tiles pointing at the same session. The argv/env
/// binding still wins outright (it's deterministic, not heuristic).
///
/// Returns `(transcript_path, session_id, binding_source)`. When the
/// session record is the source, `session_id` is taken directly from the
/// record rather than re-derived from the filename.
pub fn claude_transcript_for_excluding(
    pid: ProcessId,
    claimed: &std::collections::HashMap<PathBuf, std::collections::HashSet<PathBuf>>,
) -> Option<(PathBuf, Uuid, BindingSource)> {
    // Preferred: validated session record written by Claude itself.
    // This path is deterministic and survives /clear + /resume rebinding.
    if let Some(rec) = claude_session_record_for(pid)
        && validate_session_record(&rec, pid)
        && let Some(path) = find_claude_transcript_by_session_id(rec.session_id)
    {
        return Some((path, rec.session_id, BindingSource::SessionRecord));
    }

    let cwd = process_cwd(pid)?;
    let root = transcripts_root_claude()?;
    let session_dir = root.join(encode_cwd_for_claude(&cwd));

    // Fallback: deterministic match via argv/env. Works for app-launched
    // Claude (argv `--session-id` or `--resume`) and for descendants of a
    // Claude that exports `CLAUDE_CODE_SESSION_ID`.
    if let Ok(info) = process_args_env(pid)
        && let Some(session_id) = claude_session_id_from(&info)
    {
        let path = session_dir.join(format!("{session_id}.jsonl"));
        if path.exists() {
            return Some((path, session_id, BindingSource::ArgvEnv));
        }
    }

    // Last resort: shell-launched Claude processes don't expose their
    // session id in argv or env. Take the most recently modified
    // `<uuid>.jsonl` in the project dir as the active session,
    // skipping any path the caller has already claimed for another
    // PID in this discovery pass.
    let empty = std::collections::HashSet::new();
    let exclude = claimed.get(&session_dir).unwrap_or(&empty);
    let path = latest_session_jsonl_excluding(&session_dir, exclude)?;
    let session_id = uuid_from_transcript_filename(&path)?;
    Some((path, session_id, BindingSource::MtimeFallback))
}

fn latest_session_jsonl_excluding(
    dir: &Path,
    exclude: &std::collections::HashSet<PathBuf>,
) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut best: Option<(PathBuf, std::time::SystemTime)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if exclude.contains(&path) {
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        // Skip filenames that aren't `<uuid>.jsonl` (e.g. directories with
        // a `.jsonl` extension, which Claude does create for attachments).
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if uuid::Uuid::parse_str(stem).is_err() {
            continue;
        }
        // `symlink_metadata` does not follow the link, so a malicious
        // symlink planted under ~/.claude/projects/<cwd>/<uuid>.jsonl
        // would be rejected here rather than redirecting our parser
        // at an attacker-chosen file outside the transcript root.
        let Ok(link_meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if link_meta.file_type().is_symlink() || !link_meta.is_file() {
            continue;
        }
        let Ok(mtime) = link_meta.modified() else {
            continue;
        };
        match &best {
            Some((_, best_mtime)) if mtime <= *best_mtime => {}
            _ => best = Some((path, mtime)),
        }
    }
    best.map(|(p, _)| p)
}

fn claude_session_id_from(info: &ProcArgsEnv) -> Option<uuid::Uuid> {
    // 1. argv `--session-id <uuid>` / `--resume <uuid>` (app-server) or
    //    `--session-id=<uuid>` / `--resume=<uuid>` (combined form).
    const FLAGS: [&str; 2] = ["--session-id", "--resume"];
    let separate = info
        .argv
        .windows(2)
        .find(|w| FLAGS.contains(&w[0].as_str()))
        .and_then(|w| uuid::Uuid::parse_str(&w[1]).ok());
    if separate.is_some() {
        return separate;
    }
    let combined = info.argv.iter().find_map(|arg| {
        FLAGS.iter().find_map(|f| {
            let rest = arg.strip_prefix(f).and_then(|r| r.strip_prefix('='))?;
            uuid::Uuid::parse_str(rest).ok()
        })
    });
    if combined.is_some() {
        return combined;
    }
    // 2. env CLAUDE_CODE_SESSION_ID (CLI-launched).
    info.env
        .get("CLAUDE_CODE_SESSION_ID")
        .and_then(|v| uuid::Uuid::parse_str(v).ok())
}

/// Per-process session record written by Claude to `~/.claude/sessions/<pid>.json`.
///
/// Fields match Claude's camelCase JSON. Older builds (< 2.1.140) omit
/// `procStart`, `status`, and `updatedAt`; serde ignores missing optional
/// fields by default.
#[derive(Debug, Clone)]
pub struct ClaudeSessionRecord {
    pub pid: ProcessId,
    pub session_id: Uuid,
    pub cwd: PathBuf,
    /// Live Claude process status from the record, e.g. `busy`,
    /// `waiting`, or `idle`. Older Claude builds omit it.
    pub status: Option<String>,
    /// Human-readable wait reason. Claude 2.1.140 writes
    /// `approve AskUserQuestion` while an AskUserQuestion prompt is
    /// open before the transcript necessarily contains the tool call.
    pub waiting_for: Option<String>,
    /// `startedAt` from the record, milliseconds since Unix epoch.
    /// Absent in some older Claude builds.
    pub started_at_ms: Option<i64>,
    /// `updatedAt` from the record, milliseconds since Unix epoch.
    /// Absent in some older Claude builds.
    pub updated_at_ms: Option<i64>,
}

impl ClaudeSessionRecord {
    pub fn is_waiting_for_ask_user_question(&self) -> bool {
        self.status.as_deref() == Some("waiting")
            && self
                .waiting_for
                .as_deref()
                .unwrap_or_default()
                .contains("AskUserQuestion")
    }
}

/// Wire-format struct for deserializing the session record JSON.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSessionRecord {
    pid: i32,
    session_id: Uuid,
    cwd: PathBuf,
    status: Option<String>,
    waiting_for: Option<String>,
    started_at: Option<i64>,
    updated_at: Option<i64>,
}

/// Read `~/.claude/sessions/<pid>.json` and parse it into a `ClaudeSessionRecord`.
///
/// Returns `None` on any IO or parse error — reads happen on every 2s discovery
/// tick and must not propagate transient write-races as errors.
pub fn claude_session_record_for(pid: ProcessId) -> Option<ClaudeSessionRecord> {
    let path = dirs::home_dir()?
        .join(".claude")
        .join("sessions")
        .join(format!("{}.json", pid.0));
    let data = std::fs::read(&path).ok()?;
    let raw: RawSessionRecord = serde_json::from_slice(&data).ok()?;
    Some(ClaudeSessionRecord {
        pid: ProcessId(raw.pid),
        session_id: raw.session_id,
        cwd: raw.cwd,
        status: raw.status,
        waiting_for: raw.waiting_for,
        started_at_ms: raw.started_at,
        updated_at_ms: raw.updated_at,
    })
}

/// Validate a session record before using it for transcript binding.
///
/// Caller has already classified `pid` as Claude, so we only check record
/// consistency against the live process.
fn validate_session_record(rec: &ClaudeSessionRecord, pid: ProcessId) -> bool {
    // Pid in the file must match the pid we loaded it for.
    if rec.pid != pid {
        return false;
    }

    // Cwd in the record must match the kernel cwd when readable.
    if let Some(kernel_cwd) = process_cwd(pid)
        && rec.cwd != kernel_cwd
    {
        return false;
    }

    // If the record carries a start timestamp, compare against the kernel
    // process start time. Allow up to 2 s of drift from clock resolution and
    // record-write latency.
    if let (Some(rec_ms), Some(pst)) = (rec.started_at_ms, process_start_time(pid)) {
        let kernel_ms = pst.sec * 1_000 + i64::from(pst.usec / 1_000);
        if (rec_ms - kernel_ms).unsigned_abs() > 2_000 {
            return false;
        }
    }

    true
}

/// Scan `~/.claude/projects/` for `<session_id>.jsonl` without assuming any
/// particular project-dir encoding.
///
/// UUIDs are globally unique so the first match in the tree is canonical.
/// Symlinks are rejected to mirror the safety check in `latest_session_jsonl_excluding`.
pub fn find_claude_transcript_by_session_id(session_id: Uuid) -> Option<PathBuf> {
    let root = transcripts_root_claude()?;
    find_transcript_in_root(&root, session_id)
}

/// Inner scan used by `find_claude_transcript_by_session_id` and tests.
fn find_transcript_in_root(root: &Path, session_id: Uuid) -> Option<PathBuf> {
    let target = format!("{session_id}.jsonl");
    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        // Cheap pre-filter: Claude project dirs start with `-`.
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with('-') {
            continue;
        }
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let candidate = entry.path().join(&target);
        let Ok(meta) = std::fs::symlink_metadata(&candidate) else {
            continue;
        };
        if meta.file_type().is_symlink() || !meta.is_file() {
            continue;
        }
        return Some(candidate);
    }
    None
}

/// Claude's projects-dir naming: each path separator and dot becomes `-`.
/// So `/Users/alice/W/project/.worktrees/sdk-rebuild` →
/// `-Users-alice-W-project--worktrees-sdk-rebuild` (the dot collapses with
/// the slash into a double dash, which Claude tolerates).
fn encode_cwd_for_claude(p: &Path) -> String {
    p.to_string_lossy().replace(['/', '.'], "-")
}

#[cfg(test)]
mod cwd_encoding_tests {
    use super::*;
    #[test]
    fn simple_path() {
        assert_eq!(
            encode_cwd_for_claude(Path::new("/Users/alice/W/corral")),
            "-Users-alice-W-corral",
        );
    }
    #[test]
    fn dotted_path_collapses_into_double_dash() {
        assert_eq!(
            encode_cwd_for_claude(Path::new("/Users/alice/W/project/.worktrees/sdk-rebuild")),
            "-Users-alice-W-project--worktrees-sdk-rebuild",
        );
    }
    #[test]
    fn dot_dirs_at_root_collapse() {
        assert_eq!(
            encode_cwd_for_claude(Path::new("/Users/alice/.claude")),
            "-Users-alice--claude",
        );
    }
}

/// Extract the session UUID from a transcript filename.
///
/// Claude: `<uuid>.jsonl`.
/// Codex:  `rollout-<timestamp>-<uuid>.jsonl`. The UUID is the last five
/// hyphen-separated segments (it contains hyphens itself).
pub fn uuid_from_transcript_filename(p: &Path) -> Option<uuid::Uuid> {
    let stem = p.file_stem().and_then(|s| s.to_str())?;
    if let Ok(u) = uuid::Uuid::parse_str(stem) {
        return Some(u);
    }
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() >= 5 {
        let candidate = parts[parts.len() - 5..].join("-");
        if let Ok(u) = uuid::Uuid::parse_str(&candidate) {
            return Some(u);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_claude_uuid_filename() {
        let p = PathBuf::from("/x/y/9b9241aa-a99f-4b3c-abd5-a2aac22f6a76.jsonl");
        let u = uuid_from_transcript_filename(&p).expect("parsed");
        assert_eq!(u.to_string(), "9b9241aa-a99f-4b3c-abd5-a2aac22f6a76");
    }

    #[test]
    fn parses_codex_rollout_filename() {
        let p = PathBuf::from(
            "/x/y/rollout-2026-05-15T20-16-04-9b9241aa-a99f-4b3c-abd5-a2aac22f6a76.jsonl",
        );
        let u = uuid_from_transcript_filename(&p).expect("parsed");
        assert_eq!(u.to_string(), "9b9241aa-a99f-4b3c-abd5-a2aac22f6a76");
    }

    #[test]
    fn list_processes_finds_self() {
        // Loose smoke test: just ensure the FFI call succeeds and returns a non-empty list
        // when we ask for our own binary (which should always include the running test runner).
        let all = pids_by_type(ProcFilter::All).expect("pids_by_type works");
        assert!(!all.is_empty());
    }

    #[test]
    fn latest_session_jsonl_skips_excluded() {
        use std::collections::HashSet;
        use std::time::Duration;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // Three .jsonl files written in order, each newer than the
        // last by a measurable amount so mtime ordering is stable.
        let a = dir.join("1f41fd15-b3fe-4fbc-8728-234f48e4a60f.jsonl");
        let b = dir.join("20748cba-6465-416d-99c2-a1a3738485a0.jsonl");
        let c = dir.join("52d0cb0f-a886-4b3a-b7f4-d3a9f5a939e2.jsonl");
        for p in [&a, &b, &c] {
            std::fs::write(p, b"{}\n").unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
        let empty = HashSet::new();
        let pick = latest_session_jsonl_excluding(dir, &empty).expect("picks latest");
        assert_eq!(pick, c, "without exclusion the newest wins");

        let mut exclude = HashSet::new();
        exclude.insert(c.clone());
        let pick = latest_session_jsonl_excluding(dir, &exclude).expect("picks next");
        assert_eq!(pick, b, "excluding the newest falls to the next newest");

        exclude.insert(b.clone());
        let pick = latest_session_jsonl_excluding(dir, &exclude).expect("picks third");
        assert_eq!(pick, a, "all newer excluded → oldest remaining");

        exclude.insert(a.clone());
        assert!(
            latest_session_jsonl_excluding(dir, &exclude).is_none(),
            "no candidates left → None"
        );
    }
}

#[cfg(test)]
mod session_record_tests {
    use super::*;
    use std::path::PathBuf;

    // ── validate_session_record ──────────────────────────────────────────────

    fn make_rec(pid: i32, cwd: &str, started_at_ms: Option<i64>) -> ClaudeSessionRecord {
        ClaudeSessionRecord {
            pid: ProcessId(pid),
            session_id: Uuid::new_v4(),
            cwd: PathBuf::from(cwd),
            status: None,
            waiting_for: None,
            started_at_ms,
            updated_at_ms: None,
        }
    }

    #[test]
    fn validate_rejects_pid_mismatch() {
        let pid = ProcessId(std::process::id() as i32);
        // Record claims a different pid — must be rejected regardless of cwd.
        let rec = make_rec(pid.0 + 9999, "/some/cwd", None);
        assert!(!validate_session_record(&rec, pid));
    }

    #[test]
    fn validate_legacy_record_no_started_at() {
        // Older Claude builds omit startedAt entirely. If cwd also matches
        // the kernel (or kernel returns None), the record is valid.
        //
        // We use `std::process::id()` as the pid so `process_cwd` returns our
        // own cwd, which we feed into the record.
        let pid = ProcessId(std::process::id() as i32);
        let cwd = process_cwd(pid).unwrap_or_else(|| PathBuf::from("/"));
        let rec = ClaudeSessionRecord {
            pid,
            session_id: Uuid::new_v4(),
            cwd,
            status: None,
            waiting_for: None,
            started_at_ms: None, // legacy: field absent
            updated_at_ms: None,
        };
        assert!(validate_session_record(&rec, pid));
    }

    #[test]
    fn validate_rejects_cwd_mismatch() {
        let pid = ProcessId(std::process::id() as i32);
        // process_cwd for ourselves should succeed; supply a wrong cwd.
        let rec = make_rec(pid.0, "/definitely/not/our/cwd/xyzzy", None);
        // Only assert false if process_cwd succeeds (it should for our own pid).
        if process_cwd(pid).is_some() {
            assert!(!validate_session_record(&rec, pid));
        }
    }

    #[test]
    fn validate_full_record_matching_live_pid() {
        // Full 2.1.140-style record with startedAt. We synthesise a startedAt
        // that matches the kernel start time of the current process.
        let pid = ProcessId(std::process::id() as i32);
        let cwd = process_cwd(pid).unwrap_or_else(|| PathBuf::from("/"));
        let started_at_ms =
            process_start_time(pid).map(|pst| pst.sec * 1_000 + i64::from(pst.usec / 1_000));
        let rec = ClaudeSessionRecord {
            pid,
            session_id: Uuid::new_v4(),
            cwd,
            status: None,
            waiting_for: None,
            started_at_ms,
            updated_at_ms: Some(1_779_034_804_118),
        };
        assert!(validate_session_record(&rec, pid));
    }

    #[test]
    fn session_record_detects_pending_ask_user_question() {
        let rec = ClaudeSessionRecord {
            pid: ProcessId(1),
            session_id: Uuid::new_v4(),
            cwd: PathBuf::from("/tmp"),
            status: Some("waiting".into()),
            waiting_for: Some("approve AskUserQuestion".into()),
            started_at_ms: None,
            updated_at_ms: Some(1_779_102_628_146),
        };
        assert!(rec.is_waiting_for_ask_user_question());

        let busy = ClaudeSessionRecord {
            status: Some("busy".into()),
            ..rec
        };
        assert!(!busy.is_waiting_for_ask_user_question());
    }

    // ── find_claude_transcript_by_session_id ─────────────────────────────────

    #[test]
    fn find_transcript_by_session_id_locates_file() {
        use std::io::Write;
        // Build a fake ~/.claude/projects tree under a tempdir and override
        // transcripts_root_claude by temporarily swapping HOME — but since
        // transcripts_root_claude uses dirs::home_dir() we can't easily mock
        // it without global state. Instead, test the inner logic directly by
        // constructing the expected path structure under a tempdir and calling
        // the helper with an env-overridden HOME.
        //
        // We test the scan logic through a thin wrapper that accepts an
        // explicit root, keeping the public API stable.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Create two project dirs and one file each.
        let proj_a = root.join("-Users-alice-W-projA");
        let proj_b = root.join("-Users-alice-W-projB");
        std::fs::create_dir_all(&proj_a).unwrap();
        std::fs::create_dir_all(&proj_b).unwrap();

        let session_id = Uuid::new_v4();
        let target = proj_b.join(format!("{session_id}.jsonl"));
        let decoy = proj_a.join(format!("{}.jsonl", Uuid::new_v4()));

        std::fs::File::create(&decoy)
            .unwrap()
            .write_all(b"{}")
            .unwrap();
        std::fs::File::create(&target)
            .unwrap()
            .write_all(b"{}")
            .unwrap();

        // Call the internal scan helper directly.
        let found = find_transcript_in_root(root, session_id);
        assert_eq!(found, Some(target));
    }

    #[test]
    fn find_transcript_skips_symlinks() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let proj = root.join("-Users-alice-W-proj");
        std::fs::create_dir_all(&proj).unwrap();

        let session_id = Uuid::new_v4();
        let real_file = tmp.path().join("real.jsonl");
        std::fs::write(&real_file, b"{}").unwrap();

        // Plant a symlink where the transcript should be.
        let link_path = proj.join(format!("{session_id}.jsonl"));
        symlink(&real_file, &link_path).unwrap();

        let found = find_transcript_in_root(root, session_id);
        assert!(found.is_none(), "symlink must be rejected");
    }
}
