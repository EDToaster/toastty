//! Terminal state: ring-buffer grid, scrollback, cursor, modes, damage set.
//!
//! M3 (the toastty-term milestone) implements the structural backbone:
//!   - [`Cell`], [`Style`], [`Color`] — minimum SGR rendition state.
//!   - [`Row`], [`Grid`] — ring-buffer grid (decision #6 / scrollback.md).
//!   - [`Cursor`] — visible position + active SGR.
//!   - [`Term`] — top-level state, primary + alternate screen,
//!     implements [`toastty_parser::Perform`].
//!
//! Out of scope for M3 (deliberately TODO'd):
//!   - Damage tracking (decision #7 / redraw-policy.md).
//!   - Reflow on resize (open question in scrollback.md).
//!   - OSC / hyperlinks / mode 2026 / 2027 / 2048 / kitty keyboard
//!     (those live in `toastty-protocols`).
//!   - 256-color and truecolor SGR (open question in architecture.md).
//!
//! See [`docs/decisions/scrollback.md`](../../docs/decisions/scrollback.md) and
//! [`docs/decisions/redraw-policy.md`](../../docs/decisions/redraw-policy.md).

#![forbid(unsafe_code)]

mod cell;
mod cursor;
mod damage;
mod grid;
mod term;

pub use cell::{Cell, Color, HyperlinkId, Style, StyleFlags};
pub use cursor::Cursor;
pub use damage::{Damage, RowDamage};
pub use grid::{Grid, Row};
pub use term::{
    KITTY_FLAG_DISAMBIGUATE, KITTY_FLAG_REPORT_ALL_AS_ESC, KITTY_FLAG_REPORT_ALTERNATE,
    KITTY_FLAG_REPORT_EVENTS, KITTY_FLAG_REPORT_TEXT, MouseMode, MouseProtocol, PromptMark,
    PromptMarkKind, Term,
};

/// Re-export so the renderer / window crates can refer to the runtime
/// cursor shape via `toastty_term` instead of taking a direct dep on
/// `toastty-config`.
pub use toastty_config::CursorShape;
