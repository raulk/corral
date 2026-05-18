// Compile-time layout probe for the Darwin libproc / sysctl structs
// that watcher-core's `proc.rs` decodes by hand. If Apple ever bumps
// any of these sizes or shifts a field offset, the build fails here
// instead of producing silently-wrong reads at runtime.
//
// Keep the constants in lockstep with the Rust-side mirrors in
// `crates/watcher-core/src/proc.rs`.

#include <sys/proc_info.h>
#include <sys/sysctl.h>
#include <stddef.h>

_Static_assert(
    sizeof(struct proc_vnodepathinfo) == 2352,
    "PROC_VNODEPATHINFO_SIZE mismatch"
);
_Static_assert(
    offsetof(struct proc_vnodepathinfo, pvi_cdir.vip_path) == 152,
    "PVI_CDIR_VIP_PATH_OFFSET mismatch"
);

_Static_assert(
    sizeof(struct vnode_fdinfowithpath) == 1200,
    "VNODE_FDINFOWITHPATH_SIZE mismatch"
);
_Static_assert(
    offsetof(struct vnode_fdinfowithpath, pvip.vip_path) == 176,
    "VIP_PATH_OFFSET mismatch"
);

_Static_assert(
    sizeof(struct kinfo_proc) == 648,
    "KINFO_PROC_SIZE mismatch"
);
_Static_assert(
    offsetof(struct kinfo_proc, kp_proc.p_starttime) == 0,
    "P_STARTTIME_OFFSET mismatch"
);
_Static_assert(
    sizeof(struct timeval) == 16,
    "TIMEVAL_SIZE mismatch"
);
_Static_assert(
    offsetof(struct timeval, tv_sec) == 0,
    "TIMEVAL_TV_SEC_OFFSET mismatch"
);
_Static_assert(
    offsetof(struct timeval, tv_usec) == 8,
    "TIMEVAL_TV_USEC_OFFSET mismatch"
);
_Static_assert(
    offsetof(struct kinfo_proc, kp_eproc.e_ppid) == 560,
    "E_PPID_OFFSET mismatch"
);

// A symbol so cc::Build::compile doesn't complain about an empty
// translation unit.
int watcher_layout_probe_ok = 1;
