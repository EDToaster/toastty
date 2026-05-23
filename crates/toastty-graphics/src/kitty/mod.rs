//! Kitty graphics protocol implementation.
//!
//! Submodules:
//! - [`header`] — parser for the APC `<key>=<value>,...` control payload.
//! - [`reply`] — encoder for `OK` / error replies.

pub mod header;
pub mod reply;
