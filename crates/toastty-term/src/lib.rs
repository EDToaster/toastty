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
mod dcs_query;
mod extra;
mod grid;
mod link;
mod reflow;
mod rich;
mod selection;
mod term;
mod viewport;

pub use cell::{Cell, Color, HyperlinkId, Style, StyleFlags, UnderlineStyle};
pub use cursor::Cursor;
pub use damage::{Damage, RowDamage};
pub use extra::{CellExtra, CellExtraId, ExtraTable};
pub use grid::{Grid, RelayoutFn, Row, RowLocation};
pub use link::{LinkHit, LinkSource, LinkSpan, MAX_SCAN_ROWS};
pub use rich::{
    Osc7700, RICH_DEFAULT_BUDGET, RICH_MAX_ROWS, RichBlock, RichBlockId, RichBlockLayout,
    RichBlockStore, RichEnv, RichImage, RichLayout, RichLayoutRequest, RichLinkSpan, RichPaint,
    RichPalette, RichParams, RichRow, RichTextRun, parse_osc_7700,
};
pub use selection::{Pos, Selection, SelectionMode};
pub use term::{
    ClipboardRequest, ClipboardTarget, ClipboardTargets, KITTY_FLAG_DISAMBIGUATE,
    KITTY_FLAG_REPORT_ALL_AS_ESC, KITTY_FLAG_REPORT_ALTERNATE, KITTY_FLAG_REPORT_EVENTS,
    KITTY_FLAG_REPORT_TEXT, MouseEncoding, MouseMode, MouseProtocol, PromptMark, PromptMarkKind,
    SecurityFlags, Term,
};
pub use viewport::{Smoothing, Viewport};

/// Re-export so the renderer / window crates can refer to the runtime
/// cursor shape via `toastty_term` instead of taking a direct dep on
/// `toastty-config`.
pub use toastty_config::CursorShape;

/// Re-exports for the image protocol layer so the renderer can refer
/// to graphics types via `toastty_term` instead of taking a direct dep
/// on `toastty-graphics`.
pub use toastty_graphics::{ImageData, ImageGrid, ImageRegistry, Placement, SrcRect};

/// Re-export the Unicode placeholder base codepoint (`U+10EEEE`) so the
/// renderer can suppress glyph emission for cells that hold it without
/// taking a direct dependency on `toastty-graphics`.
pub use toastty_graphics::kitty::placeholder::PLACEHOLDER;
