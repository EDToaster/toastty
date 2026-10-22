//! Kitty graphics protocol implementation.
//!
//! Submodules:
//! - [`header`] — parser for the APC `<key>=<value>,...` control payload.
//! - [`reply`] — encoder for `OK` / error replies.
//! - [`placeholder`] — Unicode placeholder + diacritic decoding.
//! - `b64` — streaming per-chunk base64 decoder.
//! - [`decode`] — decoded image bytes from PNG / raw RGB / raw RGBA.
//! - [`handler`] — stateful dispatcher that owns chunked-upload
//!   reassembly and calls back into a [`handler::KittySink`].
//! - `medium` — `t=f` / `t=t` file reads: path policy, bounded reads,
//!   temp-file deletion.

mod b64;
pub mod decode;
pub mod handler;
pub mod header;
mod medium;
pub mod placeholder;
pub mod reply;
