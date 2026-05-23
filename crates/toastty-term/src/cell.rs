//! Cell, Style, and Color types.
//!
//! TODO: this is the bare-minimum representation that decision #6 calls out
//! as the memory dominator. Pack `Style` into a `u32` stylesheet ID once the
//! SGR coverage stabilises (decision #6 + open question in architecture.md).

/// Standard ANSI color, plus a "default" sentinel, the 256-color (xterm
/// `CSI 38;5;N m`) index, and 24-bit truecolor (xterm `CSI 38;2;R;G;B m`).
///
/// The 16 named variants are resolved by the renderer through the active
/// theme's palette. `Indexed256(0..16)` is treated identically to the named
/// variants by convention; `16..232` is the 6×6×6 RGB cube and `232..256` is
/// the 24-step grayscale ramp (the standard xterm table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    /// Use the terminal's default foreground / background.
    #[default]
    Default,
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    BrightBlack,
    BrightRed,
    BrightGreen,
    BrightYellow,
    BrightBlue,
    BrightMagenta,
    BrightCyan,
    BrightWhite,
    /// 256-color palette entry. Indices 0..16 alias the named palette, 16..232
    /// are the 6×6×6 RGB cube, 232..256 are 24 grayscale steps.
    Indexed256(u8),
    /// 24-bit truecolor in sRGB (gamma-encoded) space. The renderer converts
    /// to linear light at resolve time.
    Rgb(u8, u8, u8),
}

/// Per-cell rendering attribute flags.
///
/// `clippy::struct_excessive_bools` is suppressed here: these are a fixed,
/// orthogonal four-state SGR rendition, not a state machine in disguise.
/// The TODO above (pack `Style` into a `u32` stylesheet ID) will remove
/// this representation entirely once SGR coverage stabilises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct StyleFlags {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
}

/// SGR rendition state currently in effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub flags: StyleFlags,
}

impl Style {
    /// All SGR state reset to defaults — equivalent to `CSI 0 m`.
    pub const RESET: Self = Self {
        fg: Color::Default,
        bg: Color::Default,
        flags: StyleFlags {
            bold: false,
            italic: false,
            underline: false,
            reverse: false,
        },
    };
}

/// A single visible cell on the grid.
///
/// `is_continuation = true` marks a cell that is the second half of a
/// width-2 cluster: the previous cell holds the cluster's primary
/// codepoint, and this cell is a placeholder so the column accounting
/// stays right. The renderer skips continuation cells when emitting
/// instances; the cursor-motion logic skips over them so backspace /
/// `cursor_back` moves by the full cluster width.
///
/// TODO(cell-layout): per decision #6, fold `Style` into a packed stylesheet
/// ID (u32) and the eventual hyperlink id into a `NonZeroU16` once the SGR
/// coverage stabilises. M3 keeps the plain-Rust layout for clarity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cell {
    pub ch: char,
    pub style: Style,
    /// True iff this cell is the continuation half of a width-2 cluster.
    /// The renderer must skip continuation cells when building
    /// instances (otherwise we'd over-draw the second half of a CJK
    /// ideograph with a blank glyph).
    pub is_continuation: bool,
}

impl Cell {
    /// An empty cell with default style — used to clear regions.
    pub const BLANK: Self = Self {
        ch: ' ',
        style: Style::RESET,
        is_continuation: false,
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_cell_is_space_with_reset_style() {
        assert_eq!(Cell::BLANK.ch, ' ');
        assert_eq!(Cell::BLANK.style, Style::RESET);
    }

    #[test]
    fn default_color_is_default_variant() {
        assert_eq!(Color::default(), Color::Default);
    }

    #[test]
    fn style_reset_clears_flags_and_colors() {
        let s = Style::RESET;
        assert_eq!(s.fg, Color::Default);
        assert_eq!(s.bg, Color::Default);
        assert!(!s.flags.bold);
        assert!(!s.flags.italic);
        assert!(!s.flags.underline);
        assert!(!s.flags.reverse);
    }

    #[test]
    fn style_default_matches_reset() {
        // `Default` derives field-by-field; this guards against a future
        // change to `Color::Default`'s discriminant breaking the invariant.
        assert_eq!(Style::default(), Style::RESET);
    }

    #[test]
    fn cell_default_is_nul_with_reset_style() {
        // `Cell::default()` follows `char::default()` ('\0') rather than the
        // human-readable blank — `BLANK` is the constant we use to clear
        // regions. This test exists so future "clean up Cell::default" PRs
        // intentionally choose which behaviour to keep.
        let c = Cell::default();
        assert_eq!(c.ch, '\0');
        assert_eq!(c.style, Style::RESET);
        assert!(!c.is_continuation);
    }

    #[test]
    fn continuation_cell_default_false() {
        // The continuation marker must default to false so the M8
        // mode-2027 wide-cluster path is the only way to set it.
        assert!(!Cell::default().is_continuation);
        assert!(!Cell::BLANK.is_continuation);
    }

    #[test]
    fn style_flags_are_copy_and_eq() {
        let a = StyleFlags {
            bold: true,
            italic: false,
            underline: true,
            reverse: false,
        };
        let b = a;
        assert_eq!(a, b);
    }
}
