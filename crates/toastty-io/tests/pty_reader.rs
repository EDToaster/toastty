//! Integration tests for `spawn_pty_reader`.
//!
//! Uses a real PTY pair via `toastty-pty` (dev-dep only). winit's event
//! loop is not built — instead we exercise the `EventSink` seam via a
//! `mpsc::Sender<UserEvent>` that mirrors the proxy's behaviour. The
//! winit-side wiring is one trait impl and is exercised end-to-end by
//! the binary's smoke run.

use std::sync::Mutex;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use toastty_io::{EventSink, SinkClosed, UserEvent, spawn_pty_reader_with_sink};
use toastty_pty::{Pty, PtySpec};

/// Test sink: forwards `UserEvent` into an mpsc channel.
struct ChannelSink(Mutex<mpsc::Sender<UserEvent>>);

impl EventSink for ChannelSink {
    fn send(&self, ev: UserEvent) -> Result<(), SinkClosed> {
        let g = self.0.lock().map_err(|_| SinkClosed)?;
        g.send(ev).map_err(|_| SinkClosed)
    }
}

/// Collect events from the channel for at most `max` time, returning all
/// `PtyBytes` payloads concatenated and a flag indicating whether
/// `PtyClosed` was seen.
fn collect(rx: &mpsc::Receiver<UserEvent>, max: Duration) -> (Vec<u8>, bool) {
    let deadline = Instant::now() + max;
    let mut bytes = Vec::new();
    let mut closed = false;
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        match rx.recv_timeout(deadline - now) {
            Ok(UserEvent::PtyBytes(mut b)) => bytes.append(&mut b),
            Ok(UserEvent::PtyClosed) => {
                closed = true;
                break;
            }
            Err(_) => break,
        }
    }
    (bytes, closed)
}

#[test]
fn reads_echo_output_and_observes_close() {
    let spec = PtySpec::program("/bin/echo").arg("hello, toastty-io");
    let pty = Pty::spawn(&spec).expect("spawn echo");

    let (tx, rx) = mpsc::channel();
    let sink = ChannelSink(Mutex::new(tx));
    let _join = spawn_pty_reader_with_sink(pty.master_fd(), sink).expect("spawn reader");

    let (bytes, closed) = collect(&rx, Duration::from_secs(3));
    let s = String::from_utf8_lossy(&bytes);
    assert!(s.contains("hello, toastty-io"), "got: {s:?}");
    assert!(closed, "expected PtyClosed once child exits");

    drop(pty);
}

#[test]
fn reads_multiple_chunks_from_cat() {
    let spec = PtySpec::program("/bin/cat");
    let pty = Pty::spawn(&spec).expect("spawn cat");

    let (tx, rx) = mpsc::channel();
    let sink = ChannelSink(Mutex::new(tx));
    let _join = spawn_pty_reader_with_sink(pty.master_fd(), sink).expect("spawn reader");

    // Write a few separate lines; cat echoes them back.
    for line in &[b"alpha\n".as_slice(), b"beta\n", b"gamma\n"] {
        pty.write(line).expect("write to cat");
    }

    // cat is long-running; collect with a short timeout that ends when
    // nothing arrives for a while.
    let start = Instant::now();
    let mut bytes = Vec::new();
    while start.elapsed() < Duration::from_secs(2) {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(UserEvent::PtyBytes(mut b)) => bytes.append(&mut b),
            Ok(UserEvent::PtyClosed) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if !bytes.is_empty() {
                    // Got something + nothing new for 200 ms — done.
                    break;
                }
            }
        }
    }
    drop(pty);

    let s = String::from_utf8_lossy(&bytes);
    assert!(s.contains("alpha"), "missing alpha in: {s:?}");
    assert!(s.contains("beta"), "missing beta in: {s:?}");
    assert!(s.contains("gamma"), "missing gamma in: {s:?}");
}

#[test]
fn reports_close_when_child_exits() {
    // /usr/bin/true emits no output but exits immediately.
    let spec = PtySpec::program("/usr/bin/true");
    let pty = Pty::spawn(&spec).expect("spawn true");

    let (tx, rx) = mpsc::channel();
    let sink = ChannelSink(Mutex::new(tx));
    let _join = spawn_pty_reader_with_sink(pty.master_fd(), sink).expect("spawn reader");

    let (_bytes, closed) = collect(&rx, Duration::from_secs(3));
    assert!(closed, "expected PtyClosed when child exits");
    drop(pty);
}
