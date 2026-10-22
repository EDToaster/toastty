//! Shared test glue: a channel-backed [`EventSink`] and a helper that
//! turns the reader's wake + shared-buffer API back into "the next batch
//! of bytes, or the close", which is what these tests want to assert on.

use std::sync::Mutex;
use std::sync::mpsc;
use std::time::Duration;

use toastty_io::{EventSink, PtyReader, SinkClosed, UserEvent};

/// Test sink: forwards `UserEvent` into an mpsc channel.
pub(crate) struct ChannelSink(pub(crate) Mutex<mpsc::Sender<UserEvent>>);

impl EventSink for ChannelSink {
    fn send(&self, ev: UserEvent) -> Result<(), SinkClosed> {
        let g = self.0.lock().map_err(|_| SinkClosed)?;
        g.send(ev).map_err(|_| SinkClosed)
    }
}

/// One step of what the reader delivered.
pub(crate) enum Batch {
    Bytes(Vec<u8>),
    Closed,
}

/// Everything buffered right now, or the close once every byte is out.
/// Waits up to `timeout` for a wake when there is neither; `None` on
/// timeout.
pub(crate) fn recv_batch(
    reader: &mut PtyReader,
    rx: &mpsc::Receiver<UserEvent>,
    timeout: Duration,
) -> Option<Batch> {
    loop {
        let mut out = Vec::new();
        loop {
            let chunk = reader.peek(usize::MAX);
            if chunk.is_empty() {
                break;
            }
            out.extend_from_slice(chunk);
            let n = chunk.len();
            reader.consume(n);
        }
        if !out.is_empty() {
            return Some(Batch::Bytes(out));
        }
        if reader.is_finished() {
            return Some(Batch::Closed);
        }
        rx.recv_timeout(timeout).ok()?;
    }
}
