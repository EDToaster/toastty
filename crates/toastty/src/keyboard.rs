//! Translate a winit key press into the bytes a Unix terminal app
//! expects on its stdin.
//!
//! Implements basic xterm / VT behaviour:
//!   - Printable characters from `text` (the `text_with_all_modifiers`
//!     field; comes through `toastty_window::Event::Key.text`).
//!   - Alt as ESC-prefix on a printable char.
//!   - Named keys (Enter, Tab, arrows, function keys, …) mapped to
//!     their canonical xterm/VT escape sequences.
//!
//! Kitty keyboard / modifyOtherKeys progressive encoding is **not**
//! implemented here — see TODOs. Apps that need it will request it via
//! CSI > u, which is part of `toastty-protocols` (M6+).

use toastty_window::{LogicalKey, Modifiers, NamedKey};

/// Translate a key press into the bytes a Unix terminal app expects.
///
/// Returns `None` for keys we don't yet map (modifier keys held alone,
/// dead-key composition, the IME-only path).
#[must_use]
pub fn encode_key(
    logical: &LogicalKey,
    text: Option<&str>,
    modifiers: Modifiers,
) -> Option<Vec<u8>> {
    if let LogicalKey::Named(named) = logical {
        return encode_named(*named, modifiers);
    }

    // Character / Unidentified path: fall back to `text`. If there is no
    // text, there is no byte sequence we should emit. This is the right
    // behaviour for modifier-only key events (Shift alone, Ctrl alone…).
    let raw = text?;
    if raw.is_empty() {
        return None;
    }

    // Ctrl: when text is a single-byte printable ASCII char and Ctrl is
    // held, winit's `text_with_all_modifiers` may already give us the
    // C0 control byte (e.g. \x01 for Ctrl+A on macOS). But it's not
    // guaranteed — on some platforms / layouts we get the layout-cooked
    // character. Force the C0 mapping ourselves when Ctrl is held so
    // behaviour is uniform.
    if modifiers.contains(Modifiers::CONTROL)
        && let Some(byte) = ctrl_map(raw)
    {
        // Alt+Ctrl combos prefix ESC. Apps treat Alt+Ctrl+X as
        // ESC + Ctrl-X bytes.
        if modifiers.contains(Modifiers::ALT) {
            return Some(vec![0x1b, byte]);
        }
        return Some(vec![byte]);
    }
    // Ctrl pressed but the char doesn't map (e.g. Ctrl+`) —
    // fall through to the raw text.

    // Alt without Ctrl prefixes ESC.
    let bytes = raw.as_bytes();
    if modifiers.contains(Modifiers::ALT) {
        let mut out = Vec::with_capacity(1 + bytes.len());
        out.push(0x1b);
        out.extend_from_slice(bytes);
        return Some(out);
    }

    Some(bytes.to_vec())
}

/// Map an ASCII char to its C0 control byte (Ctrl+A → 0x01, … Ctrl+Z → 0x1A).
/// Also handles the common Ctrl+@, Ctrl+[, Ctrl+\, Ctrl+], Ctrl+^, Ctrl+_.
///
/// Returns `None` when the input isn't a single mappable ASCII byte.
fn ctrl_map(s: &str) -> Option<u8> {
    let mut chars = s.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    match c {
        'a'..='z' => Some((c as u8) - b'a' + 1),
        'A'..='Z' => Some((c as u8) - b'A' + 1),
        // Ctrl+Space and Ctrl+@ both map to NUL on most terminals.
        '@' | ' ' => Some(0x00),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        // Common single-byte controls already present in `text`.
        '\x00'..='\x1f' | '\x7f' => Some(c as u8),
        _ => None,
    }
}

/// Encode a named key.
///
/// `modifiers` is here for future xterm-style modifier-encoded CSI
/// (e.g. `CSI 1 ; 5 A` for Ctrl+Up). M5 keeps the modifier-less form;
/// see TODO below.
fn encode_named(named: NamedKey, _modifiers: Modifiers) -> Option<Vec<u8>> {
    // TODO(kitty-keyboard / modifyOtherKeys): per-modifier CSI param
    // suffixes (CSI 1 ; <Ps> <final>). M5 always emits the modifier-less
    // VT form. See `docs/decisions/window-input.md` and
    // `docs/protocols.md` (xterm keyboard).
    let bytes: &[u8] = match named {
        NamedKey::Enter => b"\r",
        NamedKey::Backspace => b"\x7f",
        NamedKey::Tab => b"\t",
        NamedKey::Escape => b"\x1b",
        NamedKey::Space => b" ",
        NamedKey::ArrowUp => b"\x1b[A",
        NamedKey::ArrowDown => b"\x1b[B",
        NamedKey::ArrowRight => b"\x1b[C",
        NamedKey::ArrowLeft => b"\x1b[D",
        NamedKey::Home => b"\x1b[H",
        NamedKey::End => b"\x1b[F",
        NamedKey::PageUp => b"\x1b[5~",
        NamedKey::PageDown => b"\x1b[6~",
        NamedKey::Delete => b"\x1b[3~",
        NamedKey::Insert => b"\x1b[2~",
        NamedKey::F(1) => b"\x1bOP",
        NamedKey::F(2) => b"\x1bOQ",
        NamedKey::F(3) => b"\x1bOR",
        NamedKey::F(4) => b"\x1bOS",
        NamedKey::F(5) => b"\x1b[15~",
        NamedKey::F(6) => b"\x1b[17~",
        NamedKey::F(7) => b"\x1b[18~",
        NamedKey::F(8) => b"\x1b[19~",
        NamedKey::F(9) => b"\x1b[20~",
        NamedKey::F(10) => b"\x1b[21~",
        NamedKey::F(11) => b"\x1b[23~",
        NamedKey::F(12) => b"\x1b[24~",
        NamedKey::F(_) | NamedKey::Other => return None,
    };
    Some(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn char_key(c: &str) -> LogicalKey {
        LogicalKey::Character(c.to_string())
    }
    fn named(n: NamedKey) -> LogicalKey {
        LogicalKey::Named(n)
    }

    // ---- character path -----------------------------------------------------

    #[test]
    fn printable_ascii_lowercase() {
        let got = encode_key(&char_key("a"), Some("a"), Modifiers::empty());
        assert_eq!(got.as_deref(), Some(&b"a"[..]));
    }

    #[test]
    fn printable_ascii_uppercase_via_shift() {
        // Shift+A: text already comes through cooked as "A".
        let got = encode_key(&char_key("a"), Some("A"), Modifiers::SHIFT);
        assert_eq!(got.as_deref(), Some(&b"A"[..]));
    }

    #[test]
    fn ctrl_a_is_soh() {
        let got = encode_key(&char_key("a"), Some("a"), Modifiers::CONTROL);
        assert_eq!(got.as_deref(), Some(&b"\x01"[..]));
    }

    #[test]
    fn ctrl_shift_a_collapses_to_soh() {
        // M5 collapses Ctrl+Shift+A to Ctrl+A bytes (kitty CSI u later
        // would differentiate). The text input here is likely "A" or "a"
        // depending on platform; both should map to 0x01.
        let got = encode_key(
            &char_key("a"),
            Some("A"),
            Modifiers::CONTROL | Modifiers::SHIFT,
        );
        assert_eq!(got.as_deref(), Some(&b"\x01"[..]));
        let got = encode_key(
            &char_key("a"),
            Some("a"),
            Modifiers::CONTROL | Modifiers::SHIFT,
        );
        assert_eq!(got.as_deref(), Some(&b"\x01"[..]));
    }

    #[test]
    fn alt_a_is_esc_prefixed() {
        let got = encode_key(&char_key("a"), Some("a"), Modifiers::ALT);
        assert_eq!(got.as_deref(), Some(&b"\x1ba"[..]));
    }

    #[test]
    fn alt_ctrl_a_is_esc_plus_soh() {
        let got = encode_key(
            &char_key("a"),
            Some("a"),
            Modifiers::ALT | Modifiers::CONTROL,
        );
        assert_eq!(got.as_deref(), Some(&b"\x1b\x01"[..]));
    }

    #[test]
    fn ctrl_at_is_nul() {
        let got = encode_key(&char_key("@"), Some("@"), Modifiers::CONTROL);
        assert_eq!(got.as_deref(), Some(&b"\x00"[..]));
    }

    #[test]
    fn ctrl_left_bracket_is_esc() {
        let got = encode_key(&char_key("["), Some("["), Modifiers::CONTROL);
        assert_eq!(got.as_deref(), Some(&b"\x1b"[..]));
    }

    #[test]
    fn ctrl_backslash_is_fs() {
        let got = encode_key(&char_key("\\"), Some("\\"), Modifiers::CONTROL);
        assert_eq!(got.as_deref(), Some(&b"\x1c"[..]));
    }

    #[test]
    fn ctrl_right_bracket_is_gs() {
        let got = encode_key(&char_key("]"), Some("]"), Modifiers::CONTROL);
        assert_eq!(got.as_deref(), Some(&b"\x1d"[..]));
    }

    #[test]
    fn ctrl_caret_is_rs() {
        let got = encode_key(&char_key("^"), Some("^"), Modifiers::CONTROL);
        assert_eq!(got.as_deref(), Some(&b"\x1e"[..]));
    }

    #[test]
    fn ctrl_underscore_is_us() {
        let got = encode_key(&char_key("_"), Some("_"), Modifiers::CONTROL);
        assert_eq!(got.as_deref(), Some(&b"\x1f"[..]));
    }

    #[test]
    fn ctrl_space_is_nul() {
        let got = encode_key(&char_key(" "), Some(" "), Modifiers::CONTROL);
        assert_eq!(got.as_deref(), Some(&b"\x00"[..]));
    }

    #[test]
    fn unmappable_ctrl_falls_through_to_text() {
        // Ctrl+` has no standard C0 mapping — we fall through to the raw text.
        let got = encode_key(&char_key("`"), Some("`"), Modifiers::CONTROL);
        assert_eq!(got.as_deref(), Some(&b"`"[..]));
    }

    #[test]
    fn multibyte_text_with_ctrl_falls_through() {
        // Non-ASCII text plus Ctrl held: we can't ctrl_map a multi-char
        // string, so fall back to text. (E.g. dead-key composed glyph.)
        let got = encode_key(&char_key("é"), Some("é"), Modifiers::CONTROL);
        assert_eq!(got.as_deref(), Some("é".as_bytes()));
    }

    #[test]
    fn empty_text_yields_none() {
        let got = encode_key(&char_key(""), Some(""), Modifiers::empty());
        assert_eq!(got, None);
    }

    #[test]
    fn no_text_yields_none() {
        // Modifier-only press (Shift alone): logical is the character but
        // no text was produced.
        let got = encode_key(&char_key("a"), None, Modifiers::empty());
        assert_eq!(got, None);
    }

    #[test]
    fn unidentified_logical_with_text_uses_text() {
        let got = encode_key(&LogicalKey::Unidentified, Some("x"), Modifiers::empty());
        assert_eq!(got.as_deref(), Some(&b"x"[..]));
    }

    #[test]
    fn ctrl_already_in_text_passes_through() {
        // Some platforms hand us the C0 byte directly when Ctrl is held.
        // ctrl_map preserves it.
        let got = encode_key(&char_key("\x01"), Some("\x01"), Modifiers::CONTROL);
        assert_eq!(got.as_deref(), Some(&b"\x01"[..]));
    }

    #[test]
    fn del_in_text_with_ctrl_preserved() {
        // DEL (\x7f) is sometimes delivered with Ctrl held.
        let got = encode_key(&char_key("\x7f"), Some("\x7f"), Modifiers::CONTROL);
        assert_eq!(got.as_deref(), Some(&b"\x7f"[..]));
    }

    // ---- named-key path -----------------------------------------------------

    #[test]
    fn enter_is_cr() {
        let got = encode_key(&named(NamedKey::Enter), None, Modifiers::empty());
        assert_eq!(got.as_deref(), Some(&b"\r"[..]));
    }

    #[test]
    fn backspace_is_del() {
        let got = encode_key(&named(NamedKey::Backspace), None, Modifiers::empty());
        assert_eq!(got.as_deref(), Some(&b"\x7f"[..]));
    }

    #[test]
    fn tab_is_ht() {
        let got = encode_key(&named(NamedKey::Tab), None, Modifiers::empty());
        assert_eq!(got.as_deref(), Some(&b"\t"[..]));
    }

    #[test]
    fn escape_is_esc() {
        let got = encode_key(&named(NamedKey::Escape), None, Modifiers::empty());
        assert_eq!(got.as_deref(), Some(&b"\x1b"[..]));
    }

    #[test]
    fn space_is_space() {
        let got = encode_key(&named(NamedKey::Space), None, Modifiers::empty());
        assert_eq!(got.as_deref(), Some(&b" "[..]));
    }

    #[test]
    fn arrows() {
        for (k, s) in [
            (NamedKey::ArrowUp, &b"\x1b[A"[..]),
            (NamedKey::ArrowDown, b"\x1b[B"),
            (NamedKey::ArrowRight, b"\x1b[C"),
            (NamedKey::ArrowLeft, b"\x1b[D"),
        ] {
            let got = encode_key(&named(k), None, Modifiers::empty());
            assert_eq!(got.as_deref(), Some(s), "arrow {k:?}");
        }
    }

    #[test]
    fn home_end() {
        let got = encode_key(&named(NamedKey::Home), None, Modifiers::empty());
        assert_eq!(got.as_deref(), Some(&b"\x1b[H"[..]));
        let got = encode_key(&named(NamedKey::End), None, Modifiers::empty());
        assert_eq!(got.as_deref(), Some(&b"\x1b[F"[..]));
    }

    #[test]
    fn page_keys() {
        let got = encode_key(&named(NamedKey::PageUp), None, Modifiers::empty());
        assert_eq!(got.as_deref(), Some(&b"\x1b[5~"[..]));
        let got = encode_key(&named(NamedKey::PageDown), None, Modifiers::empty());
        assert_eq!(got.as_deref(), Some(&b"\x1b[6~"[..]));
    }

    #[test]
    fn delete_insert() {
        let got = encode_key(&named(NamedKey::Delete), None, Modifiers::empty());
        assert_eq!(got.as_deref(), Some(&b"\x1b[3~"[..]));
        let got = encode_key(&named(NamedKey::Insert), None, Modifiers::empty());
        assert_eq!(got.as_deref(), Some(&b"\x1b[2~"[..]));
    }

    #[test]
    fn function_keys_f1_to_f4() {
        for (n, s) in [
            (1u8, &b"\x1bOP"[..]),
            (2, b"\x1bOQ"),
            (3, b"\x1bOR"),
            (4, b"\x1bOS"),
        ] {
            let got = encode_key(&named(NamedKey::F(n)), None, Modifiers::empty());
            assert_eq!(got.as_deref(), Some(s), "F{n}");
        }
    }

    #[test]
    fn function_keys_f5_to_f12() {
        for (n, s) in [
            (5u8, &b"\x1b[15~"[..]),
            (6, b"\x1b[17~"),
            (7, b"\x1b[18~"),
            (8, b"\x1b[19~"),
            (9, b"\x1b[20~"),
            (10, b"\x1b[21~"),
            (11, b"\x1b[23~"),
            (12, b"\x1b[24~"),
        ] {
            let got = encode_key(&named(NamedKey::F(n)), None, Modifiers::empty());
            assert_eq!(got.as_deref(), Some(s), "F{n}");
        }
    }

    #[test]
    fn unmapped_function_key_returns_none() {
        let got = encode_key(&named(NamedKey::F(13)), None, Modifiers::empty());
        assert_eq!(got, None);
    }

    #[test]
    fn other_named_key_returns_none() {
        let got = encode_key(&named(NamedKey::Other), None, Modifiers::empty());
        assert_eq!(got, None);
    }

    // ---- ctrl_map directly ---------------------------------------------------

    #[test]
    fn ctrl_map_uppercase_letters() {
        assert_eq!(ctrl_map("A"), Some(0x01));
        assert_eq!(ctrl_map("Z"), Some(0x1a));
    }

    #[test]
    fn ctrl_map_rejects_multi_char() {
        assert_eq!(ctrl_map("ab"), None);
    }

    #[test]
    fn ctrl_map_rejects_unmappable() {
        assert_eq!(ctrl_map("1"), None);
        assert_eq!(ctrl_map("é"), None);
    }
}
