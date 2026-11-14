//! Integration tests that actually spawn real child processes.
//!
//! These are why `toastty-pty` is exempt from the 95% line gate —
//! you can't usefully unit-test a PTY without forking.

use std::os::fd::AsRawFd;
use std::thread;
use std::time::{Duration, Instant};
use toastty_pty::{Pty, PtySpec, WinSize};

/// Collect a child's output, acknowledging `expected` with a newline.
/// The child waits for that acknowledgement before exiting: merely
/// reading concurrently with an instant exit still loses bytes on macOS
/// when the last slave closes before the reader runs. Bounded by `max_ms`.
fn run_and_drain(pty: &mut Pty, expected: &[u8], max_ms: u64) -> Vec<u8> {
    pty.set_nonblocking(true).expect("nonblocking");
    let deadline = Instant::now() + Duration::from_millis(max_ms);
    let mut all = Vec::new();
    let mut buf = [0u8; 4096];
    let mut child_exited = false;
    let mut acknowledged = false;
    loop {
        if Instant::now() > deadline {
            break;
        }
        match pty.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                all.extend_from_slice(&buf[..n]);
                if !acknowledged && all.windows(expected.len()).any(|s| s == expected) {
                    assert_eq!(pty.write(b"\n").expect("acknowledge output"), 1);
                    acknowledged = true;
                }
            }
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
fn child_does_not_inherit_another_ptys_master() {
    let first = Pty::spawn(&PtySpec::program("/bin/sleep").arg("60")).expect("first PTY");
    let inherited_fd = first.master_fd().as_raw_fd();
    let mut second = Pty::spawn(
        &PtySpec::program("/bin/sh").args(["-c", &format!("test ! -e /dev/fd/{inherited_fd}")]),
    )
    .expect("second PTY");
    assert!(
        second.wait().expect("wait").success(),
        "child inherited another PTY's master fd {inherited_fd}"
    );
}

#[test]
fn spawn_echo_captures_output() {
    let spec = PtySpec::program("/bin/sh").args(["-c", "echo 'hello, toastty'; read -r ack"]);
    let mut pty = Pty::spawn(&spec).expect("spawn");
    let out = run_and_drain(&mut pty, b"hello, toastty", 3000);
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
        .arg("stty size; read -r ack")
        .size(size);
    let mut pty = Pty::spawn(&spec).expect("spawn");
    let out = run_and_drain(&mut pty, b"30 100", 3000);
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("30 100"), "expected '30 100' in: {s:?}");
}

#[test]
fn resize_propagates_to_child() {
    let spec = PtySpec::program("/bin/sh")
        .arg("-c")
        .arg("read -r ready; stty size; read -r ack");
    let mut pty = Pty::spawn(&spec).expect("spawn");
    pty.resize(WinSize {
        rows: 50,
        cols: 200,
        pixel_width: 0,
        pixel_height: 0,
    })
    .expect("resize");
    assert_eq!(pty.write(b"\n").expect("request size after resize"), 1);
    let out = run_and_drain(&mut pty, b"50 200", 3000);
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
fn running_program_name_reports_job_started_by_interactive_shell() {
    // An interactive bash puts `sleep` in its own foreground pgrp, which
    // is the case the close prompt names. No rc files and no history so
    // the test leaves nothing behind.
    let spec = PtySpec::program("/bin/bash")
        .args(["--norc", "--noprofile", "-i"])
        .env("HISTFILE", "/dev/null")
        .env("PATH", "/usr/bin:/bin");
    let pty = Pty::spawn(&spec).expect("spawn");
    assert_eq!(pty.running_program_name(), None, "bare shell names nothing");
    pty.write(b"sleep 60\n").expect("write");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut name = None;
    while Instant::now() < deadline {
        drain_now(&pty, 20);
        name = pty.running_program_name();
        // Job control publishes the new pgrp before the child execs;
        // its name can briefly still be "bash".
        if name.as_deref() == Some("sleep") {
            break;
        }
    }
    assert_eq!(name.as_deref(), Some("sleep"));
    assert!(pty.has_running_program());
}

#[test]
fn drop_reaps_child_with_unread_output() {
    // Leave bytes queued, as an interactive shell can do while handling
    // SIGHUP during teardown. Waiting before closing/draining the master
    // deadlocked in macOS CI even though SIGKILL had been delivered.
    let pty =
        Pty::spawn(&PtySpec::program("/bin/sh").args(["-c", "printf pending; exec /bin/sleep 60"]))
            .expect("spawn");
    let pid = pty.child_id() as i32;
    let mut ready = libc::pollfd {
        fd: pty.master_fd().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: ready is one valid pollfd, and the PTY remains open.
    assert_eq!(unsafe { libc::poll(&raw mut ready, 1, 3000) }, 1);
    assert_ne!(ready.revents & libc::POLLIN, 0, "expected unread output");
    let (tx, rx) = std::sync::mpsc::channel();
    let teardown = thread::spawn(move || {
        drop(pty);
        tx.send(()).unwrap();
    });
    rx.recv_timeout(Duration::from_secs(3))
        .expect("PTY drop must reap without waiting for a reader");
    teardown.join().unwrap();
    // SAFETY: signal 0 only checks for existence.
    assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "child was not reaped");
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
