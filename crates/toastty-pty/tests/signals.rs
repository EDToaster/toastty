//! PTY-1: the child starts with default signal dispositions and an empty
//! signal mask, whatever toastty itself inherited.
//!
//! A separate test binary, because it changes process-wide signal state.

#![cfg(target_os = "linux")]

use std::thread;
use std::time::{Duration, Instant};
use toastty_pty::{Pty, PtySpec};

/// `SigIgn` / `SigBlk` of the child, read from its own `/proc/self/status`.
fn child_signal_masks() -> (u64, u64) {
    let spec = PtySpec::program("/bin/cat").arg("/proc/self/status");
    let mut pty = Pty::spawn(&spec).expect("spawn");
    pty.set_nonblocking(true).expect("nonblocking");
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    while Instant::now() < deadline {
        match pty.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(toastty_pty::PtyError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if matches!(pty.try_wait(), Ok(Some(_))) && !out.is_empty() {
                    thread::sleep(Duration::from_millis(20));
                    if let Ok(n) = pty.read(&mut buf) {
                        out.extend_from_slice(&buf[..n]);
                    }
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&out);
    let field = |name: &str| {
        let v = text
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .unwrap_or_else(|| panic!("{name} missing in {text:?}"));
        u64::from_str_radix(v.trim(), 16).expect("hex")
    };
    (field("SigIgn:"), field("SigBlk:"))
}

#[test]
fn child_gets_default_signals_and_an_empty_mask() {
    // Start like `toastty &` from a non-interactive script: SIGINT and
    // SIGQUIT ignored. Also block SIGUSR1 in this (the spawning) thread.
    // SAFETY: plain libc signal-state calls on valid arguments; this test
    // binary does nothing else with these signals.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGQUIT, libc::SIG_IGN);
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&raw mut set);
        libc::sigaddset(&raw mut set, libc::SIGUSR1);
        libc::pthread_sigmask(libc::SIG_BLOCK, &raw const set, std::ptr::null_mut());
    }
    let (ign, blk) = child_signal_masks();
    assert_eq!(
        ign, 0,
        "SigIgn {ign:#x}: ignored signals leaked to the shell"
    );
    assert_eq!(
        blk, 0,
        "SigBlk {blk:#x}: blocked signals leaked to the shell"
    );
}
