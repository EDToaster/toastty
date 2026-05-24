//! Foreground-process CWD lookup.
//!
//! Given the PTY master fd, find the foreground process group on the
//! slave side (`tcgetpgrp`) and read its working directory from the
//! kernel. Used by RGP to resolve `path=` values relative to the
//! currently-running app's CWD — which is what every TUI app
//! intuitively assumes, even though neither Ratty nor most other
//! terminals actually implement it.
//!
//! Platform support (this crate is `cfg(unix)`-gated, so we only
//! need macOS + Linux here; ConPTY-era Windows support is a v2 item):
//! - **macOS**: `proc_pidinfo(pid, PROC_PIDVNODEPATHINFO, ...)`.
//! - **Linux**: `std::fs::read_link("/proc/<pid>/cwd")`.
//! - **Other Unix** (BSDs etc): returns `None` — host-specific
//!   procfs paths haven't been wired. Falls back to caller's other
//!   CWD sources (OSC 7, toastty's own CWD).

use std::os::fd::{AsRawFd, BorrowedFd};
use std::path::PathBuf;

/// PID of the foreground process group on the slave side of the
/// PTY whose master is `master_fd`. Returns `None` if `tcgetpgrp`
/// fails (e.g. the child has exited, no slave attached).
#[must_use]
pub fn foreground_pid(master_fd: BorrowedFd<'_>) -> Option<u32> {
    // SAFETY: `tcgetpgrp` reads a process group from a fd; no
    // aliasing, no buffers. A non-positive return signals error.
    let pid = unsafe { libc::tcgetpgrp(master_fd.as_raw_fd()) };
    if pid <= 0 {
        return None;
    }
    Some(pid as u32)
}

/// Read process `pid`'s current working directory from the kernel.
#[cfg(target_os = "macos")]
#[must_use]
pub fn process_cwd(pid: u32) -> Option<PathBuf> {
    use std::ffi::CStr;
    use std::mem::MaybeUninit;

    let mut info: MaybeUninit<libc::proc_vnodepathinfo> = MaybeUninit::uninit();
    // SAFETY: `proc_pidinfo` with `PROC_PIDVNODEPATHINFO` writes
    // `size_of::<proc_vnodepathinfo>()` bytes into the buffer on
    // success. A non-positive return means failure (bad pid,
    // permission denied, etc.) and we don't read the buffer.
    let bytes_written = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            info.as_mut_ptr().cast::<libc::c_void>(),
            std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int,
        )
    };
    if bytes_written <= 0 {
        return None;
    }
    // SAFETY: positive return → buffer is fully initialised.
    let info = unsafe { info.assume_init() };
    // `vip_path` is typed as `[[c_char; 32]; 32]` in `libc` (a
    // 2D-array workaround for old rustc support) but is
    // memory-equivalent to `[c_char; 1024]` = MAXPATHLEN. Use
    // `size_of_val` so we read all 1024 bytes, not just `.len()`
    // (which returns the outer dimension, 32).
    let cdir = &info.pvi_cdir.vip_path;
    let len_bytes = std::mem::size_of_val(cdir);
    // SAFETY: `cdir` covers `len_bytes` contiguous bytes (kernel-
    // populated). The kernel guarantees a NUL within that span for
    // any successful PROC_PIDVNODEPATHINFO.
    let raw_bytes: &[u8] = unsafe {
        std::slice::from_raw_parts(cdir.as_ptr().cast::<u8>(), len_bytes)
    };
    let cstr = CStr::from_bytes_until_nul(raw_bytes).ok()?;
    let s = cstr.to_str().ok()?;
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s))
    }
}

#[cfg(target_os = "linux")]
#[must_use]
pub fn process_cwd(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
#[must_use]
pub fn process_cwd(_pid: u32) -> Option<PathBuf> {
    None
}

/// Convenience: combine [`foreground_pid`] and [`process_cwd`]. The
/// hot path the binary calls before each PTY-byte batch.
#[must_use]
pub fn foreground_cwd(master_fd: BorrowedFd<'_>) -> Option<PathBuf> {
    process_cwd(foreground_pid(master_fd)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On macOS / Linux, looking up the *current* process's CWD
    /// must match `std::env::current_dir()`. This is the closest
    /// portable round-trip we can do without spawning a subprocess.
    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn process_cwd_for_self_matches_env_current_dir() {
        let me = std::process::id();
        let got = process_cwd(me).expect("self should be readable");
        let expected = std::env::current_dir().expect("env::current_dir");
        // Canonicalise both — macOS's proc_pidinfo returns the
        // resolved path while env::current_dir returns the symbolic
        // one (e.g. /private/tmp vs /tmp).
        let got_canon = got.canonicalize().unwrap_or(got);
        let expected_canon = expected.canonicalize().unwrap_or(expected);
        assert_eq!(got_canon, expected_canon);
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn process_cwd_for_nonexistent_pid_returns_none() {
        // PID 0 is the kernel on macOS / Linux — proc_pidinfo
        // refuses, /proc/0/cwd doesn't exist.
        assert!(process_cwd(0).is_none());
    }
}
