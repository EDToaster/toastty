//! Translate a winit key press into the bytes a Unix terminal app
//! expects on its stdin.
//!
//! There are two encoders:
//!
//! 1. **Legacy** (`kitty_flags == 0`): xterm / VT behaviour — printable
//!    text plus the canonical escape sequences for named keys (arrows,
//!    F-keys, etc.). Alt is ESC-prefixed.
//! 2. **Kitty progressive enhancement** (`kitty_flags != 0`): emits
//!    `CSI ... u` sequences with the active flag bits in mind. The
//!    "disambiguate" (bit 1) and "report event types" (bit 2) variants
//!    are implemented; the remaining bits (4, 8, 16) are partially
//!    supported and marked with TODOs.
//!
//! Spec: <https://sw.kovidgoyal.net/kitty/keyboard-protocol/>.

use toastty_term::{KITTY_FLAG_DISAMBIGUATE, KITTY_FLAG_REPORT_EVENTS};
use toastty_window::{KeyState, LogicalKey, Modifiers, NamedKey};

/// Translate a key press / release into the bytes a Unix terminal app
/// expects.
///
/// Arguments:
/// - `logical`: the layout-cooked logical key.
/// - `text`: the OS-cooked text (from `text_with_all_modifiers()`).
/// - `modifiers`: bitfield including `CAPS_LOCK` / `NUM_LOCK` once the
///   platform LED reader is wired.
/// - `kitty_flags`: top of the kitty progressive-enhancement stack
///   (0 == legacy behaviour).
/// - `state`: `Pressed` or `Released`. The legacy encoder ignores
///   releases; the kitty encoder reports them when bit 2 is on.
/// - `repeat`: true for auto-repeat presses; under kitty bit 2 this
///   becomes the `:2` event-type suffix.
///
/// Returns `None` for keys we don't yet map (modifier keys held alone,
/// release events without kitty's event-type flag, ...).
#[must_use]
pub fn encode_key(
    logical: &LogicalKey,
    text: Option<&str>,
    modifiers: Modifiers,
    kitty_flags: u8,
    state: KeyState,
    repeat: bool,
) -> Option<Vec<u8>> {
    if kitty_flags != 0 {
        return encode_key_kitty(logical, text, modifiers, kitty_flags, state, repeat);
    }
    // Legacy path: ignore releases. The terminal doesn't see release events
    // in xterm/VT mode.
    if state == KeyState::Released {
        return None;
    }
    encode_key_legacy(logical, text, modifiers)
}

// ---- legacy (xterm / VT) ----------------------------------------------------

fn encode_key_legacy(
    logical: &LogicalKey,
    text: Option<&str>,
    modifiers: Modifiers,
) -> Option<Vec<u8>> {
    if let LogicalKey::Named(named) = logical {
        return encode_named(*named, modifiers);
    }

    // Character / Unidentified path: fall back to `text`. If there is no
    // text, there is no byte sequence we should emit. This is the right
    // behaviour for modifier-only key events (Shift alone, Ctrl alone...).
    let raw = text?;
    if raw.is_empty() {
        return None;
    }

    // Ctrl: when text is a single-byte printable ASCII char and Ctrl is
    // held, winit's `text_with_all_modifiers` may already give us the
    // C0 control byte (e.g. \x01 for Ctrl+A on macOS). But it's not
    // guaranteed -- on some platforms / layouts we get the layout-cooked
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
    // Ctrl pressed but the char doesn't map (e.g. Ctrl+`) --
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

/// Map an ASCII char to its C0 control byte (Ctrl+A -> 0x01, ... Ctrl+Z -> 0x1A).
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

/// Encode a named key for the legacy (xterm/VT) path.
fn encode_named(named: NamedKey, _modifiers: Modifiers) -> Option<Vec<u8>> {
    // TODO(modifyOtherKeys): per-modifier CSI param suffixes
    // (CSI 1 ; <Ps> <final>) -- the kitty encoder is the real fix.
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

// ---- kitty progressive enhancement -----------------------------------------

/// Kitty modifier bitfield, *after* the spec's "+1" offset.
///
/// Bits: shift=1, alt=2, ctrl=4, super=8, hyper=16, meta=32,
/// capslock=64, numlock=128. Hyper and Meta aren't exposed by winit; we
/// leave them zero.
fn kitty_modifiers(m: Modifiers) -> u32 {
    let mut bits = 0u32;
    if m.contains(Modifiers::SHIFT) {
        bits |= 1;
    }
    if m.contains(Modifiers::ALT) {
        bits |= 2;
    }
    if m.contains(Modifiers::CONTROL) {
        bits |= 4;
    }
    if m.contains(Modifiers::SUPER) {
        bits |= 8;
    }
    if m.contains(Modifiers::CAPS_LOCK) {
        bits |= 64;
    }
    if m.contains(Modifiers::NUM_LOCK) {
        bits |= 128;
    }
    bits + 1
}

fn kitty_event_suffix(state: KeyState, repeat: bool) -> &'static str {
    match (state, repeat) {
        (KeyState::Pressed, false) => "1",
        (KeyState::Pressed, true) => "2",
        (KeyState::Released, _) => "3",
    }
}

/// Numeric codes for the subset of named keys that the kitty spec encodes
/// in `CSI <code> u` form. This is only the C0-aliased keys (Escape, Enter,
/// Tab, Backspace) and Space. Arrows, F-keys, Home/End, PageUp/PageDown,
/// and Insert/Delete are **not** in this set — per the spec they keep
/// their legacy CSI form (`CSI 1; mod A`, `CSI 5; mod ~`, etc.) even with
/// the disambiguate / report-all-as-escape flags on, and have no Private
/// Use Area codepoints assigned. See `kitty_legacy_form` for those.
fn kitty_named_keycode(named: NamedKey) -> Option<u32> {
    Some(match named {
        NamedKey::Escape => 27,
        NamedKey::Enter => 13,
        NamedKey::Tab => 9,
        NamedKey::Backspace => 127,
        NamedKey::Space => 32,
        _ => return None,
    })
}

/// Legacy-CSI form for keys that the kitty spec keeps in the `CSI ...
/// [~ABCDEFHPQS]` family rather than the `CSI ... u` family. Returns
/// `(keycode, final_byte)` where `final_byte` is one of `~ABCDEFHPQS`.
/// See <https://sw.kovidgoyal.net/kitty/keyboard-protocol/#functional-key-definitions>.
fn kitty_legacy_form(named: NamedKey) -> Option<(u32, u8)> {
    Some(match named {
        NamedKey::ArrowUp => (1, b'A'),
        NamedKey::ArrowDown => (1, b'B'),
        NamedKey::ArrowRight => (1, b'C'),
        NamedKey::ArrowLeft => (1, b'D'),
        NamedKey::Home => (1, b'H'),
        NamedKey::End => (1, b'F'),
        NamedKey::F(1) => (1, b'P'),
        NamedKey::F(2) => (1, b'Q'),
        NamedKey::F(3) => (1, b'R'),
        NamedKey::F(4) => (1, b'S'),
        NamedKey::Insert => (2, b'~'),
        NamedKey::Delete => (3, b'~'),
        NamedKey::PageUp => (5, b'~'),
        NamedKey::PageDown => (6, b'~'),
        NamedKey::F(5) => (15, b'~'),
        NamedKey::F(6) => (17, b'~'),
        NamedKey::F(7) => (18, b'~'),
        NamedKey::F(8) => (19, b'~'),
        NamedKey::F(9) => (20, b'~'),
        NamedKey::F(10) => (21, b'~'),
        NamedKey::F(11) => (23, b'~'),
        NamedKey::F(12) => (24, b'~'),
        _ => return None,
    })
}

fn encode_key_kitty(
    logical: &LogicalKey,
    text: Option<&str>,
    modifiers: Modifiers,
    flags: u8,
    state: KeyState,
    repeat: bool,
) -> Option<Vec<u8>> {
    let report_events = flags & KITTY_FLAG_REPORT_EVENTS != 0;
    let disambiguate = flags & KITTY_FLAG_DISAMBIGUATE != 0;
    // TODO(kitty-keyboard): bits 4 (alternate keys) and 8 (all keys as
    // escape codes) -- partial. We always emit CSI u for named keys
    // under disambiguate, which is close to bit 8 for them. Real bit-8
    // support would also emit CSI u for *every* keypress even when the
    // legacy path would have produced text. Skipped here to avoid
    // breaking apps that expect plain "a" bytes.
    let _ = flags & toastty_term::KITTY_FLAG_REPORT_ALL_AS_ESC;
    // TODO(kitty-keyboard): bit 16 (associated text) -- emit the
    // `text_with_all_modifiers` payload as a `:` sub-parameter after the
    // event-type field. Not wired yet; downstream apps that need it
    // (kitty-shell-integration, fancy editors) will fall back to the
    // base form gracefully.
    let _ = flags & toastty_term::KITTY_FLAG_REPORT_TEXT;
    let _ = flags & toastty_term::KITTY_FLAG_REPORT_ALTERNATE;

    // Releases are only reported when bit 2 is on.
    if state == KeyState::Released && !report_events {
        return None;
    }

    let m = kitty_modifiers(modifiers);
    let event_suffix = if report_events {
        Some(kitty_event_suffix(state, repeat))
    } else {
        None
    };

    match logical {
        LogicalKey::Named(named) => {
            // Keys with a legacy CSI letter/tilde form stay in that form
            // even under kitty progressive enhancement — that's what the
            // spec mandates and what every other terminal does. crossterm's
            // parser also expects this; sending PUA `CSI 57352 u` for Up
            // would be interpreted as a literal U+E008 character.
            if let Some((code, final_byte)) = kitty_legacy_form(*named) {
                return Some(format_kitty_legacy(code, m, event_suffix, final_byte));
            }
            let code = kitty_named_keycode(*named)?;
            Some(format_kitty(code, m, event_suffix, 'u'))
        }
        LogicalKey::Character(s) => {
            let c = s.chars().next()?;
            // Use lowercase codepoint for ASCII letters (kitty wants the
            // un-shifted base character). For other Unicode characters,
            // pass through.
            let code = u32::from(if c.is_ascii_uppercase() {
                c.to_ascii_lowercase()
            } else {
                c
            });
            // Under disambiguate-only mode, the kitty spec lists exactly
            // which modifier combos get the CSI u form for text-producing
            // keys: Esc, alt+key, ctrl+key, ctrl+alt+key, shift+alt+key.
            // Bare Shift (and the lock modifiers, which never appear in
            // that list) stay on the legacy text path — Shift+E goes on
            // the wire as the byte "E", not as "CSI 101;2 u". Otherwise
            // upstream multiplexers like zellij that strip the shift bit
            // when forwarding to non-kitty children deliver lowercase
            // letters to the inner app.
            const CSI_U_MODS: Modifiers = Modifiers::CONTROL
                .union(Modifiers::ALT)
                .union(Modifiers::SUPER);
            if disambiguate
                && !modifiers.intersects(CSI_U_MODS)
                && state == KeyState::Pressed
                && !repeat
                && let Some(t) = text
                && !t.is_empty()
            {
                return Some(t.as_bytes().to_vec());
            }
            Some(format_kitty(code, m, event_suffix, 'u'))
        }
        LogicalKey::Unidentified => text
            .filter(|t| !t.is_empty())
            .map(|t| t.as_bytes().to_vec()),
    }
}

/// Format a key in the legacy `CSI [code][;mod[:event]] <final>` form
/// used for arrows, F-keys, Home/End, PageUp/PageDown, Insert/Delete.
///
/// - Letter-final keys (A/B/C/D/H/F/P/Q/R/S): the leading code is omitted
///   when no modifier and no event-type suffix are present (`CSI A`), so
///   apps that only handle the bare legacy form still see it.
/// - Tilde-final keys: the leading code is always emitted (`CSI 5 ~`),
///   since `CSI ~` alone isn't a valid functional-key encoding.
fn format_kitty_legacy(code: u32, modifiers: u32, event_suffix: Option<&str>, final_byte: u8) -> Vec<u8> {
    let mut out: Vec<u8> = b"\x1b[".to_vec();
    let mods_default = modifiers == 1 && event_suffix.is_none();
    let is_letter_final = final_byte.is_ascii_alphabetic();
    if !is_letter_final || !mods_default {
        out.extend_from_slice(code.to_string().as_bytes());
    }
    if !mods_default {
        out.push(b';');
        out.extend_from_slice(modifiers.to_string().as_bytes());
        if let Some(suf) = event_suffix {
            out.push(b':');
            out.extend_from_slice(suf.as_bytes());
        }
    }
    out.push(final_byte);
    out
}

fn format_kitty(code: u32, modifiers: u32, event_suffix: Option<&str>, final_byte: char) -> Vec<u8> {
    // Kitty CSI u form:
    //   CSI keycode [; modifiers [: event-type]] u
    // We always include modifiers when they're non-default or when an
    // event-type suffix is present (the suffix's position requires the
    // modifier field).
    let mut out = format!("\x1b[{code}");
    if modifiers != 1 || event_suffix.is_some() {
        out.push(';');
        out.push_str(&modifiers.to_string());
    }
    if let Some(suf) = event_suffix {
        out.push(':');
        out.push_str(suf);
    }
    out.push(final_byte);
    out.into_bytes()
}

// ---- tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn char_key(c: &str) -> LogicalKey {
        LogicalKey::Character(c.to_string())
    }
    fn named(n: NamedKey) -> LogicalKey {
        LogicalKey::Named(n)
    }

    /// Shorthand: legacy-mode press, no kitty flags.
    fn enc_legacy(k: &LogicalKey, text: Option<&str>, m: Modifiers) -> Option<Vec<u8>> {
        encode_key(k, text, m, 0, KeyState::Pressed, false)
    }

    // ---- character path (legacy) ----

    #[test]
    fn printable_ascii_lowercase() {
        let got = enc_legacy(&char_key("a"), Some("a"), Modifiers::empty());
        assert_eq!(got.as_deref(), Some(&b"a"[..]));
    }

    #[test]
    fn printable_ascii_uppercase_via_shift() {
        let got = enc_legacy(&char_key("a"), Some("A"), Modifiers::SHIFT);
        assert_eq!(got.as_deref(), Some(&b"A"[..]));
    }

    #[test]
    fn ctrl_a_is_soh() {
        let got = enc_legacy(&char_key("a"), Some("a"), Modifiers::CONTROL);
        assert_eq!(got.as_deref(), Some(&b"\x01"[..]));
    }

    #[test]
    fn ctrl_shift_a_collapses_to_soh() {
        let got = enc_legacy(
            &char_key("a"),
            Some("A"),
            Modifiers::CONTROL | Modifiers::SHIFT,
        );
        assert_eq!(got.as_deref(), Some(&b"\x01"[..]));
        let got = enc_legacy(
            &char_key("a"),
            Some("a"),
            Modifiers::CONTROL | Modifiers::SHIFT,
        );
        assert_eq!(got.as_deref(), Some(&b"\x01"[..]));
    }

    #[test]
    fn alt_a_is_esc_prefixed() {
        let got = enc_legacy(&char_key("a"), Some("a"), Modifiers::ALT);
        assert_eq!(got.as_deref(), Some(&b"\x1ba"[..]));
    }

    #[test]
    fn alt_ctrl_a_is_esc_plus_soh() {
        let got = enc_legacy(
            &char_key("a"),
            Some("a"),
            Modifiers::ALT | Modifiers::CONTROL,
        );
        assert_eq!(got.as_deref(), Some(&b"\x1b\x01"[..]));
    }

    #[test]
    fn ctrl_at_is_nul() {
        let got = enc_legacy(&char_key("@"), Some("@"), Modifiers::CONTROL);
        assert_eq!(got.as_deref(), Some(&b"\x00"[..]));
    }

    #[test]
    fn ctrl_left_bracket_is_esc() {
        let got = enc_legacy(&char_key("["), Some("["), Modifiers::CONTROL);
        assert_eq!(got.as_deref(), Some(&b"\x1b"[..]));
    }

    #[test]
    fn ctrl_backslash_is_fs() {
        let got = enc_legacy(&char_key("\\"), Some("\\"), Modifiers::CONTROL);
        assert_eq!(got.as_deref(), Some(&b"\x1c"[..]));
    }

    #[test]
    fn ctrl_right_bracket_is_gs() {
        let got = enc_legacy(&char_key("]"), Some("]"), Modifiers::CONTROL);
        assert_eq!(got.as_deref(), Some(&b"\x1d"[..]));
    }

    #[test]
    fn ctrl_caret_is_rs() {
        let got = enc_legacy(&char_key("^"), Some("^"), Modifiers::CONTROL);
        assert_eq!(got.as_deref(), Some(&b"\x1e"[..]));
    }

    #[test]
    fn ctrl_underscore_is_us() {
        let got = enc_legacy(&char_key("_"), Some("_"), Modifiers::CONTROL);
        assert_eq!(got.as_deref(), Some(&b"\x1f"[..]));
    }

    #[test]
    fn ctrl_space_is_nul() {
        let got = enc_legacy(&char_key(" "), Some(" "), Modifiers::CONTROL);
        assert_eq!(got.as_deref(), Some(&b"\x00"[..]));
    }

    #[test]
    fn unmappable_ctrl_falls_through_to_text() {
        let got = enc_legacy(&char_key("`"), Some("`"), Modifiers::CONTROL);
        assert_eq!(got.as_deref(), Some(&b"`"[..]));
    }

    #[test]
    fn multibyte_text_with_ctrl_falls_through() {
        let got = enc_legacy(&char_key("é"), Some("é"), Modifiers::CONTROL);
        assert_eq!(got.as_deref(), Some("é".as_bytes()));
    }

    #[test]
    fn empty_text_yields_none() {
        let got = enc_legacy(&char_key(""), Some(""), Modifiers::empty());
        assert_eq!(got, None);
    }

    #[test]
    fn no_text_yields_none() {
        let got = enc_legacy(&char_key("a"), None, Modifiers::empty());
        assert_eq!(got, None);
    }

    #[test]
    fn unidentified_logical_with_text_uses_text() {
        let got = enc_legacy(&LogicalKey::Unidentified, Some("x"), Modifiers::empty());
        assert_eq!(got.as_deref(), Some(&b"x"[..]));
    }

    #[test]
    fn ctrl_already_in_text_passes_through() {
        let got = enc_legacy(&char_key("\x01"), Some("\x01"), Modifiers::CONTROL);
        assert_eq!(got.as_deref(), Some(&b"\x01"[..]));
    }

    #[test]
    fn del_in_text_with_ctrl_preserved() {
        let got = enc_legacy(&char_key("\x7f"), Some("\x7f"), Modifiers::CONTROL);
        assert_eq!(got.as_deref(), Some(&b"\x7f"[..]));
    }

    // ---- named-key path (legacy) ----

    #[test]
    fn enter_is_cr() {
        let got = enc_legacy(&named(NamedKey::Enter), None, Modifiers::empty());
        assert_eq!(got.as_deref(), Some(&b"\r"[..]));
    }

    #[test]
    fn backspace_is_del() {
        let got = enc_legacy(&named(NamedKey::Backspace), None, Modifiers::empty());
        assert_eq!(got.as_deref(), Some(&b"\x7f"[..]));
    }

    #[test]
    fn tab_is_ht() {
        let got = enc_legacy(&named(NamedKey::Tab), None, Modifiers::empty());
        assert_eq!(got.as_deref(), Some(&b"\t"[..]));
    }

    #[test]
    fn escape_is_esc() {
        let got = enc_legacy(&named(NamedKey::Escape), None, Modifiers::empty());
        assert_eq!(got.as_deref(), Some(&b"\x1b"[..]));
    }

    #[test]
    fn space_is_space() {
        let got = enc_legacy(&named(NamedKey::Space), None, Modifiers::empty());
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
            let got = enc_legacy(&named(k), None, Modifiers::empty());
            assert_eq!(got.as_deref(), Some(s), "arrow {k:?}");
        }
    }

    #[test]
    fn home_end() {
        let got = enc_legacy(&named(NamedKey::Home), None, Modifiers::empty());
        assert_eq!(got.as_deref(), Some(&b"\x1b[H"[..]));
        let got = enc_legacy(&named(NamedKey::End), None, Modifiers::empty());
        assert_eq!(got.as_deref(), Some(&b"\x1b[F"[..]));
    }

    #[test]
    fn page_keys() {
        let got = enc_legacy(&named(NamedKey::PageUp), None, Modifiers::empty());
        assert_eq!(got.as_deref(), Some(&b"\x1b[5~"[..]));
        let got = enc_legacy(&named(NamedKey::PageDown), None, Modifiers::empty());
        assert_eq!(got.as_deref(), Some(&b"\x1b[6~"[..]));
    }

    #[test]
    fn delete_insert() {
        let got = enc_legacy(&named(NamedKey::Delete), None, Modifiers::empty());
        assert_eq!(got.as_deref(), Some(&b"\x1b[3~"[..]));
        let got = enc_legacy(&named(NamedKey::Insert), None, Modifiers::empty());
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
            let got = enc_legacy(&named(NamedKey::F(n)), None, Modifiers::empty());
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
            let got = enc_legacy(&named(NamedKey::F(n)), None, Modifiers::empty());
            assert_eq!(got.as_deref(), Some(s), "F{n}");
        }
    }

    #[test]
    fn unmapped_function_key_returns_none() {
        let got = enc_legacy(&named(NamedKey::F(13)), None, Modifiers::empty());
        assert_eq!(got, None);
    }

    #[test]
    fn other_named_key_returns_none() {
        let got = enc_legacy(&named(NamedKey::Other), None, Modifiers::empty());
        assert_eq!(got, None);
    }

    // ---- ctrl_map directly ----

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

    // ---- legacy release events are dropped ----

    #[test]
    fn legacy_release_is_dropped() {
        let got = encode_key(
            &char_key("a"),
            Some("a"),
            Modifiers::empty(),
            0,
            KeyState::Released,
            false,
        );
        assert_eq!(got, None);
    }

    // ---- kitty mode ----

    /// Shorthand: kitty-mode encode.
    fn enc_kitty(
        k: &LogicalKey,
        text: Option<&str>,
        m: Modifiers,
        flags: u8,
        state: KeyState,
        repeat: bool,
    ) -> Option<Vec<u8>> {
        encode_key(k, text, m, flags, state, repeat)
    }

    #[test]
    fn kitty_modifiers_table() {
        // Empty modifiers -> 1 (per spec: bits + 1).
        assert_eq!(kitty_modifiers(Modifiers::empty()), 1);
        assert_eq!(kitty_modifiers(Modifiers::SHIFT), 2);
        assert_eq!(kitty_modifiers(Modifiers::ALT), 3);
        assert_eq!(kitty_modifiers(Modifiers::CONTROL), 5);
        assert_eq!(kitty_modifiers(Modifiers::SUPER), 9);
        assert_eq!(kitty_modifiers(Modifiers::CAPS_LOCK), 65);
        assert_eq!(kitty_modifiers(Modifiers::NUM_LOCK), 129);
        assert_eq!(kitty_modifiers(Modifiers::SHIFT | Modifiers::CONTROL), 6);
    }

    #[test]
    fn kitty_event_suffix_table() {
        assert_eq!(kitty_event_suffix(KeyState::Pressed, false), "1");
        assert_eq!(kitty_event_suffix(KeyState::Pressed, true), "2");
        assert_eq!(kitty_event_suffix(KeyState::Released, false), "3");
    }

    #[test]
    fn kitty_disambiguate_ctrl_shift_a_differs_from_ctrl_a() {
        // Both with disambiguate on; modifiers differ -> different
        // sequences. Ctrl+a -> modifiers 5; Ctrl+Shift+a -> 6.
        let ctrl_a = enc_kitty(
            &char_key("a"),
            Some("\x01"),
            Modifiers::CONTROL,
            1,
            KeyState::Pressed,
            false,
        );
        let ctrl_shift_a = enc_kitty(
            &char_key("a"),
            Some("\x01"),
            Modifiers::CONTROL | Modifiers::SHIFT,
            1,
            KeyState::Pressed,
            false,
        );
        assert_eq!(ctrl_a.as_deref(), Some(b"\x1b[97;5u".as_ref()));
        assert_eq!(ctrl_shift_a.as_deref(), Some(b"\x1b[97;6u".as_ref()));
        assert_ne!(ctrl_a, ctrl_shift_a);
    }

    #[test]
    fn kitty_disambiguate_plain_a_falls_back_to_text() {
        // No modifiers, disambiguate on: emit text directly so apps that
        // don't track keycodes still see "a". This matches kitty's
        // behaviour.
        let got = enc_kitty(
            &char_key("a"),
            Some("a"),
            Modifiers::empty(),
            1,
            KeyState::Pressed,
            false,
        );
        assert_eq!(got.as_deref(), Some(b"a".as_ref()));
    }

    #[test]
    fn kitty_event_types_press_release_repeat() {
        let press = enc_kitty(
            &char_key("a"),
            Some("a"),
            Modifiers::CONTROL,
            1 | 2,
            KeyState::Pressed,
            false,
        );
        let repeat = enc_kitty(
            &char_key("a"),
            Some("a"),
            Modifiers::CONTROL,
            1 | 2,
            KeyState::Pressed,
            true,
        );
        let release = enc_kitty(
            &char_key("a"),
            Some("a"),
            Modifiers::CONTROL,
            1 | 2,
            KeyState::Released,
            false,
        );
        assert_eq!(press.as_deref(), Some(b"\x1b[97;5:1u".as_ref()));
        assert_eq!(repeat.as_deref(), Some(b"\x1b[97;5:2u".as_ref()));
        assert_eq!(release.as_deref(), Some(b"\x1b[97;5:3u".as_ref()));
    }

    #[test]
    fn kitty_release_without_event_types_is_dropped() {
        let got = enc_kitty(
            &char_key("a"),
            Some("a"),
            Modifiers::CONTROL,
            1,
            KeyState::Released,
            false,
        );
        assert_eq!(got, None);
    }

    #[test]
    fn kitty_enter_uses_keycode_13() {
        let got = enc_kitty(
            &named(NamedKey::Enter),
            None,
            Modifiers::empty(),
            1,
            KeyState::Pressed,
            false,
        );
        assert_eq!(got.as_deref(), Some(b"\x1b[13u".as_ref()));
    }

    #[test]
    fn kitty_backspace_uses_keycode_127() {
        let got = enc_kitty(
            &named(NamedKey::Backspace),
            None,
            Modifiers::empty(),
            1,
            KeyState::Pressed,
            false,
        );
        assert_eq!(got.as_deref(), Some(b"\x1b[127u".as_ref()));
    }

    #[test]
    fn kitty_arrows_use_legacy_csi_form() {
        // Per the kitty spec, the main arrow keys keep their legacy CSI
        // form (`CSI A` etc.) under progressive enhancement — they have no
        // PUA codepoint assigned. The keypad arrows (KP_*) are separate
        // keys at codepoints 57417-57420.
        for (k, final_byte) in [
            (NamedKey::ArrowUp, b'A'),
            (NamedKey::ArrowDown, b'B'),
            (NamedKey::ArrowRight, b'C'),
            (NamedKey::ArrowLeft, b'D'),
        ] {
            let got = enc_kitty(
                &named(k),
                None,
                Modifiers::empty(),
                1,
                KeyState::Pressed,
                false,
            );
            let want = [b'\x1b', b'[', final_byte];
            assert_eq!(got.as_deref(), Some(&want[..]), "arrow {k:?}");
        }
    }

    #[test]
    fn kitty_ctrl_arrow_uses_xterm_modifier_form() {
        // Ctrl+Up under kitty mode: `CSI 1; 5 A`, not the PUA u form.
        let got = enc_kitty(
            &named(NamedKey::ArrowUp),
            None,
            Modifiers::CONTROL,
            1,
            KeyState::Pressed,
            false,
        );
        assert_eq!(got.as_deref(), Some(b"\x1b[1;5A".as_ref()));
    }

    #[test]
    fn kitty_arrow_with_event_suffix_emits_keycode_and_default_modifier() {
        // Up release with no modifiers but event reporting on: `CSI 1; 1: 3 A`.
        let got = enc_kitty(
            &named(NamedKey::ArrowUp),
            None,
            Modifiers::empty(),
            1 | 2,
            KeyState::Released,
            false,
        );
        assert_eq!(got.as_deref(), Some(b"\x1b[1;1:3A".as_ref()));
    }

    #[test]
    fn kitty_home_end() {
        let home = enc_kitty(
            &named(NamedKey::Home),
            None,
            Modifiers::empty(),
            1,
            KeyState::Pressed,
            false,
        );
        assert_eq!(home.as_deref(), Some(b"\x1b[H".as_ref()));
        let end = enc_kitty(
            &named(NamedKey::End),
            None,
            Modifiers::empty(),
            1,
            KeyState::Pressed,
            false,
        );
        assert_eq!(end.as_deref(), Some(b"\x1b[F".as_ref()));
    }

    #[test]
    fn kitty_tilde_form_keys() {
        // Tilde-final keys always carry their keycode, even unmodified.
        for (k, code) in [
            (NamedKey::Insert, 2),
            (NamedKey::Delete, 3),
            (NamedKey::PageUp, 5),
            (NamedKey::PageDown, 6),
        ] {
            let got = enc_kitty(
                &named(k),
                None,
                Modifiers::empty(),
                1,
                KeyState::Pressed,
                false,
            );
            let want = format!("\x1b[{code}~");
            assert_eq!(got.as_deref(), Some(want.as_bytes()), "tilde {k:?}");
        }
    }

    #[test]
    fn kitty_ctrl_pageup_includes_modifier() {
        // `CSI 5; 5 ~` — keycode + modifier, tilde final.
        let got = enc_kitty(
            &named(NamedKey::PageUp),
            None,
            Modifiers::CONTROL,
            1,
            KeyState::Pressed,
            false,
        );
        assert_eq!(got.as_deref(), Some(b"\x1b[5;5~".as_ref()));
    }

    #[test]
    fn kitty_f_keys() {
        // F1-F4 use the `1 P/Q/R/S` letter-final form; F5-F12 use the
        // tilde-final form with the conventional keycodes (15, 17-24).
        let f1 = enc_kitty(
            &named(NamedKey::F(1)),
            None,
            Modifiers::empty(),
            1,
            KeyState::Pressed,
            false,
        );
        assert_eq!(f1.as_deref(), Some(b"\x1b[P".as_ref()));
        let f4 = enc_kitty(
            &named(NamedKey::F(4)),
            None,
            Modifiers::empty(),
            1,
            KeyState::Pressed,
            false,
        );
        assert_eq!(f4.as_deref(), Some(b"\x1b[S".as_ref()));
        let f5 = enc_kitty(
            &named(NamedKey::F(5)),
            None,
            Modifiers::empty(),
            1,
            KeyState::Pressed,
            false,
        );
        assert_eq!(f5.as_deref(), Some(b"\x1b[15~".as_ref()));
        let f12 = enc_kitty(
            &named(NamedKey::F(12)),
            None,
            Modifiers::empty(),
            1,
            KeyState::Pressed,
            false,
        );
        assert_eq!(f12.as_deref(), Some(b"\x1b[24~".as_ref()));
    }

    #[test]
    fn kitty_ctrl_f1_uses_modifier_form() {
        let got = enc_kitty(
            &named(NamedKey::F(1)),
            None,
            Modifiers::CONTROL,
            1,
            KeyState::Pressed,
            false,
        );
        assert_eq!(got.as_deref(), Some(b"\x1b[1;5P".as_ref()));
    }

    #[test]
    fn kitty_unmapped_named_key_returns_none() {
        let got = enc_kitty(
            &named(NamedKey::Other),
            None,
            Modifiers::empty(),
            1,
            KeyState::Pressed,
            false,
        );
        assert_eq!(got, None);
    }

    #[test]
    fn kitty_capslock_modifier_bit_set() {
        let got = enc_kitty(
            &char_key("a"),
            Some("a"),
            Modifiers::CONTROL | Modifiers::CAPS_LOCK,
            1,
            KeyState::Pressed,
            false,
        );
        // Modifiers: CONTROL (4) + CAPS_LOCK (64) + 1 = 69.
        assert_eq!(got.as_deref(), Some(b"\x1b[97;69u".as_ref()));
    }

    #[test]
    fn kitty_numlock_modifier_bit_set() {
        let got = enc_kitty(
            &char_key("a"),
            Some("a"),
            Modifiers::CONTROL | Modifiers::NUM_LOCK,
            1,
            KeyState::Pressed,
            false,
        );
        // Modifiers: CONTROL (4) + NUM_LOCK (128) + 1 = 133.
        assert_eq!(got.as_deref(), Some(b"\x1b[97;133u".as_ref()));
    }

    #[test]
    fn kitty_disambiguate_shift_letter_emits_plain_text() {
        // Per spec, bare Shift+printable stays on the legacy text path
        // under disambiguate-only mode. Shift+A => byte "A", not the
        // "CSI 97;2 u" form (that would lose the shift bit when a
        // non-kitty consumer like zellij forwards to a child).
        let got = enc_kitty(
            &char_key("A"),
            Some("A"),
            Modifiers::SHIFT,
            1,
            KeyState::Pressed,
            false,
        );
        assert_eq!(got.as_deref(), Some(b"A".as_ref()));
    }

    #[test]
    fn kitty_disambiguate_shift_punctuation_emits_plain_text() {
        // Same rule applies to shifted symbols: Shift+1 => "!", not
        // "CSI 33;2 u". The shifted glyph is whatever the OS hands us
        // in `text`.
        let got = enc_kitty(
            &char_key("1"),
            Some("!"),
            Modifiers::SHIFT,
            1,
            KeyState::Pressed,
            false,
        );
        assert_eq!(got.as_deref(), Some(b"!".as_ref()));
        let got = enc_kitty(
            &char_key(";"),
            Some(":"),
            Modifiers::SHIFT,
            1,
            KeyState::Pressed,
            false,
        );
        assert_eq!(got.as_deref(), Some(b":".as_ref()));
    }

    #[test]
    fn kitty_disambiguate_caps_or_num_lock_alone_still_text() {
        // Lock modifiers aren't in the disambiguate CSI-u list.
        let got = enc_kitty(
            &char_key("A"),
            Some("A"),
            Modifiers::CAPS_LOCK,
            1,
            KeyState::Pressed,
            false,
        );
        assert_eq!(got.as_deref(), Some(b"A".as_ref()));
        let got = enc_kitty(
            &char_key("1"),
            Some("1"),
            Modifiers::NUM_LOCK,
            1,
            KeyState::Pressed,
            false,
        );
        assert_eq!(got.as_deref(), Some(b"1".as_ref()));
    }

    #[test]
    fn kitty_disambiguate_alt_letter_still_csi_u() {
        // Alt+E is in the disambiguate CSI-u list.
        let got = enc_kitty(
            &char_key("e"),
            Some("e"),
            Modifiers::ALT,
            1,
            KeyState::Pressed,
            false,
        );
        assert_eq!(got.as_deref(), Some(b"\x1b[101;3u".as_ref()));
    }

    #[test]
    fn kitty_disambiguate_super_letter_still_csi_u() {
        // Super (Cmd on macOS) should also force CSI u so apps can
        // distinguish Cmd+E from a bare "E" typed by the user.
        let got = enc_kitty(
            &char_key("e"),
            Some("e"),
            Modifiers::SUPER,
            1,
            KeyState::Pressed,
            false,
        );
        assert_eq!(got.as_deref(), Some(b"\x1b[101;9u".as_ref()));
    }

    #[test]
    fn kitty_disambiguate_shift_alt_letter_still_csi_u() {
        // Per spec, shift+alt+key IS in the CSI-u list — shift alone
        // isn't, but combined with Alt it is.
        let got = enc_kitty(
            &char_key("e"),
            Some("E"),
            Modifiers::SHIFT | Modifiers::ALT,
            1,
            KeyState::Pressed,
            false,
        );
        assert_eq!(got.as_deref(), Some(b"\x1b[101;4u".as_ref()));
    }

    #[test]
    fn kitty_unicode_codepoint_with_modifier() {
        // With a modifier, we emit the codepoint form.
        let got = enc_kitty(
            &char_key("é"),
            Some("é"),
            Modifiers::CONTROL,
            1,
            KeyState::Pressed,
            false,
        );
        assert_eq!(got.as_deref(), Some(b"\x1b[233;5u".as_ref()));
    }

    #[test]
    fn kitty_unicode_without_modifier_falls_back_to_text() {
        // A literal "é" under disambiguate with no modifiers -> we still
        // emit text (fast-path).
        let got = enc_kitty(
            &char_key("é"),
            Some("é"),
            Modifiers::empty(),
            1,
            KeyState::Pressed,
            false,
        );
        assert_eq!(got.as_deref(), Some("é".as_bytes()));
    }

    #[test]
    fn kitty_unidentified_uses_text() {
        let got = enc_kitty(
            &LogicalKey::Unidentified,
            Some("x"),
            Modifiers::empty(),
            1,
            KeyState::Pressed,
            false,
        );
        assert_eq!(got.as_deref(), Some(b"x".as_ref()));
    }

    #[test]
    fn kitty_unidentified_without_text_returns_none() {
        let got = enc_kitty(
            &LogicalKey::Unidentified,
            None,
            Modifiers::empty(),
            1,
            KeyState::Pressed,
            false,
        );
        assert_eq!(got, None);
    }

    #[test]
    fn format_kitty_omits_modifier_when_default_and_no_event_suffix() {
        let bytes = format_kitty(97, 1, None, 'u');
        assert_eq!(bytes, b"\x1b[97u".to_vec());
    }

    #[test]
    fn format_kitty_includes_event_suffix_when_present() {
        let bytes = format_kitty(97, 1, Some("1"), 'u');
        // When event-type is present, modifiers must be emitted too.
        assert_eq!(bytes, b"\x1b[97;1:1u".to_vec());
    }
}
