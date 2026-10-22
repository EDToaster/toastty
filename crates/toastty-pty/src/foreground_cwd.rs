//! Foreground-process lookups: CWD and name.
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
    let raw_bytes: &[u8] =
        unsafe { std::slice::from_raw_parts(cdir.as_ptr().cast::<u8>(), len_bytes) };
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

/// Short name of process `pid` (e.g. `"vim"`), for showing the user
/// what is running. Control characters are dropped and an empty name
/// is `None`.
///
/// - **Linux**: `/proc/<pid>/comm`. The kernel truncates it to 15
///   bytes, so a name exactly that long is swapped for the basename of
///   `argv[0]` when that extends it (`cargo-clippy-dr` → full name).
/// - **macOS**: `proc_name`.
#[must_use]
pub fn process_name(pid: u32) -> Option<String> {
    clean_name(&raw_process_name(pid)?)
}

#[cfg(target_os = "linux")]
fn raw_process_name(pid: u32) -> Option<String> {
    /// `TASK_COMM_LEN - 1`: the longest name `comm` can hold.
    const COMM_MAX: usize = 15;
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    let comm = comm.trim_end_matches('\n').to_owned();
    if comm.len() < COMM_MAX {
        return Some(comm);
    }
    let full = std::fs::read(format!("/proc/{pid}/cmdline"))
        .ok()
        .and_then(|cmd| {
            let argv0 = cmd.split(|&b| b == 0).next()?;
            let argv0 = String::from_utf8_lossy(argv0);
            let base = argv0.rsplit('/').next()?.to_owned();
            base.starts_with(&comm).then_some(base)
        });
    Some(full.unwrap_or(comm))
}

#[cfg(target_os = "macos")]
fn raw_process_name(pid: u32) -> Option<String> {
    // `2 * MAXCOMLEN + 1`, what `proc_name` fills at most.
    let mut buf = [0u8; 33];
    // SAFETY: `proc_name` writes at most `buf.len()` bytes into `buf`
    // and returns the length written; a non-positive return is failure
    // and we don't read the buffer.
    let n = unsafe {
        libc::proc_name(
            pid as libc::c_int,
            buf.as_mut_ptr().cast::<libc::c_void>(),
            buf.len() as u32,
        )
    };
    if n <= 0 {
        return None;
    }
    let n = usize::try_from(n).ok()?.min(buf.len());
    Some(String::from_utf8_lossy(&buf[..n]).into_owned())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn raw_process_name(_pid: u32) -> Option<String> {
    None
}

/// Drop control characters (a process can name itself anything) and
/// surrounding whitespace; `None` when nothing is left.
fn clean_name(raw: &str) -> Option<String> {
    let name: String = raw.chars().filter(|c| !c.is_control()).collect();
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_owned())
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
    fn process_name_for_self_is_the_test_binary() {
        // The test harness binary is named after the crate plus a hash
        // (`toastty_pty-0123abcd`); comm may truncate it, so compare
        // the prefix.
        let name = process_name(std::process::id()).expect("self should be readable");
        assert!(name.starts_with("toastty_pty"), "got {name:?}");
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn process_name_for_nonexistent_pid_returns_none() {
        assert!(process_name(0).is_none());
    }

    #[test]
    fn clean_name_strips_controls_and_empties() {
        assert_eq!(clean_name("vim\n").as_deref(), Some("vim"));
        assert_eq!(clean_name("\u{1b}[31mevil").as_deref(), Some("[31mevil"));
        assert_eq!(clean_name("  "), None);
        assert_eq!(clean_name(""), None);
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn process_cwd_for_nonexistent_pid_returns_none() {
        // PID 0 is the kernel on macOS / Linux — proc_pidinfo
        // refuses, /proc/0/cwd doesn't exist.
        assert!(process_cwd(0).is_none());
    }
}
