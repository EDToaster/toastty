//! Terminal escape sequence parser.
//!
//! Wraps `vte` for C0/CSI/OSC/DCS/ESC dispatch, and ships our own
//! streaming APC scanner because `vte 0.15` silently discards APC
//! payloads (no `apc_dispatch` hook exists).
//!
//! Handlers implement the [`Perform`] trait; APC handling is split
//! into [`Perform::apc_start`], [`Perform::apc_chunk`], and
//! [`Perform::apc_end`] so large payloads (e.g. a 50 MB RGP `.glb`)
//! don't have to be buffered before dispatch.
//!
//! See [`docs/decisions/streaming-apc.md`](../../docs/decisions/streaming-apc.md).

#![forbid(unsafe_code)]

mod parser;
mod perform;

pub use parser::Parser;
pub use perform::{BufferingApcHandler, Perform};
pub use vte::Params;
