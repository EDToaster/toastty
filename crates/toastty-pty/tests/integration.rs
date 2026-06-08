//! Integration tests that actually spawn real child processes.
//!
//! These are why `toastty-pty` is exempt from the 95% line gate —
//! you can't usefully unit-test a PTY without forking.

use std::thread;
use std::time::{Duration, Instant};
use toastty_pty::{Pty, PtySpec, WinSize};

/// Run a child and collect its output. Reads non-blockingly while the
/// child is alive — important because slave-close behaviour after
/// `wait()` is OS- and load-sensitive (macOS may drop buffered bytes;
/// Linux returns EIO). Bounded by `max_ms`.
fn run_and_drain(pty: &mut Pty, max_ms: u64) -> Vec<u8> {
    pty.set_nonblocking(true).expect("nonblocking");
    let deadline = Instant::now() + Duration::from_millis(max_ms);
    let mut all = Vec::new();
    let mut buf = [0u8; 4096];
    let mut child_exited = false;
    loop {
        if Instant::now() > deadline {
            break;
        }
        match pty.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => all.extend_from_slice(&buf[..n]),
            Err(toastty_pty::PtyError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if child_exited {
                    break;
                }
                match pty.try_wait() {
                    Ok(Some(_)) => {
                        child_exited = true;
                        // Give the kernel a beat to surface any trailing bytes.
                        thread::sleep(Duration::from_millis(20));
                    }
                    Ok(None) => thread::sleep(Duration::from_millis(5)),
                    Err(_) => break,
                }
            }
            Err(_) => break, // EIO on Linux once slave is closed.
        }
    }
    all
}

/// Drain whatever is currently available without waiting for the child.
/// For tests that talk to a long-running child (like `cat`).
fn drain_now(pty: &Pty, max_ms: u64) -> Vec<u8> {
    pty.set_nonblocking(true).expect("nonblocking");
    let deadline = Instant::now() + Duration::from_millis(max_ms);
    let mut all = Vec::new();
    let mut buf = [0u8; 4096];
    while Instant::now() < deadline {
        match pty.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => all.extend_from_slice(&buf[..n]),
            Err(toastty_pty::PtyError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }
    all
}

#[test]
fn spawn_echo_captures_output() {
    let spec = PtySpec::program("/bin/echo").arg("hello, toastty");
    let mut pty = Pty::spawn(&spec).expect("spawn");
    let out = run_and_drain(&mut pty, 3000);
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("hello, toastty"), "got: {s:?}");
}

#[test]
fn spawn_true_exits_zero() {
    let spec = PtySpec::program("/usr/bin/true");
    let mut pty = Pty::spawn(&spec).expect("spawn");
    let status = pty.wait().expect("wait");
    assert!(status.success(), "exit status: {status:?}");
}

#[test]
fn spawn_false_exits_nonzero() {
    let spec = PtySpec::program("/usr/bin/false");
    let mut pty = Pty::spawn(&spec).expect("spawn");
    let status = pty.wait().expect("wait");
    assert!(!status.success(), "exit status: {status:?}");
}

#[test]
fn write_input_reaches_child() {
    let spec = PtySpec::program("/bin/cat");
    let pty = Pty::spawn(&spec).expect("spawn");
    pty.write(b"ping\n").expect("write");
    // Cat is long-running — drain without waiting.
    let out = drain_now(&pty, 1000);
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("ping"), "expected echoed 'ping' in: {s:?}");
}

#[test]
fn winsize_is_set_on_spawn() {
    let size = WinSize {
        rows: 30,
        cols: 100,
        pixel_width: 0,
        pixel_height: 0,
    };
    let spec = PtySpec::program("/bin/sh")
        .arg("-c")
        .arg("stty size")
        .size(size);
    let mut pty = Pty::spawn(&spec).expect("spawn");
    let out = run_and_drain(&mut pty, 3000);
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("30 100"), "expected '30 100' in: {s:?}");
}

#[test]
fn resize_propagates_to_child() {
    let spec = PtySpec::program("/bin/sh")
        .arg("-c")
        .arg("sleep 0.05; stty size");
    let mut pty = Pty::spawn(&spec).expect("spawn");
    pty.resize(WinSize {
        rows: 50,
        cols: 200,
        pixel_width: 0,
        pixel_height: 0,
    })
    .expect("resize");
    let out = run_and_drain(&mut pty, 3000);
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("50 200"), "expected '50 200' in: {s:?}");
}

#[test]
fn spawn_failure_for_nonexistent_program() {
    let spec = PtySpec::program("/no/such/binary/exists/here");
    let result = Pty::spawn(&spec);
    assert!(result.is_err(), "spawn of nonexistent program should fail");
}

#[test]
fn has_running_program_false_for_exited_child() {
    // `/usr/bin/true` exits immediately. Once we've reaped it, the slave
    // side has no foreground pgrp to report, so `tcgetpgrp` fails and
    // `has_running_program()` must take the `None` → false path. A
    // dead/reaped child must never block close.
    let spec = PtySpec::program("/usr/bin/true");
    let mut pty = Pty::spawn(&spec).expect("spawn");
    let status = pty.wait().expect("wait");
    assert!(status.success(), "exit status: {status:?}");
    assert!(
        !pty.has_running_program(),
        "exited+reaped child should report no running program"
    );
}

#[test]
fn has_running_program_false_when_child_is_foreground_leader() {
    // Spawning `sleep` directly makes it the PTY child *and* its own
    // foreground pgrp leader, so `foreground_pid == child_id` and the
    // method returns false — "sleep" is effectively the shell here.
    let spec = PtySpec::program("/bin/sleep").arg("60");
    let pty = Pty::spawn(&spec).expect("spawn");
    assert!(
        !pty.has_running_program(),
        "direct child that leads its own pgrp should report no separate program"
    );
}

#[test]
fn drop_kills_running_child() {
    let spec = PtySpec::program("/bin/sleep").arg("60");
    let pty = Pty::spawn(&spec).expect("spawn");
    let pid = pty.child_id() as i32;
    drop(pty);
    // Give the kernel a moment to reap.
    thread::sleep(Duration::from_millis(50));
    // SAFETY: kill with signal 0 is just an existence check.
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    assert!(!alive, "child {pid} should have been reaped");
}
