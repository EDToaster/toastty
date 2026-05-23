//! Kitty graphics protocol implementation.
//!
//! Submodules:
//! - [`header`] — parser for the APC `<key>=<value>,...` control payload.
//! - [`reply`] — encoder for `OK` / error replies.
//! - [`placeholder`] — Unicode placeholder + diacritic decoding.

pub mod header;
pub mod placeholder;
pub mod reply;
