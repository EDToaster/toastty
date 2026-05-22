//! Thin winit wrapper.
//!
//! Owns the platform realities winit doesn't surface cleanly:
//! Caps/Num Lock LED state, macOS dead-key routing through IME,
//! Wayland `RedrawRequested` cadence. See
//! [`docs/decisions/window-input.md`](../../docs/decisions/window-input.md).

#![forbid(unsafe_code)]
