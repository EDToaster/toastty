//! `toastty` binary library — re-exports for `main.rs` plus the pure-
//! function modules that are unit-tested directly (keyboard encoder,
//! theme bridge).
//!
//! `#![forbid(unsafe_code)]` would be nice but Rust 2024 requires
//! `unsafe { std::env::set_var(...) }` for the env-mutation test on
//! `resolve_shell`. We use the narrower `deny(unsafe_op_in_unsafe_fn)`
//! and keep unsafe to a single test-only call.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod geometry;
pub mod keyboard;
pub mod shell;
pub mod theme_bridge;
