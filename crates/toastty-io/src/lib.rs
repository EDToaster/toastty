//! mio + winit event-loop bridge.
//!
//! Background mio thread polls the PTY fd and posts a [`UserEvent`] to
//! winit's event loop via [`EventLoopProxy::send_event`]. The render thread
//! receives those events as a winit `UserEvent`, advances the parser, and
//! requests a redraw.
//!
//! See [`docs/decisions/pty-event-loop.md`](../../docs/decisions/pty-event-loop.md).
//!
//! ## Dep graph
//!
//! This crate depends only on `mio`, `winit`, `libc`, and `tracing`. In
//! particular it does **not** depend on `toastty-pty`: the caller hands
//! us a raw `BorrowedFd`, we dup it, and we treat it as an opaque
//! readable byte source. That keeps the dep graph linear and makes the
//! crate trivially reusable for non-PTY fds (e.g. a `socketpair` in
//! tests, or future Wayland-clipboard glue).

#![deny(unsafe_op_in_unsafe_fn)]

use std::io::{self, ErrorKind, Read};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token};
use tracing::{debug, trace, warn};
use winit::event_loop::EventLoopProxy;

/// PTY-reader → winit bridge events.
///
/// The mio thread emits these via `EventLoopProxy::send_event(T::from(...))`,
/// so the main thread can match on them inside its winit `user_event` callback.
#[derive(Debug)]
pub enum UserEvent {
    /// Bytes read from the PTY master. Sized so each event carries a
    /// reasonable chunk (4 KiB max per wake); does not allocate per byte.
    PtyBytes(Vec<u8>),
    /// PTY master closed (child exited / EIO). Main thread should
    /// reap the child and exit.
    PtyClosed,
}

/// Token for the PTY fd inside the mio poll set.
const PTY_TOKEN: Token = Token(0);

/// Per-wake read buffer size. The mio thread reads in chunks of this
/// size, packages each chunk into a `PtyBytes` event, and posts it to the
/// proxy. 4 KiB matches the kernel pipe-buffer default and keeps each
/// allocation small.
const READ_BUF_SIZE: usize = 4096;

/// Returned when the event-loop destination is gone (proxy dropped,
/// channel closed). Causes the reader thread to shut down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SinkClosed;

/// Something that can deliver a [`UserEvent`] to the main thread.
///
/// Implemented for [`EventLoopProxy<T>`] where `T: From<UserEvent>` so
/// the binary can pass its winit proxy. The trait exists so tests can
/// inject a mock that funnels events into an `mpsc::Sender` — building
/// a real winit `EventLoop` on macOS forces a window onto the main
/// thread, which we don't want in unit tests.
pub trait EventSink: Send + 'static {
    /// Deliver one event. Returns `Err(SinkClosed)` if the destination
    /// is gone and the reader should shut down.
    fn send(&self, ev: UserEvent) -> Result<(), SinkClosed>;
}

impl<T> EventSink for EventLoopProxy<T>
where
    T: From<UserEvent> + Send + 'static,
{
    fn send(&self, ev: UserEvent) -> Result<(), SinkClosed> {
        self.send_event(ev.into()).map_err(|_| SinkClosed)
    }
}

/// Spawn a background thread that polls `fd` with mio and posts
/// [`UserEvent::PtyBytes`] to `proxy` as bytes arrive. Returns a
/// `JoinHandle` so the binary can join on shutdown.
///
/// The thread:
/// - dups `fd` so it owns its own copy (caller keeps the original for
///   `write` / `resize` / `try_wait`);
/// - sets the duped fd non-blocking;
/// - registers it with mio for `READABLE`;
/// - on wake, drains it into a 4 KiB buffer (one allocation per wake)
///   and posts `UserEvent::PtyBytes(bytes)`;
/// - exits cleanly when `fd` returns EOF / EIO (posts
///   [`UserEvent::PtyClosed`]) or when `proxy.send_event` fails (the
///   event loop has gone away).
///
/// # Errors
///
/// Returns `io::Error` only if the initial setup fails (dup, fcntl, mio
/// `Poll::new`, `register`). Once the thread is running, errors are
/// logged via `tracing::warn` and the thread exits.
pub fn spawn_pty_reader<T>(
    fd: BorrowedFd<'_>,
    proxy: EventLoopProxy<T>,
) -> io::Result<JoinHandle<()>>
where
    T: From<UserEvent> + Send + 'static,
{
    spawn_pty_reader_with_sink(fd, proxy)
}

/// Variant of [`spawn_pty_reader`] that accepts any [`EventSink`] —
/// public for the integration tests, kept hidden from the rustdoc index.
#[doc(hidden)]
pub fn spawn_pty_reader_with_sink<S: EventSink>(
    fd: BorrowedFd<'_>,
    sink: S,
) -> io::Result<JoinHandle<()>> {
    // Dup the fd up-front so the thread owns its own `OwnedFd`. We rely
    // on libc::dup; rustix's dup is similar but we already need libc for
    // fcntl below, so keep it consistent.
    let raw = fd.as_raw_fd();
    // SAFETY: `raw` is a valid open fd from the caller's `BorrowedFd`.
    // `libc::dup` returns a new fd referring to the same kernel object,
    // or -1 on error.
    let duped_raw = unsafe { libc::dup(raw) };
    if duped_raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `duped_raw` is a fresh fd we own from the libc::dup above.
    let owned = unsafe { OwnedFd::from_raw_fd(duped_raw) };

    set_nonblocking(&owned)?;

    // Build the poll + events buffer on the spawning thread so initial
    // failures surface as Result on the caller.
    let poll = Poll::new()?;
    let source_fd = duped_raw;
    poll.registry()
        .register(&mut SourceFd(&source_fd), PTY_TOKEN, Interest::READABLE)?;

    let handle = thread::Builder::new()
        .name("toastty-io::pty-reader".into())
        .spawn(move || run_loop(owned, poll, &sink))?;

    Ok(handle)
}

/// Wrapper around `fcntl` to set `O_NONBLOCK`.
fn set_nonblocking(fd: &OwnedFd) -> io::Result<()> {
    let raw = fd.as_raw_fd();
    // SAFETY: `raw` is valid for the lifetime of `fd`.
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is valid; new_flags is a sane fcntl arg.
    let rc = unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn run_loop<S: EventSink>(owned: OwnedFd, mut poll: Poll, sink: &S) {
    // Wrap the OwnedFd in std::fs::File so we get `Read`. The fd is
    // already non-blocking; reads will return `WouldBlock` between
    // pipe-fills.
    let mut file = std::fs::File::from(owned);
    let mut events = Events::with_capacity(8);

    debug!("toastty-io::pty-reader started");

    loop {
        if let Err(e) = poll.poll(&mut events, None) {
            if e.kind() == ErrorKind::Interrupted {
                continue;
            }
            warn!("pty-reader poll error: {e}; exiting");
            let _ = sink.send(UserEvent::PtyClosed);
            return;
        }

        for ev in &events {
            if ev.token() != PTY_TOKEN {
                continue;
            }
            // Read in a loop until WouldBlock — mio is level-triggered by
            // default, but draining each wake keeps p99 latency low and
            // gives the parser the largest contiguous chunks possible.
            match drain_into_proxy(&mut file, sink) {
                DrainResult::Continue => {}
                DrainResult::Closed => {
                    debug!("pty-reader EOF; sending PtyClosed");
                    let _ = sink.send(UserEvent::PtyClosed);
                    return;
                }
                DrainResult::ProxyDropped => {
                    debug!("pty-reader proxy dropped; exiting");
                    return;
                }
                DrainResult::Error(e) => {
                    warn!("pty-reader read error: {e}; sending PtyClosed and exiting");
                    let _ = sink.send(UserEvent::PtyClosed);
                    return;
                }
            }
        }
    }
}

#[derive(Debug)]
enum DrainResult {
    /// Read everything currently available; loop back to poll.
    Continue,
    /// EOF (0 bytes returned).
    Closed,
    /// `proxy.send_event` failed — the event loop has been dropped.
    ProxyDropped,
    /// I/O error other than `WouldBlock` / `Interrupted`.
    Error(io::Error),
}

fn drain_into_proxy<S: EventSink>(file: &mut std::fs::File, sink: &S) -> DrainResult {
    loop {
        let mut buf = vec![0u8; READ_BUF_SIZE];
        match file.read(&mut buf) {
            Ok(0) => return DrainResult::Closed,
            Ok(n) => {
                buf.truncate(n);
                trace!("pty-reader read {n} bytes");
                if sink.send(UserEvent::PtyBytes(buf)).is_err() {
                    return DrainResult::ProxyDropped;
                }
            }
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                return DrainResult::Continue;
            }
            Err(ref e) if e.kind() == ErrorKind::Interrupted => {}
            // EIO on Linux when the PTY slave is closed; treat as EOF.
            Err(e) if is_eio(&e) => return DrainResult::Closed,
            Err(e) => return DrainResult::Error(e),
        }
    }
}

fn is_eio(e: &io::Error) -> bool {
    e.raw_os_error() == Some(libc::EIO)
}

// ============================================================================
// PTY writer
// ============================================================================

/// Handle to the background PTY-writer thread. Enqueue bytes with
/// [`PtyWriter::send`]; a dedicated thread drains the queue to the PTY
/// master in order.
///
/// Why a thread instead of writing on the caller's thread: the PTY master
/// is non-blocking (the reader dups it and sets `O_NONBLOCK`, and `dup`
/// shares the open file description, so the write fd is non-blocking too).
/// A single `write()` to the tty accepts at most one input-ring's worth of
/// bytes (~1 KiB on macOS) and returns a short count — the rest is lost if
/// the caller ignores it. That silently truncated large pastes, dropping
/// the bracketed-paste terminator `\x1b[201~` and wedging the app in
/// paste-collect mode. Looping the write on the UI thread would instead
/// freeze the window whenever the child stalls its stdin. This thread does
/// the looping off the UI thread: it writes all queued bytes in order,
/// parking on `poll(POLLOUT)` when the ring is full, so delivery is
/// complete and ordered without ever blocking rendering.
#[derive(Debug)]
pub struct PtyWriter {
    /// `Option` so `Drop` can drop the `Sender` — closing the channel and
    /// unblocking the thread's `rx.iter()` — *before* joining. Joining
    /// first would deadlock: the thread parks in `recv` until the channel
    /// closes, but the channel only closes when this field drops.
    tx: Option<Sender<Vec<u8>>>,
    handle: Option<JoinHandle<()>>,
}

impl PtyWriter {
    /// Enqueue `bytes` for delivery to the PTY. Never blocks. Bytes are
    /// written in call order; each call's payload is written contiguously.
    /// A no-op if the writer thread has already exited (PTY closed).
    pub fn send(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let Some(tx) = self.tx.as_ref() else {
            return;
        };
        if tx.send(bytes.to_vec()).is_err() {
            warn!("pty-writer thread gone; dropping {} bytes", bytes.len());
        }
    }
}

impl Drop for PtyWriter {
    fn drop(&mut self) {
        // Close the channel FIRST so the thread's `rx.iter()` returns and
        // the loop exits; then join. (If the thread is instead parked in
        // `poll(POLLOUT)` mid-write because the child stalled, closing the
        // PTY — which the caller does before dropping us — delivers POLLHUP
        // and unblocks it too.)
        self.tx.take();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Spawn the background writer thread for the PTY master `fd`.
///
/// Like the reader, this dups `fd` so it owns its own copy. Returns a
/// [`PtyWriter`] handle whose [`send`](PtyWriter::send) enqueues bytes.
///
/// # Errors
///
/// Returns `io::Error` only if the initial `dup` fails. Once running, a
/// terminal write error (e.g. `EIO` after the child exits) makes the
/// thread exit; further `send`s become no-ops.
pub fn spawn_pty_writer(fd: BorrowedFd<'_>) -> io::Result<PtyWriter> {
    let raw = fd.as_raw_fd();
    // SAFETY: `raw` is a valid open fd from the caller's `BorrowedFd`.
    let duped_raw = unsafe { libc::dup(raw) };
    if duped_raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `duped_raw` is a fresh fd we own from the libc::dup above.
    let owned = unsafe { OwnedFd::from_raw_fd(duped_raw) };

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let handle = thread::Builder::new()
        .name("toastty-io::pty-writer".into())
        .spawn(move || write_loop(owned, &rx))?;

    Ok(PtyWriter {
        tx: Some(tx),
        handle: Some(handle),
    })
}

fn write_loop(owned: OwnedFd, rx: &Receiver<Vec<u8>>) {
    let raw = owned.as_raw_fd();
    debug!("toastty-io::pty-writer started");
    // Blocks until the next payload is queued; ends when the Sender drops.
    for chunk in rx.iter() {
        if !write_all_blocking(raw, &chunk) {
            // Terminal error (PTY closed). Drain remaining sends without
            // writing so producers don't wedge on a full channel, then exit.
            debug!("pty-writer: master closed; exiting");
            return;
        }
    }
    debug!("toastty-io::pty-writer stopped");
}

/// Write every byte of `buf` to the non-blocking fd `raw`, parking on
/// `poll(POLLOUT)` whenever the tty input ring is full. Returns `false` on
/// a terminal error (the caller should stop), `true` once all bytes land.
fn write_all_blocking(raw: i32, buf: &[u8]) -> bool {
    let mut off = 0;
    while off < buf.len() {
        // SAFETY: raw is a valid open fd; the slice is in-bounds.
        let n = unsafe {
            libc::write(
                raw,
                buf[off..].as_ptr().cast(),
                buf.len() - off,
            )
        };
        if n > 0 {
            off += n as usize;
            continue;
        }
        if n == 0 {
            // Shouldn't happen for a tty; treat as no progress and wait.
            if !wait_writable(raw) {
                return false;
            }
            continue;
        }
        let err = io::Error::last_os_error();
        match err.kind() {
            ErrorKind::WouldBlock => {
                if !wait_writable(raw) {
                    return false;
                }
            }
            ErrorKind::Interrupted => {}
            _ => {
                // EIO once the slave is gone, or any other hard error.
                warn!("pty-writer write failed: {err}");
                return false;
            }
        }
    }
    true
}

/// Block until `raw` is writable (or hung up). Returns `false` if the fd
/// hung up / errored so the caller can stop.
fn wait_writable(raw: i32) -> bool {
    let mut pfd = libc::pollfd {
        fd: raw,
        events: libc::POLLOUT,
        revents: 0,
    };
    loop {
        // SAFETY: pfd points to one valid pollfd; -1 == wait indefinitely.
        let rc = unsafe { libc::poll(&mut pfd, 1, -1) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == ErrorKind::Interrupted {
                continue;
            }
            warn!("pty-writer poll failed: {err}");
            return false;
        }
        // POLLHUP / POLLERR / POLLNVAL mean the master is gone.
        if pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            return false;
        }
        if pfd.revents & libc::POLLOUT != 0 {
            return true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_eio_recognises_eio() {
        let e = io::Error::from_raw_os_error(libc::EIO);
        assert!(is_eio(&e));
    }

    #[test]
    fn is_eio_rejects_other_errors() {
        let e = io::Error::from_raw_os_error(libc::EAGAIN);
        assert!(!is_eio(&e));
        let e = io::Error::from(ErrorKind::WouldBlock);
        assert!(!is_eio(&e));
    }

    #[test]
    fn user_event_debug_renders() {
        let ev = UserEvent::PtyBytes(b"hello".to_vec());
        let s = format!("{ev:?}");
        assert!(s.contains("PtyBytes"));
        let ev = UserEvent::PtyClosed;
        let s = format!("{ev:?}");
        assert!(s.contains("PtyClosed"));
    }
}
