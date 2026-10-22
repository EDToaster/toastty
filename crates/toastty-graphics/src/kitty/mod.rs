//! Kitty graphics protocol implementation.
//!
//! Submodules:
//! - [`header`] — parser for the APC `<key>=<value>,...` control payload.
//! - [`reply`] — encoder for `OK` / error replies.
//! - [`placeholder`] — Unicode placeholder + diacritic decoding.
//! - [`decode`] — decoded image bytes from PNG / raw RGB / raw RGBA.
//! - [`handler`] — stateful dispatcher that owns chunked-upload
//!   reassembly and calls back into a [`handler::KittySink`].
//! - `medium` — `t=f` / `t=t` file reads: path policy, bounded reads,
//!   temp-file deletion.

pub mod decode;
pub mod handler;
pub mod header;
mod medium;
pub mod placeholder;
pub mod reply;
