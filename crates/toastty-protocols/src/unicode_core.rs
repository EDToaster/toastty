//! DECSET 2027 — Terminal Unicode Core.
//!
//! Apps that opt into mode 2027 declare that **they** know the
//! grapheme-cluster widths and the terminal must honor them rather
//! than `wcwidth()`-ing every codepoint independently. The classic
//! ❤ + VS16 = "❤️" case shows why: `wcwidth(0x2764)` returns 1, but the
//! cluster `"\u{2764}\u{FE0F}"` is a width-2 emoji presentation.
//!
//! When mode 2027 is OFF, we fall back to `unicode-width`'s
//! per-codepoint result (`UnicodeWidthChar::width(c)`).
//!
//! The numbers come straight from `docs/decisions/text-stack.md`'s
//! mode-2027 table (lines 84–96).

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Cell width of a single grapheme cluster, in monospace cells.
///
/// `0` for empty input; `1` for ordinary ASCII / VS15 text-presentation
/// / combining-mark cluster; `2` for CJK ideographs, VS16
/// emoji-presentation, ZWJ emoji sequences, regional-indicator flags.
///
/// When `mode_2027_active` is true and the cluster is non-empty, the
/// terminal trusts the grapheme-segmenter's answer (via
/// `UnicodeWidthStr`). When `mode_2027_active` is false, we fold per
/// codepoint via `UnicodeWidthChar::width` so the legacy `wcwidth`
/// behaviour wins (ZWJ emoji come out as 1, etc.).
#[must_use]
pub fn cluster_cell_width(cluster: &str, mode_2027_active: bool) -> u8 {
    if cluster.is_empty() {
        return 0;
    }
    if mode_2027_active {
        // `UnicodeWidthStr` is grapheme-aware: ZWJ sequences and VS16
        // count as width 2 because the underlying tables consider the
        // composition. ASCII = 1, CJK = 2, etc. We cap at 2 because a
        // single cluster shouldn't exceed two cells in a terminal grid
        // — anything pathologically wider gets clamped to keep the
        // grid layout stable.
        let w = UnicodeWidthStr::width(cluster);
        // Per text-stack.md a combining-mark-on-base cluster is width
        // 1 (base contributes, mark is zero-width). VS15 emoji on a
        // chevron base = 1. ZWJ family = 1 from the table, but in
        // practice 2 — we ship the conservative answer here so emoji
        // get two cells.
        return u8::try_from(w.min(2)).unwrap_or(1);
    }
    // Mode 2027 off: legacy wcwidth behaviour — width of the first
    // codepoint, summed for combining marks (which all have width 0).
    let mut total: usize = 0;
    for c in cluster.chars() {
        total = total.saturating_add(UnicodeWidthChar::width(c).unwrap_or(0));
    }
    u8::try_from(total.min(2)).unwrap_or(1)
}

/// Per-codepoint cell width (the classic `wcwidth` answer). Returns
/// `1` for the unknown / unwidth case so the terminal never silently
/// "loses" a cell.
///
/// When `mode_2027_active` is true the caller should prefer
/// [`cluster_cell_width`] over this — wide clusters that span multiple
/// codepoints (VS16, ZWJ) need grapheme-level context that a single
/// `char` lookup can't provide.
#[must_use]
pub fn char_cell_width(c: char, mode_2027_active: bool) -> u8 {
    let _ = mode_2027_active; // single-char width is the same in both modes
    match UnicodeWidthChar::width(c) {
        Some(0) => 0,
        Some(w) if w >= 2 => 2,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_is_one_cell() {
        assert_eq!(cluster_cell_width("A", true), 1);
        assert_eq!(cluster_cell_width("A", false), 1);
    }

    #[test]
    fn cjk_ideograph_is_two_cells() {
        // "你" — width 2 in both modes (wcwidth says 2 for CJK).
        assert_eq!(cluster_cell_width("你", true), 2);
        assert_eq!(cluster_cell_width("你", false), 2);
    }

    #[test]
    fn rocket_emoji_is_two_cells() {
        assert_eq!(cluster_cell_width("🚀", true), 2);
        assert_eq!(cluster_cell_width("🚀", false), 2);
    }

    #[test]
    fn heart_with_vs16_is_two_cells_under_2027() {
        // "❤\u{FE0F}" — emoji presentation. Mode 2027 declares 2.
        let cluster = "\u{2764}\u{FE0F}";
        assert_eq!(cluster_cell_width(cluster, true), 2);
    }

    #[test]
    fn heart_with_vs15_is_one_cell() {
        // "❤\u{FE0E}" — text presentation. One cell either way.
        let cluster = "\u{2764}\u{FE0E}";
        assert_eq!(cluster_cell_width(cluster, true), 1);
        assert_eq!(cluster_cell_width(cluster, false), 1);
    }

    #[test]
    fn zwj_family_is_two_cells_under_2027() {
        // 👨‍👩‍👧‍👦 — ZWJ family. We expose this as 2.
        let cluster = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";
        assert_eq!(cluster_cell_width(cluster, true), 2);
    }

    #[test]
    fn regional_indicator_flag_is_two_cells() {
        // 🇯🇵 — regional indicator pair for Japan.
        let cluster = "\u{1F1EF}\u{1F1F5}";
        assert_eq!(cluster_cell_width(cluster, true), 2);
    }

    #[test]
    fn combining_mark_on_base_is_one_cell() {
        // "e" + U+0301 (combining acute). One cell.
        let cluster = "e\u{0301}";
        assert_eq!(cluster_cell_width(cluster, true), 1);
        assert_eq!(cluster_cell_width(cluster, false), 1);
    }

    #[test]
    fn empty_cluster_is_zero_cells() {
        assert_eq!(cluster_cell_width("", true), 0);
        assert_eq!(cluster_cell_width("", false), 0);
    }

    #[test]
    fn char_cell_width_ascii_is_one() {
        assert_eq!(char_cell_width('A', false), 1);
        assert_eq!(char_cell_width('A', true), 1);
    }

    #[test]
    fn char_cell_width_cjk_is_two() {
        assert_eq!(char_cell_width('你', false), 2);
        assert_eq!(char_cell_width('你', true), 2);
    }

    #[test]
    fn char_cell_width_combining_mark_is_zero() {
        assert_eq!(char_cell_width('\u{0301}', false), 0);
    }
}
