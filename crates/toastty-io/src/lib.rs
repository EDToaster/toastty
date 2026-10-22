//! mio + winit event-loop bridge.
//!
//! A background mio thread reads the PTY master into a bounded buffer it
//! shares with the main thread, and wakes winit's event loop with
//! [`UserEvent::PtyReady`] via [`EventLoopProxy::send_event`]. The main
//! thread drains that buffer through its [`PtyReader`] at its own pace,
//! feeds the parser, and redraws.
//!
//! See [`docs/decisions/pty-event-loop.md`](../../docs/decisions/pty-event-loop.md).
//!
//! ## Flow control
//!
//! Two properties matter under a flood (`cat` of a big file), where the
//! child writes far faster than the main thread can parse:
//!
//! - **At most one wake in flight.** The reader posts `PtyReady` only when
//!   the main thread has taken everything since the last one. winit drains
//!   every queued user event before it delivers `RedrawRequested`, so the
//!   old one-event-per-4-KiB-read design starved redraws for the whole
//!   flood (no frame until `cat` finished).
//! - **Bounded buffering.** Once [`BUFFER_CAP`] bytes are waiting, the
//!   reader stops reading until the main thread takes them. The kernel
//!   PTY buffer then fills and the child blocks in `write`, instead of the
//!   reader queueing hundreds of MB the main thread will take seconds to
//!   parse (and that a Ctrl-C can't cancel).
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
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};

use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token};
use tracing::{debug, trace, warn};
use winit::event_loop::EventLoopProxy;

/// PTY-reader → winit bridge events.
///
/// The mio thread emits these via `EventLoopProxy::send_event(T::from(...))`,
/// so the main thread can match on them inside its winit `user_event` callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserEvent {
    /// The reader has buffered bytes, or seen the PTY close, since the
    /// main thread last took its buffer. Drain it with
    /// [`PtyReader::peek`] / [`PtyReader::consume`]; the close is
    /// [`PtyReader::is_finished`]. Carries no payload: the bytes stay in
    /// the shared buffer, and the next wake is posted only after the main
    /// thread has taken them, so the event queue never holds more than one.
    PtyReady,
}

/// Token for the PTY fd inside the mio poll set.
const PTY_TOKEN: Token = Token(0);

/// How many bytes the reader buffers ahead of the main thread before it
/// stops reading (it can overshoot by one [`READ_BUF_SIZE`] batch). Big
/// enough that the reader never starves the parser — it refills this in
/// well under a millisecond, the parser needs several to get through it —
/// and small enough that a Ctrl-C during a flood takes effect after a few
/// ms of already-read output, not after hundreds of MB.
pub const BUFFER_CAP: usize = 256 * 1024;

/// Size of the reader thread's private read buffer. A Linux PTY read
/// returns at most ~4 KiB, so the reader keeps reading into this until it
/// is full or the fd runs dry, and only then takes the shared lock: one
/// lock + memcpy per 64 KiB instead of per read.
const READ_BUF_SIZE: usize = 64 * 1024;

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

/// State shared by the reader thread and the main thread's [`PtyReader`].
#[derive(Debug, Default)]
struct Shared {
    state: Mutex<State>,
    /// Signalled when the main thread takes a full buffer, or detaches.
    space: Condvar,
}

#[derive(Debug, Default)]
struct State {
    /// Bytes read but not yet taken by the main thread.
    buf: Vec<u8>,
    /// A `PtyReady` was posted and the main thread hasn't taken the
    /// buffer since. While set, the reader appends without posting.
    wake_pending: bool,
    /// The reader hit EOF / an error and has exited; `buf` holds the
    /// last bytes it will ever deliver.
    closed: bool,
    /// The [`PtyReader`] was dropped. A reader blocked on a full buffer
    /// exits instead of waiting forever.
    detached: bool,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // A panic while holding the lock leaves only a Vec and three
        // flags behind, all still consistent; keep going.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Reader side: block while [`BUFFER_CAP`] bytes are waiting.
    /// Returns `false` if the main thread detached.
    fn wait_for_space(&self) -> bool {
        let mut st = self.lock();
        while st.buf.len() >= BUFFER_CAP && !st.detached {
            st = self
                .space
                .wait(st)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        !st.detached
    }

    /// Reader side: append `bytes`. Returns whether the caller must post
    /// a `PtyReady` (none is in flight).
    fn push(&self, bytes: &[u8]) -> bool {
        let mut st = self.lock();
        st.buf.extend_from_slice(bytes);
        !std::mem::replace(&mut st.wake_pending, true)
    }

    /// Reader side: the PTY closed. Returns whether the caller must post
    /// a `PtyReady` to report it.
    fn close(&self) -> bool {
        let mut st = self.lock();
        st.closed = true;
        !std::mem::replace(&mut st.wake_pending, true)
    }

    /// Main side: swap the shared buffer with `into` (which must be
    /// empty) and re-arm the wake. Returns whether the PTY has closed.
    fn take(&self, into: &mut Vec<u8>) -> bool {
        debug_assert!(into.is_empty());
        let mut st = self.lock();
        std::mem::swap(&mut st.buf, into);
        st.wake_pending = false;
        let closed = st.closed;
        drop(st);
        // The reader only waits while the buffer is full, so only then is
        // there anyone to wake (a futex wake is a syscall even without
        // waiters).
        if into.len() >= BUFFER_CAP {
            self.space.notify_one();
        }
        closed
    }
}

/// The main thread's end of the PTY reader: the bytes read so far, in
/// order. Returned by [`spawn_pty_reader`].
///
/// Bytes are taken from the reader thread a whole buffer at a time (a
/// swap, so no copy and, once both buffers have grown, no allocation);
/// [`peek`](Self::peek) / [`consume`](Self::consume) then hand them out
/// in pieces so the caller can parse with a time budget and stop at any
/// byte.
#[derive(Debug)]
pub struct PtyReader {
    shared: Arc<Shared>,
    /// The batch being consumed; `buf[pos..]` is still unparsed.
    buf: Vec<u8>,
    pos: usize,
    /// The reader thread reported close at the last take, so `buf` holds
    /// the final bytes.
    closed: bool,
    /// Held, not joined: the thread may be parked in `poll` on an fd that
    /// stays open until the process exits.
    handle: Option<JoinHandle<()>>,
}

impl PtyReader {
    /// Up to `max` unconsumed bytes, oldest first, without consuming them.
    /// Takes the reader thread's buffer when the current batch is used
    /// up. Empty when nothing is buffered.
    pub fn peek(&mut self, max: usize) -> &[u8] {
        if self.pos == self.buf.len() && !self.closed {
            self.buf.clear();
            self.pos = 0;
            self.closed = self.shared.take(&mut self.buf);
        }
        let end = self.buf.len().min(self.pos.saturating_add(max));
        &self.buf[self.pos..end]
    }

    /// Mark the first `n` bytes of the last [`peek`](Self::peek) as
    /// consumed.
    pub fn consume(&mut self, n: usize) {
        debug_assert!(n <= self.buf.len() - self.pos);
        self.pos = (self.pos + n).min(self.buf.len());
    }

    /// Whether unconsumed bytes are already in hand (not counting any
    /// the reader thread buffered since the last take).
    pub fn has_unconsumed(&self) -> bool {
        self.pos < self.buf.len()
    }

    /// The PTY has closed and every byte read before the close has been
    /// consumed. Valid after a [`peek`](Self::peek) that returned empty.
    pub fn is_finished(&self) -> bool {
        self.closed && self.pos == self.buf.len()
    }

    /// The reader thread's handle, for tests that wait for it to exit.
    #[doc(hidden)]
    pub fn take_join_handle(&mut self) -> Option<JoinHandle<()>> {
        self.handle.take()
    }
}

impl Drop for PtyReader {
    fn drop(&mut self) {
        // Release a reader blocked on a full buffer so its thread exits.
        self.shared.lock().detached = true;
        self.shared.space.notify_all();
    }
}

/// Spawn a background thread that polls `fd` with mio, buffers what it
/// reads, and posts [`UserEvent::PtyReady`] to `proxy` when there is
/// something new to take. Returns the main thread's [`PtyReader`].
///
/// The thread:
/// - dups `fd` so it owns its own copy (caller keeps the original for
///   `write` / `resize` / `try_wait`);
/// - sets the duped fd non-blocking;
/// - registers it with mio for `READABLE`;
/// - on wake, drains it into the shared buffer, stopping while
///   [`BUFFER_CAP`] bytes are waiting (see the module docs);
/// - exits cleanly when `fd` returns EOF / EIO (marks the reader closed
///   and wakes the loop), when `proxy.send_event` fails (the event loop
///   has gone away) or when the [`PtyReader`] is dropped while it waits
///   for space.
///
/// # Errors
///
/// Returns `io::Error` only if the initial setup fails (dup, fcntl, mio
/// `Poll::new`, `register`). Once the thread is running, errors are
/// logged via `tracing::warn` and the thread exits.
pub fn spawn_pty_reader<T>(fd: BorrowedFd<'_>, proxy: EventLoopProxy<T>) -> io::Result<PtyReader>
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
) -> io::Result<PtyReader> {
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

    let shared = Arc::new(Shared::default());
    let thread_shared = Arc::clone(&shared);
    let handle = thread::Builder::new()
        .name("toastty-io::pty-reader".into())
        .spawn(move || run_loop(owned, poll, &thread_shared, &sink))?;

    Ok(PtyReader {
        shared,
        buf: Vec::new(),
        pos: 0,
        closed: false,
        handle: Some(handle),
    })
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

fn run_loop<S: EventSink>(owned: OwnedFd, mut poll: Poll, shared: &Shared, sink: &S) {
    // Wrap the OwnedFd in std::fs::File so we get `Read`. The fd is
    // already non-blocking; reads will return `WouldBlock` between
    // pipe-fills.
    let mut file = std::fs::File::from(owned);
    let mut events = Events::with_capacity(8);
    let mut buf = vec![0u8; READ_BUF_SIZE].into_boxed_slice();

    debug!("toastty-io::pty-reader started");

    let close = || {
        if shared.close() {
            let _ = sink.send(UserEvent::PtyReady);
        }
    };

    loop {
        if let Err(e) = poll.poll(&mut events, None) {
            if e.kind() == ErrorKind::Interrupted {
                continue;
            }
            warn!("pty-reader poll error: {e}; exiting");
            close();
            return;
        }

        for ev in &events {
            if ev.token() != PTY_TOKEN {
                continue;
            }
            // mio is edge-triggered: read until WouldBlock before polling
            // again, or the rest of the data would never raise another
            // event.
            match drain(&mut file, &mut buf, shared, sink) {
                DrainResult::Continue => {}
                DrainResult::Closed => {
                    debug!("pty-reader EOF; closing");
                    close();
                    return;
                }
                DrainResult::Detached => {
                    debug!("pty-reader consumer gone; exiting");
                    return;
                }
                DrainResult::Error(e) => {
                    warn!("pty-reader read error: {e}; closing and exiting");
                    close();
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
    /// EOF (0 bytes returned, or EIO).
    Closed,
    /// The event loop or the [`PtyReader`] has gone away.
    Detached,
    /// I/O error other than `WouldBlock` / `Interrupted`.
    Error(io::Error),
}

/// Read `file` until it would block, handing the bytes to the main thread
/// a [`READ_BUF_SIZE`] batch at a time.
fn drain<S: EventSink>(
    file: &mut std::fs::File,
    buf: &mut [u8],
    shared: &Shared,
    sink: &S,
) -> DrainResult {
    let mut filled = 0;
    loop {
        if filled == buf.len() && !flush(&buf[..filled], &mut filled, shared, sink) {
            return DrainResult::Detached;
        }
        // Back-pressure: don't start another batch while the main thread
        // is a full buffer behind.
        if filled == 0 && !shared.wait_for_space() {
            return DrainResult::Detached;
        }
        let outcome = match file.read(&mut buf[filled..]) {
            Ok(0) => DrainResult::Closed,
            Ok(n) => {
                filled += n;
                continue;
            }
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => DrainResult::Continue,
            Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
            // EIO on Linux when the PTY slave is closed; treat as EOF.
            Err(e) if is_eio(&e) => DrainResult::Closed,
            Err(e) => DrainResult::Error(e),
        };
        // Whatever ends the drain, deliver what was read first.
        if !flush(&buf[..filled], &mut filled, shared, sink) {
            return DrainResult::Detached;
        }
        return outcome;
    }
}

/// Hand `bytes` (the reader's first `*filled` bytes) to the main thread
/// and post a wake if none is in flight. Returns `false` if the event
/// loop is gone.
fn flush<S: EventSink>(bytes: &[u8], filled: &mut usize, shared: &Shared, sink: &S) -> bool {
    *filled = 0;
    if bytes.is_empty() {
        return true;
    }
    trace!("pty-reader buffered {} bytes", bytes.len());
    !shared.push(bytes) || sink.send(UserEvent::PtyReady).is_ok()
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
        let s = format!("{:?}", UserEvent::PtyReady);
        assert!(s.contains("PtyReady"));
    }

    // ---- shared buffer / back-pressure ----------------------------------

    use std::io::Write as _;
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    /// Counts wakes and forwards them so a test can block on the next one.
    struct CountingSink {
        wakes: Arc<AtomicUsize>,
        tx: Mutex<mpsc::Sender<()>>,
    }

    impl EventSink for CountingSink {
        fn send(&self, _ev: UserEvent) -> Result<(), SinkClosed> {
            self.wakes.fetch_add(1, Ordering::SeqCst);
            let tx = self.tx.lock().map_err(|_| SinkClosed)?;
            tx.send(()).map_err(|_| SinkClosed)
        }
    }

    struct Harness {
        reader: PtyReader,
        /// The child's end; dropping it closes the reader's side.
        child: Option<UnixStream>,
        wakes: Arc<AtomicUsize>,
        rx: mpsc::Receiver<()>,
        _ours: UnixStream,
    }

    fn harness() -> Harness {
        let (ours, child) = UnixStream::pair().expect("socketpair");
        child.set_nonblocking(true).expect("nonblocking");
        let wakes = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = mpsc::channel();
        let sink = CountingSink {
            wakes: Arc::clone(&wakes),
            tx: Mutex::new(tx),
        };
        let reader = spawn_pty_reader_with_sink(ours.as_fd(), sink).expect("spawn");
        Harness {
            reader,
            child: Some(child),
            wakes,
            rx,
            _ours: ours,
        }
    }

    /// Write as much of `data` as the socket takes without blocking.
    fn write_some(child: &mut UnixStream, data: &[u8]) -> usize {
        let mut off = 0;
        while off < data.len() {
            match child.write(&data[off..]) {
                Ok(n) => off += n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => panic!("write: {e}"),
            }
        }
        off
    }

    /// Wait until `f` holds, failing the test after 5 s.
    fn wait_until(mut f: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !f() {
            assert!(Instant::now() < deadline, "timed out");
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// Everything currently buffered, consumed `step` bytes at a time.
    fn drain_all(reader: &mut PtyReader, step: usize) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let chunk = reader.peek(step);
            if chunk.is_empty() {
                return out;
            }
            assert!(chunk.len() <= step);
            out.extend_from_slice(chunk);
            let n = chunk.len();
            reader.consume(n);
        }
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn one_wake_per_take_however_many_reads() {
        let mut h = harness();
        let child = h.child.as_mut().unwrap();
        // Many separate writes while the main thread takes nothing: the
        // reader buffers them all behind a single wake.
        for i in 0..50u8 {
            child.write_all(&[i; 100]).unwrap();
            thread::sleep(Duration::from_micros(200));
        }
        h.rx.recv_timeout(Duration::from_secs(5))
            .expect("first wake");
        wait_until(|| h.reader.shared.lock().buf.len() == 5000);
        assert_eq!(h.wakes.load(Ordering::SeqCst), 1);

        let got = drain_all(&mut h.reader, 4096);
        assert_eq!(got.len(), 5000);
        assert!(
            got.chunks(100)
                .enumerate()
                .all(|(i, c)| c.iter().all(|&b| usize::from(b) == i))
        );

        // Taking the buffer re-arms the wake: the next write posts again.
        h.child.as_mut().unwrap().write_all(b"more").unwrap();
        h.rx.recv_timeout(Duration::from_secs(5))
            .expect("second wake");
        assert_eq!(h.wakes.load(Ordering::SeqCst), 2);
        assert_eq!(drain_all(&mut h.reader, 4096), b"more");
    }

    #[test]
    fn reader_stops_at_the_cap_and_the_writer_blocks() {
        let mut h = harness();
        let data = pattern(BUFFER_CAP * 4);
        let child = h.child.as_mut().unwrap();
        let mut written = 0;
        // Push until the socket stops taking bytes for a while: the reader
        // has stopped reading, so the kernel buffer stays full.
        let mut stalled_since = Instant::now();
        while stalled_since.elapsed() < Duration::from_millis(200) {
            let n = write_some(child, &data[written..]);
            if n > 0 {
                written += n;
                stalled_since = Instant::now();
            }
            assert!(
                written < data.len(),
                "the reader never applied back-pressure"
            );
            thread::sleep(Duration::from_millis(1));
        }
        let buffered = h.reader.shared.lock().buf.len();
        assert!(buffered >= BUFFER_CAP, "stopped early: {buffered}");
        assert!(
            buffered < BUFFER_CAP + READ_BUF_SIZE,
            "overshot: {buffered}"
        );
        assert_eq!(h.wakes.load(Ordering::SeqCst), 1);

        // Consuming frees space; the reader resumes and every byte arrives
        // in order.
        let mut got = Vec::new();
        while got.len() < data.len() {
            written += write_some(h.child.as_mut().unwrap(), &data[written..]);
            got.extend(drain_all(&mut h.reader, 10_000));
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(got, data);
    }

    #[test]
    fn close_is_reported_only_after_every_byte_is_consumed() {
        let mut h = harness();
        let data = pattern(100_000);
        let mut child = h.child.take().unwrap();
        child.set_nonblocking(false).unwrap();
        let writer = thread::spawn(move || {
            child.write_all(&data).unwrap();
            // Dropping `child` closes the socket: EOF on the reader's end.
        });
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let chunk = h.reader.peek(7000);
            if chunk.is_empty() {
                if h.reader.is_finished() {
                    break;
                }
                assert!(Instant::now() < deadline, "no close");
                let _ = h.rx.recv_timeout(Duration::from_millis(50));
                continue;
            }
            got.extend_from_slice(chunk);
            let n = chunk.len();
            h.reader.consume(n);
        }
        writer.join().unwrap();
        // Finished only once the last byte was handed out.
        assert_eq!(got, pattern(100_000));
    }

    #[test]
    fn partial_consume_keeps_the_rest_for_later() {
        let mut h = harness();
        h.child.as_mut().unwrap().write_all(b"hello world").unwrap();
        h.rx.recv_timeout(Duration::from_secs(5)).expect("wake");
        wait_until(|| h.reader.shared.lock().buf.len() == 11);
        assert_eq!(h.reader.peek(5), b"hello");
        h.reader.consume(5);
        assert!(h.reader.has_unconsumed());
        assert_eq!(h.reader.peek(100), b" world");
        h.reader.consume(6);
        assert!(!h.reader.has_unconsumed());
        assert!(h.reader.peek(100).is_empty());
        assert!(!h.reader.is_finished());
    }

    #[test]
    fn dropping_the_reader_releases_a_thread_blocked_on_a_full_buffer() {
        let mut h = harness();
        let data = pattern(BUFFER_CAP * 2);
        let mut written = 0;
        wait_until(|| {
            written += write_some(h.child.as_mut().unwrap(), &data[written..]);
            h.reader.shared.lock().buf.len() >= BUFFER_CAP
        });
        let handle = h.reader.take_join_handle().unwrap();
        // The thread is parked in `wait_for_space`; the socket is still
        // open, so only the detach can end it.
        drop(h.reader);
        wait_until(|| handle.is_finished());
        handle.join().unwrap();
    }
}
