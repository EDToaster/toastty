//! PTY open / read / write.
//!
//! Unix-only for v1; `ConPTY` support deferred to v2. This crate owns
//! the master `OwnedFd`. The mio loop in `toastty-io` polls it, and
//! the dispatcher in `toastty-protocols` writes back to it for query
//! responses, in-band resize (mode 2048), and keyboard input.
//!
//! See [`docs/decisions/pty-event-loop.md`](../../docs/decisions/pty-event-loop.md).

#![cfg(unix)]
#![deny(unsafe_op_in_unsafe_fn)]

mod error;
mod pty;
mod spec;

pub use error::{PtyError, Result};
pub use pty::Pty;
pub use spec::{PtySpec, WinSize};
