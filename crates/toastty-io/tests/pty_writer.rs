//! Integration tests for `spawn_pty_writer`.
//!
//! Regression coverage for the large-paste truncation bug: the PTY master
//! is non-blocking, so a single `write()` accepts at most one tty
//! input-ring's worth (~1 KiB on macOS) and returns a short count. The old
//! `write_pty` ignored that count, so a paste bigger than the ring lost its
//! tail — including the bracketed-paste terminator `\x1b[201~` — and wedged
//! the foreground app in paste-collect mode. `PtyWriter` must deliver every
//! byte in order regardless of ring size.

use std::os::fd::{AsRawFd, BorrowedFd};
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use toastty_io::{
    EventSink, SinkClosed, UserEvent, spawn_pty_reader_with_sink, spawn_pty_writer,
};
use toastty_pty::{Pty, PtySpec};

struct ChannelSink(Mutex<mpsc::Sender<UserEvent>>);

impl EventSink for ChannelSink {
    fn send(&self, ev: UserEvent) -> Result<(), SinkClosed> {
        let g = self.0.lock().map_err(|_| SinkClosed)?;
        g.send(ev).map_err(|_| SinkClosed)
    }
}

/// Put the tty behind `fd` into raw mode so bytes round-trip 1:1 through
/// `cat` (no echo, no CR/LF translation) and the ~1 KiB input ring is the
/// only bottleneck.
fn make_raw(fd: BorrowedFd<'_>) {
    // SAFETY: fd is a valid open tty fd; termios is initialised by
    // tcgetattr before cfmakeraw reads it.
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        assert_eq!(libc::tcgetattr(fd.as_raw_fd(), &mut t), 0, "tcgetattr");
        libc::cfmakeraw(&mut t);
        assert_eq!(
            libc::tcsetattr(fd.as_raw_fd(), libc::TCSANOW, &t),
            0,
            "tcsetattr"
        );
    }
}

/// Collect echoed bytes from the reader channel until `want` bytes have
/// arrived or `max` elapses.
fn collect_until(rx: &mpsc::Receiver<UserEvent>, want: usize, max: Duration) -> Vec<u8> {
    let deadline = Instant::now() + max;
    let mut bytes = Vec::new();
    while bytes.len() < want {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        match rx.recv_timeout(deadline - now) {
            Ok(UserEvent::PtyBytes(mut b)) => bytes.append(&mut b),
            Ok(UserEvent::PtyClosed) | Err(_) => break,
        }
    }
    bytes
}

/// Build a bracketed paste of `text_len` filler bytes wrapped in the
/// DECSET 2004 markers, mirroring `toastty::paste::wrap_for_paste`.
fn bracketed_paste(text_len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(6 + text_len + 6);
    v.extend_from_slice(b"\x1b[200~");
    v.extend(std::iter::repeat(b'x').take(text_len));
    v.extend_from_slice(b"\x1b[201~");
    v
}

#[test]
fn large_paste_delivers_every_byte_including_terminator() {
    // cat echoes stdin back to stdout, so the reader sees exactly what the
    // writer delivered to the child.
    let spec = PtySpec::program("/bin/cat");
    let pty = Pty::spawn(&spec).expect("spawn cat");
    make_raw(pty.master_fd());

    let (tx, rx) = mpsc::channel();
    let sink = ChannelSink(Mutex::new(tx));
    let _reader = spawn_pty_reader_with_sink(pty.master_fd(), sink).expect("spawn reader");
    let writer = spawn_pty_writer(pty.master_fd()).expect("spawn writer");

    // 8 KiB: many times the ~1 KiB ring, so the writer parks on POLLOUT
    // repeatedly. The old single-write path would have stopped at ~1022.
    let paste = bracketed_paste(8192);
    writer.send(&paste);

    let got = collect_until(&rx, paste.len(), Duration::from_secs(5));

    assert_eq!(
        got.len(),
        paste.len(),
        "expected all {} bytes echoed back, got {}",
        paste.len(),
        got.len()
    );
    assert_eq!(got, paste, "round-tripped bytes differ from what was sent");
    assert!(
        got.ends_with(b"\x1b[201~"),
        "closing bracketed-paste terminator was not delivered"
    );

    drop(writer);
    drop(pty);
}

#[test]
fn writes_are_delivered_in_order() {
    let spec = PtySpec::program("/bin/cat");
    let pty = Pty::spawn(&spec).expect("spawn cat");
    make_raw(pty.master_fd());

    let (tx, rx) = mpsc::channel();
    let sink = ChannelSink(Mutex::new(tx));
    let _reader = spawn_pty_reader_with_sink(pty.master_fd(), sink).expect("spawn reader");
    let writer = spawn_pty_writer(pty.master_fd()).expect("spawn writer");

    // Interleave a big (ring-spanning) send with small ones; order must be
    // preserved even though the big one forces the writer to park.
    let mut expected = Vec::new();
    for chunk in [
        b"AAAA".as_slice(),
        &vec![b'B'; 4096],
        b"CCCC".as_slice(),
        &vec![b'D'; 4096],
        b"EEEE".as_slice(),
    ] {
        writer.send(chunk);
        expected.extend_from_slice(chunk);
    }

    let got = collect_until(&rx, expected.len(), Duration::from_secs(5));
    assert_eq!(got, expected, "bytes arrived out of order or incomplete");

    drop(writer);
    drop(pty);
}

/// Documents *why* the writer must loop: a lone non-blocking `Pty::write`
/// of a ring-spanning payload returns a short count. If this ever stops
/// truncating, the writer's poll-loop is no longer load-bearing — but until
/// then, removing it reintroduces the paste bug.
#[test]
fn single_raw_write_truncates_confirming_the_hazard() {
    let spec = PtySpec::program("/bin/sleep").arg("30");
    let pty = Pty::spawn(&spec).expect("spawn sleep");
    make_raw(pty.master_fd());
    pty.set_nonblocking(true).expect("nonblocking");

    let blob = vec![b'x'; 8192];
    let accepted = pty.write(&blob).expect("write");
    assert!(
        accepted < blob.len(),
        "expected a short write on a full ring, but all {} bytes were accepted \
         (ring behaviour changed — revisit whether PtyWriter's loop is still needed)",
        blob.len()
    );
}
