//! Cell, Style, and Color types.
//!
//! TODO: this is the bare-minimum representation that decision #6 calls out
//! as the memory dominator. Pack `Style` into a `u32` stylesheet ID once the
//! SGR coverage stabilises (decision #6 + open question in architecture.md).

use std::num::NonZeroU16;

#[cfg(doc)]
use crate::extra::CellExtra;
use crate::extra::CellExtraId;

/// Intern-table identifier for an OSC 8 hyperlink URL.
///
/// `NonZeroU16` exists so `Option<HyperlinkId>` niche-packs into 2 bytes
/// without growing `Cell`. 65535 distinct URLs per session is plenty.
pub type HyperlinkId = NonZeroU16;

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

/// SGR underline style (`CSI 4 m`, `CSI 21 m`, and the kitty / ITU-T
/// T.416 colon form `CSI 4:N m`).
///
/// Stored as a one-byte enum rather than more bools so further styles
/// can be added without growing [`Cell`]. The discriminants match the
/// `4:N` sub-parameter (kitty's "styled underlines" extension,
/// <https://sw.kovidgoyal.net/kitty/underlines/>), so `4:N` maps
/// directly through [`UnderlineStyle::from_sgr_subparam`].
///
/// The renderer currently draws `Single` and `Double`; `Curly`,
/// `Dotted` and `Dashed` are stored faithfully but drawn as a single
/// underline until they get dedicated geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
#[repr(u8)]
pub enum UnderlineStyle {
    /// No underline (`CSI 24 m`, `CSI 4:0 m`).
    #[default]
    None = 0,
    /// `CSI 4 m` / `CSI 4:1 m`.
    Single = 1,
    /// `CSI 21 m` / `CSI 4:2 m` (xterm, kitty, VTE: SGR 21 is doubly
    /// underlined per ECMA-48, not "bold off").
    Double = 2,
    /// `CSI 4:3 m` (undercurl).
    Curly = 3,
    /// `CSI 4:4 m`.
    Dotted = 4,
    /// `CSI 4:5 m`.
    Dashed = 5,
}

impl UnderlineStyle {
    /// Decode the `N` of `CSI 4:N m`. Unknown values return `None`
    /// (the Rust `Option`, not [`UnderlineStyle::None`]) so the caller
    /// leaves the current underline untouched, as kitty does.
    #[must_use]
    pub const fn from_sgr_subparam(n: u16) -> Option<Self> {
        Some(match n {
            0 => Self::None,
            1 => Self::Single,
            2 => Self::Double,
            3 => Self::Curly,
            4 => Self::Dotted,
            5 => Self::Dashed,
            _ => return None,
        })
    }
}

/// Per-cell rendering attribute flags.
///
/// The on/off SGR renditions are packed into one byte (`bits`, see the
/// `StyleFlags::BOLD` … `StyleFlags::HIDDEN` constants) and the
/// underline is a separate [`UnderlineStyle`] byte, so the whole thing
/// is 2 bytes. Eight separate bools plus the enum would be 9 bytes and
/// push [`Cell`] from 20 to 28 bytes; see `cell_size_documented`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct StyleFlags {
    bits: u8,
    /// Underline style (SGR 4 / 4:N / 21 / 24).
    pub underline: UnderlineStyle,
}

impl StyleFlags {
    /// SGR 1. Cleared by SGR 22.
    pub const BOLD: u8 = 1 << 0;
    /// SGR 2 (faint). Cleared by SGR 22, which also clears bold.
    pub const DIM: u8 = 1 << 1;
    /// SGR 3. Cleared by SGR 23.
    pub const ITALIC: u8 = 1 << 2;
    /// SGR 5 (slow) and SGR 6 (rapid, treated as slow). Cleared by 25.
    pub const BLINK: u8 = 1 << 3;
    /// SGR 7. Cleared by SGR 27.
    pub const REVERSE: u8 = 1 << 4;
    /// SGR 8 (conceal). Cleared by SGR 28.
    pub const HIDDEN: u8 = 1 << 5;
    /// SGR 9 (crossed-out). Cleared by SGR 29.
    pub const STRIKETHROUGH: u8 = 1 << 6;
    /// SGR 53. Cleared by SGR 55.
    pub const OVERLINE: u8 = 1 << 7;

    /// No attributes, no underline. Same as `Default`.
    pub const EMPTY: Self = Self {
        bits: 0,
        underline: UnderlineStyle::None,
    };

    /// True iff every bit in `mask` is set.
    #[inline]
    #[must_use]
    pub const fn contains(self, mask: u8) -> bool {
        self.bits & mask == mask
    }

    /// Set or clear every bit in `mask`.
    #[inline]
    pub fn set(&mut self, mask: u8, on: bool) {
        if on {
            self.bits |= mask;
        } else {
            self.bits &= !mask;
        }
    }

    /// Builder form of [`StyleFlags::set`] (`on = true`).
    #[inline]
    #[must_use]
    pub const fn with(self, mask: u8) -> Self {
        Self {
            bits: self.bits | mask,
            underline: self.underline,
        }
    }

    /// Builder: replace the underline style.
    #[inline]
    #[must_use]
    pub const fn with_underline(self, underline: UnderlineStyle) -> Self {
        Self {
            bits: self.bits,
            underline,
        }
    }

    /// Raw attribute bits (the `StyleFlags::BOLD` … constants OR-ed).
    #[inline]
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.bits
    }

    #[inline]
    #[must_use]
    pub const fn bold(self) -> bool {
        self.contains(Self::BOLD)
    }

    #[inline]
    #[must_use]
    pub const fn dim(self) -> bool {
        self.contains(Self::DIM)
    }

    #[inline]
    #[must_use]
    pub const fn italic(self) -> bool {
        self.contains(Self::ITALIC)
    }

    #[inline]
    #[must_use]
    pub const fn blink(self) -> bool {
        self.contains(Self::BLINK)
    }

    #[inline]
    #[must_use]
    pub const fn reverse(self) -> bool {
        self.contains(Self::REVERSE)
    }

    #[inline]
    #[must_use]
    pub const fn hidden(self) -> bool {
        self.contains(Self::HIDDEN)
    }

    #[inline]
    #[must_use]
    pub const fn strikethrough(self) -> bool {
        self.contains(Self::STRIKETHROUGH)
    }

    #[inline]
    #[must_use]
    pub const fn overline(self) -> bool {
        self.contains(Self::OVERLINE)
    }

    /// True for any underline style other than [`UnderlineStyle::None`].
    #[inline]
    #[must_use]
    pub const fn has_underline(self) -> bool {
        !matches!(self.underline, UnderlineStyle::None)
    }
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
        flags: StyleFlags::EMPTY,
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
    /// Intern-table id of the OSC 8 hyperlink this cell belongs to, if
    /// any. `None` when not part of a hyperlink. Resolves to a URL via
    /// `Term::hyperlink_url`. Stored as `NonZeroU16` so
    /// `Option<HyperlinkId>` niche-packs to 2 bytes.
    pub hyperlink_id: Option<HyperlinkId>,
    /// Intern-table id of this cell's [`CellExtra`]: the rest of a
    /// multi-codepoint grapheme cluster (combining marks, ZWJ emoji
    /// tails, variation selectors) or a tab-start marker. `None` for
    /// the common single-codepoint cell. Resolve via
    /// `Term::cell_extra` / `Term::grapheme_tail`. Lives in the cell
    /// (rather than a per-row side table) so every cell move — shifts,
    /// scrolls, reflow — carries it along; see `extra.rs`.
    pub extra: Option<CellExtraId>,
}

impl Cell {
    /// An empty cell with default style — used to clear regions.
    pub const BLANK: Self = Self {
        ch: ' ',
        style: Style::RESET,
        is_continuation: false,
        hyperlink_id: None,
        extra: None,
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
        assert_eq!(s.flags, StyleFlags::EMPTY);
        assert_eq!(s.flags.bits(), 0);
        assert!(!s.flags.has_underline());
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
        // We assert via dynamic values (constructed at runtime via
        // `Cell::default()`) rather than the `Cell::BLANK` const, to
        // sidestep clippy's `assertions_on_constants` warning.
        let blank = Cell::default();
        assert!(!blank.is_continuation);
    }

    #[test]
    fn style_flags_are_copy_and_eq() {
        let a = StyleFlags::EMPTY
            .with(StyleFlags::BOLD)
            .with_underline(UnderlineStyle::Single);
        let b = a;
        assert_eq!(a, b);
    }

    #[test]
    fn style_flag_bits_are_distinct_and_cover_a_byte() {
        let all = [
            StyleFlags::BOLD,
            StyleFlags::DIM,
            StyleFlags::ITALIC,
            StyleFlags::BLINK,
            StyleFlags::REVERSE,
            StyleFlags::HIDDEN,
            StyleFlags::STRIKETHROUGH,
            StyleFlags::OVERLINE,
        ];
        let mut acc = 0u8;
        for b in all {
            assert_eq!(b.count_ones(), 1);
            assert_eq!(acc & b, 0, "bit {b:#x} reused");
            acc |= b;
        }
        assert_eq!(acc, 0xFF);
    }

    #[test]
    fn style_flag_getters_and_set() {
        type Getter = (u8, fn(StyleFlags) -> bool);
        let mut f = StyleFlags::default();
        let getters: [Getter; 8] = [
            (StyleFlags::BOLD, StyleFlags::bold),
            (StyleFlags::DIM, StyleFlags::dim),
            (StyleFlags::ITALIC, StyleFlags::italic),
            (StyleFlags::BLINK, StyleFlags::blink),
            (StyleFlags::REVERSE, StyleFlags::reverse),
            (StyleFlags::HIDDEN, StyleFlags::hidden),
            (StyleFlags::STRIKETHROUGH, StyleFlags::strikethrough),
            (StyleFlags::OVERLINE, StyleFlags::overline),
        ];
        for (bit, get) in getters {
            assert!(!get(f));
            f.set(bit, true);
            assert!(get(f));
            assert!(f.contains(bit));
            assert_eq!(f.bits(), bit);
            f.set(bit, false);
            assert!(!get(f));
            assert_eq!(f, StyleFlags::EMPTY);
        }
        // Multi-bit masks set and clear together (SGR 22 uses this).
        f.set(StyleFlags::BOLD | StyleFlags::DIM, true);
        assert!(f.bold() && f.dim());
        assert!(!f.contains(StyleFlags::BOLD | StyleFlags::ITALIC));
        f.set(StyleFlags::BOLD | StyleFlags::DIM, false);
        assert_eq!(f, StyleFlags::EMPTY);
    }

    #[test]
    fn underline_style_subparams() {
        use UnderlineStyle as U;
        let want = [
            U::None,
            U::Single,
            U::Double,
            U::Curly,
            U::Dotted,
            U::Dashed,
        ];
        for (n, u) in want.into_iter().enumerate() {
            assert_eq!(U::from_sgr_subparam(n as u16), Some(u));
            assert_eq!(u as u8, n as u8, "discriminant matches 4:N");
        }
        assert_eq!(U::from_sgr_subparam(6), None);
        assert_eq!(U::from_sgr_subparam(u16::MAX), None);
        assert!(!StyleFlags::EMPTY.has_underline());
        for u in [U::Single, U::Double, U::Curly, U::Dotted, U::Dashed] {
            assert!(StyleFlags::EMPTY.with_underline(u).has_underline());
        }
    }

    #[test]
    fn style_flags_pack_into_two_bytes() {
        assert_eq!(std::mem::size_of::<UnderlineStyle>(), 1);
        assert_eq!(std::mem::size_of::<StyleFlags>(), 2);
        assert_eq!(std::mem::size_of::<Style>(), 10);
    }

    /// M10.5: `Option<HyperlinkId>` must niche-pack to 2 bytes — the
    /// whole point of using `NonZeroU16`. If a future refactor swaps in
    /// a plain `u16` (or worse, `u32`) this gate will catch it.
    #[test]
    fn option_hyperlink_id_niche_packs_to_two_bytes() {
        assert_eq!(std::mem::size_of::<Option<HyperlinkId>>(), 2);
    }

    /// M10.5: document the current `Cell` size so a future cell-layout
    /// refactor (decision #6 / stylesheet packing) intentionally
    /// chooses what to do. Updating the assertion when the layout
    /// genuinely changes is fine — the goal is to catch *accidental*
    /// bloat.
    #[test]
    fn cell_size_documented() {
        // ch (4) + style (10: fg 4 + bg 4 + flags 2) + hyperlink_id (2)
        // + extra (2) + is_continuation (1), padded to 4-byte alignment
        // = 20. Before the SGR-attrs work `StyleFlags` was four bools
        // (style = 12) and the cell was 24; packing the renditions into
        // a bit byte + an underline-style byte made room for dim,
        // blink, hidden, strike, overline and underline styles while
        // shrinking the cell. Multi-codepoint text lives in the
        // per-Term intern table so the common cell stays small.
        let s = std::mem::size_of::<Cell>();
        assert!(
            s <= 20,
            "Cell grew to {s} bytes — please check the layout (decision #6)"
        );
    }

    #[test]
    fn blank_cell_has_no_hyperlink_id() {
        assert_eq!(Cell::BLANK.hyperlink_id, None);
    }

    #[test]
    fn blank_and_default_cells_have_no_extra() {
        assert_eq!(Cell::BLANK.extra, None);
        assert_eq!(Cell::default().extra, None);
    }
}
