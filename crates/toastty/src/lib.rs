//! `toastty` binary library — re-exports for `main.rs` plus the pure-
//! function modules that are unit-tested directly (keyboard encoder,
//! theme bridge).
//!
//! `#![forbid(unsafe_code)]` would be nice but Rust 2024 requires
//! `unsafe { std::env::set_var(...) }` for the env-mutation test on
//! `resolve_shell`. We use the narrower `deny(unsafe_op_in_unsafe_fn)`
//! and keep unsafe to a single test-only call.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod child_env;
pub mod cli;
pub mod clipboard;
pub mod config_watcher;
pub mod focus;
pub mod font_zoom;
pub mod geometry;
pub mod heap;
pub mod image_cmd;
pub mod ingest;
pub mod keyboard;
pub mod links;
pub mod markdown;
pub mod md_cmd;
pub mod mouse;
pub mod paste;
pub mod perf_stats;
pub mod preload;
pub mod pty_log;
pub mod shell;
pub mod theme_bridge;
