//! mio + winit event-loop bridge.
//!
//! Background mio thread polls the PTY fd; `Poll(timeout)` doubles as
//! the frame deadline. Bytes-ready posts `UserEvent::PtyReady` to winit
//! via `EventLoopProxy::send_event`. See
//! [`docs/decisions/pty-event-loop.md`](../../docs/decisions/pty-event-loop.md).

#![forbid(unsafe_code)]
