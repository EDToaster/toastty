//! Protocol handler registry.
//!
//! One module per protocol; each implements the `Protocol` trait and
//! registers handlers for the dispatch events it cares about. See
//! `docs/protocols.md` for the full support matrix.

#![forbid(unsafe_code)]
