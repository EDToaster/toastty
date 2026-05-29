//! Emoji-vs-text presentation selection (UTS #51).
//!
//! cosmic-text's font fallback is *presentation-unaware*: when the primary
//! font lacks a glyph it walks a fixed coverage-ordered fallback chain, and
//! on macOS that chain reaches "Apple Color Emoji" before most monochrome
//! symbol fonts. So a codepoint like U+23FA (⏺ BLACK CIRCLE FOR RECORD) —
//! which is `Emoji=Yes` but `Emoji_Presentation=No`, i.e. its *default*
//! presentation is **text** — gets rendered as a color emoji even though a
//! conforming renderer (and alacritty) shows a plain monochrome circle.
//!
//! This module decides, per cluster, whether we want text or emoji
//! presentation, and — for the text case — finds a monochrome fallback face
//! to steer cosmic-text toward. Variation selectors are honored: VS16
//! (U+FE0F) forces emoji, VS15 (U+FE0E) forces text; otherwise the base
//! codepoint's `Emoji_Presentation` property is the default.
//!
//! Real emoji (`Emoji_Presentation=Yes`, ZWJ sequences, skin-tone, flags)
//! are left to cosmic-text's natural fallback, so they still render in
//! color.

use cosmic_text::fontdb;

/// Text-presentation variation selector (VS15) — forces the preceding
/// emoji-capable codepoint to its text form.
pub const VS15: char = '\u{FE0E}';
/// Emoji-presentation variation selector (VS16) — forces the preceding
/// codepoint to its color emoji form.
pub const VS16: char = '\u{FE0F}';

/// Whether `c` is worth running through presentation steering at all.
///
/// Pure ASCII is always covered by the primary monospace font and is never
/// steered, so we skip it cheaply — this keeps the scan over ordinary text
/// (the overwhelmingly common case) to a single comparison per char. Only
/// non-ASCII emoji-property codepoints and the variation selectors are
/// candidates.
#[must_use]
pub fn is_candidate(c: char) -> bool {
    (c as u32) > 0x7F && (c == VS15 || c == VS16 || unic_emoji_char::is_emoji(c))
}

/// True when a char attaches to the preceding base (variation selectors,
/// ZWJ, and combining marks). Such chars inherit the base cluster's
/// presentation/family rather than starting a new run.
#[must_use]
pub fn is_attaching(c: char) -> bool {
    c == VS15
        || c == VS16
        || c == '\u{200D}' // ZERO WIDTH JOINER
        || unicode_width::UnicodeWidthChar::width(c) == Some(0)
}

/// Does this cluster want **text** (monochrome) presentation rather than a
/// color emoji glyph?
///
/// Mirrors UTS #51: an explicit VS16 forces emoji, an explicit VS15 forces
/// text, otherwise the base codepoint's `Emoji_Presentation` property
/// decides. Non-emoji codepoints have only a text form anyway, so they are
/// reported as not-text-steered (the caller leaves them on the default
/// path).
#[must_use]
pub fn wants_text_presentation(base: char, has_vs15: bool, has_vs16: bool) -> bool {
    if has_vs16 {
        return false;
    }
    if has_vs15 {
        return true;
    }
    unic_emoji_char::is_emoji(base) && !unic_emoji_char::is_emoji_presentation(base)
}

/// Whether the face `id` is a color font (has COLR/CPAL color palettes or
/// sbix/CBDT color bitmap strikes). Apple Color Emoji uses sbix, so it is
/// detected via `color_strikes`. Used both to recognize an unwanted color
/// fallback and to exclude color faces when searching for a text fallback.
#[must_use]
pub fn face_is_color(db: &fontdb::Database, id: fontdb::ID) -> bool {
    db.with_face_data(id, |data, index| {
        swash::FontRef::from_index(data, index as usize)
            .is_some_and(|f| f.color_strikes().count() > 0 || f.color_palettes().count() > 0)
    })
    .unwrap_or(false)
}

/// Family name of the first **non-color** face in the database that covers
/// `ch`, or `None` if no monochrome font has the glyph (in which case the
/// caller leaves the cluster on the default fallback path — degrading to
/// whatever cosmic-text picks rather than guessing).
///
/// O(faces) and parses each candidate's tables, but it only runs once per
/// distinct text-presentation codepoint; callers cache the result.
#[must_use]
pub fn find_text_face(db: &fontdb::Database, ch: char) -> Option<String> {
    for face in db.faces() {
        let covers_as_text = db
            .with_face_data(face.id, |data, index| {
                let Some(font) = swash::FontRef::from_index(data, index as usize) else {
                    return false;
                };
                if font.color_strikes().count() > 0 || font.color_palettes().count() > 0 {
                    return false; // color face — not a text fallback
                }
                font.charmap().map(ch) != 0
            })
            .unwrap_or(false);
        if covers_as_text {
            return face.families.first().map(|(name, _)| name.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vs16_forces_emoji_vs15_forces_text() {
        // U+23FA defaults to text; selectors override either way.
        assert!(wants_text_presentation('\u{23FA}', false, false));
        assert!(!wants_text_presentation('\u{23FA}', false, true)); // VS16 → emoji
        assert!(wants_text_presentation('\u{23FA}', true, false)); // VS15 → text
    }

    #[test]
    fn record_circle_defaults_to_text() {
        // ⏺ U+23FA: Emoji=Yes, Emoji_Presentation=No → text by default.
        assert!(wants_text_presentation('\u{23FA}', false, false));
    }

    #[test]
    fn real_emoji_defaults_to_color() {
        // 😀 U+1F600: Emoji_Presentation=Yes → not text-steered.
        assert!(!wants_text_presentation('\u{1F600}', false, false));
        // ⌚ U+231A WATCH: Emoji_Presentation=Yes → color even though BMP.
        assert!(!wants_text_presentation('\u{231A}', false, false));
    }

    #[test]
    fn plain_text_is_not_steered() {
        // Letters/punctuation are not emoji → never steered.
        assert!(!wants_text_presentation('a', false, false));
        assert!(!wants_text_presentation('Z', false, false));
        // ASCII digits ARE Emoji=Yes (keycap bases) but the primary font
        // covers them, so although `wants_text_presentation` is true the
        // candidate filter excludes ASCII before we ever steer them.
        assert!(!is_candidate('0'));
        assert!(!is_candidate('#'));
        assert!(!is_candidate('a'));
    }

    #[test]
    fn candidate_filter_targets_nonascii_emoji() {
        assert!(is_candidate('\u{23FA}')); // ⏺
        assert!(is_candidate(VS16));
        assert!(is_candidate(VS15));
        assert!(is_candidate('\u{1F600}')); // 😀
        assert!(!is_candidate(' '));
        assert!(!is_candidate('λ')); // non-ASCII but not emoji
    }

    #[test]
    fn variation_selectors_and_combining_attach() {
        assert!(is_attaching(VS15));
        assert!(is_attaching(VS16));
        assert!(is_attaching('\u{200D}')); // ZWJ
        assert!(is_attaching('\u{0301}')); // combining acute
        assert!(!is_attaching('a'));
        assert!(!is_attaching('\u{23FA}'));
    }
}
