//! Kitty graphics protocol implementation.
//!
//! Submodules:
//! - [`header`] — parser for the APC `<key>=<value>,...` control payload.
//! - [`reply`] — encoder for `OK` / error replies.
//! - [`placeholder`] — Unicode placeholder + diacritic decoding.
//! - [`decode`] — decoded image bytes from PNG / raw RGB / raw RGBA.

pub mod decode;
pub mod header;
pub mod placeholder;
pub mod reply;
