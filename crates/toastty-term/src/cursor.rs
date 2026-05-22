//! Cursor: visible position + currently-active SGR style.

use crate::cell::Style;

/// Terminal cursor. `row` and `col` are 0-based and always clamped within
/// the visible viewport. `style` is the SGR rendition applied to characters
/// printed next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cursor {
    pub row: u16,
    pub col: u16,
    pub style: Style,
}
