//! Translate a winit key press into the bytes a Unix terminal app
//! expects on its stdin.
//!
//! There are two encoders:
//!
//! 1. **Legacy** (`kitty_flags == 0`): xterm / VT behaviour — printable
//!    text plus the canonical escape sequences for named keys (arrows,
//!    F-keys, etc.). Alt is ESC-prefixed.
//! 2. **Kitty progressive enhancement** (`kitty_flags != 0`): emits
//!    `CSI ... u` sequences with the active flag bits in mind, matching
//!    kitty 0.49.2 (see `encode_key_kitty`). Not yet covered: the
//!    keypad's own key codes and modifier-key events under report-all,
//!    which need the physical key.
//!
//! Spec: <https://sw.kovidgoyal.net/kitty/keyboard-protocol/>.

use toastty_term::{
    KITTY_FLAG_DISAMBIGUATE, KITTY_FLAG_REPORT_ALL_AS_ESC, KITTY_FLAG_REPORT_ALTERNATE,
    KITTY_FLAG_REPORT_EVENTS, KITTY_FLAG_REPORT_TEXT,
};
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
    encode_key_with_modes(
        logical,
        text,
        modifiers,
        kitty_flags,
        state,
        repeat,
        LegacyModes::default(),
    )
}

/// Terminal modes that change the legacy (non-kitty) key encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegacyModes {
    /// DECCKM (`CSI ? 1 h`): unmodified arrows / Home / End send
    /// `SS3 A` .. instead of `CSI A` ...
    pub app_cursor: bool,
    /// `?1036` / `?1039`: Alt-modified keys are ESC-prefixed. When off,
    /// Alt is dropped and the key's text is sent as-is.
    pub alt_escape: bool,
}

impl Default for LegacyModes {
    fn default() -> Self {
        Self {
            app_cursor: false,
            alt_escape: true,
        }
    }
}

/// [`encode_key`] with the legacy-encoding modes the terminal has set.
#[must_use]
pub fn encode_key_with_modes(
    logical: &LogicalKey,
    text: Option<&str>,
    modifiers: Modifiers,
    kitty_flags: u8,
    state: KeyState,
    repeat: bool,
    modes: LegacyModes,
) -> Option<Vec<u8>> {
    // kitty treats report-alternates (4) alone, and report-text (16)
    // without report-all (8, which the spec says it requires), as the
    // legacy encoding: neither changes a key that isn't already an
    // escape code. Checked against kitty 0.49.2.
    if kitty_flags
        & (KITTY_FLAG_DISAMBIGUATE | KITTY_FLAG_REPORT_EVENTS | KITTY_FLAG_REPORT_ALL_AS_ESC)
        != 0
    {
        return encode_key_kitty(logical, text, modifiers, kitty_flags, state, repeat, modes);
    }
    // Legacy path: ignore releases. The terminal doesn't see release events
    // in xterm/VT mode.
    if state == KeyState::Released {
        return None;
    }
    encode_key_legacy(logical, text, modifiers, modes)
}

/// Application-keypad encoding (DECKPAM / DECNKM) for a numeric-keypad
/// key, or `None` when the key should go through the normal encoder.
///
/// Follows xterm with its default `numLock: true` resource: while
/// `NumLock` is on the keypad types digits / operators as usual; with
/// `NumLock` off it sends the VT `SS3` application codes (`SS3 p`..`SS3 y`
/// for 0-9, `SS3 M` for Enter, ...). The digit keys report `NumLock`
/// through their logical key (a digit vs. a navigation key);
/// `numlock_on` covers the operator keys, which look the same either
/// way. Modified keys (Ctrl / Alt / Super) fall through.
#[must_use]
pub fn encode_app_keypad(
    physical_code: &str,
    logical: &LogicalKey,
    modifiers: Modifiers,
    numlock_on: bool,
) -> Option<Vec<u8>> {
    if modifiers.intersects(Modifiers::CONTROL | Modifiers::ALT | Modifiers::SUPER) {
        return None;
    }
    let key = physical_code.strip_prefix("Numpad")?;
    let digit_like = matches!(key.as_bytes(), [b'0'..=b'9']) || key == "Decimal";
    let numlock_on = if digit_like {
        matches!(logical, LogicalKey::Character(_))
    } else {
        numlock_on
    };
    if numlock_on {
        return None;
    }
    let fin = match key {
        "Enter" => b'M',
        "Multiply" => b'j',
        "Add" => b'k',
        "Comma" => b'l',
        "Subtract" => b'm',
        "Decimal" => b'n',
        "Divide" => b'o',
        "Equal" => b'X',
        d if digit_like => b'p' + (d.as_bytes()[0] - b'0'),
        _ => return None,
    };
    Some(vec![0x1b, b'O', fin])
}

/// `NumLock` state implied by a numeric-keypad *digit* key: `Some(true)`
/// when it typed a digit, `Some(false)` when it acted as a navigation
/// key, `None` for any other key. The binary remembers the last answer
/// to decide how the operator keys behave under DECKPAM.
#[must_use]
pub fn numlock_hint(physical_code: &str, logical: &LogicalKey) -> Option<bool> {
    let key = physical_code.strip_prefix("Numpad")?;
    if matches!(key.as_bytes(), [b'0'..=b'9']) || key == "Decimal" {
        Some(matches!(logical, LogicalKey::Character(_)))
    } else {
        None
    }
}

// ---- legacy (xterm / VT) ----------------------------------------------------

fn encode_key_legacy(
    logical: &LogicalKey,
    text: Option<&str>,
    modifiers: Modifiers,
    modes: LegacyModes,
) -> Option<Vec<u8>> {
    if let LogicalKey::Named(named) = logical {
        return encode_named(*named, modifiers, modes);
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
        if modifiers.contains(Modifiers::ALT) && modes.alt_escape {
            return Some(vec![0x1b, byte]);
        }
        return Some(vec![byte]);
    }
    // Ctrl pressed but the char doesn't map (e.g. Ctrl+`) --
    // fall through to the raw text.

    // Alt without Ctrl prefixes ESC (unless the app reset ?1036/?1039).
    let bytes = raw.as_bytes();
    if modifiers.contains(Modifiers::ALT) && modes.alt_escape {
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
fn encode_named(named: NamedKey, modifiers: Modifiers, modes: LegacyModes) -> Option<Vec<u8>> {
    // xterm-style modifier parameter (`CSI 1 ; <mods> <final>`), the same
    // bits as kitty's: shift 1, alt 2, ctrl 4, super 8, plus 1. Lock
    // modifiers aren't reported in legacy mode.
    let shift = modifiers.contains(Modifiers::SHIFT);
    let alt = modifiers.contains(Modifiers::ALT);
    let ctrl = modifiers.contains(Modifiers::CONTROL);
    let plain = !modifiers
        .intersects(Modifiers::SHIFT | Modifiers::CONTROL | Modifiers::ALT | Modifiers::SUPER);
    let mod_param = legacy_modifier_param(modifiers);
    // Alt prefixes ESC to the C0-style keys (unless ?1036 / ?1039 are
    // reset), as kitty and xterm do.
    let esc_prefix = |bytes: &[u8]| -> Vec<u8> {
        if alt && modes.alt_escape {
            let mut v = vec![0x1b];
            v.extend_from_slice(bytes);
            v
        } else {
            bytes.to_vec()
        }
    };
    match named {
        NamedKey::Enter => return Some(esc_prefix(b"\r")),
        // Shift+Tab is the back-tab `CSI Z` (zsh / readline reverse
        // completion).
        NamedKey::Tab if shift => return Some(esc_prefix(b"\x1b[Z")),
        NamedKey::Tab => return Some(esc_prefix(b"\t")),
        NamedKey::Backspace if ctrl => return Some(esc_prefix(b"\x08")),
        NamedKey::Backspace => return Some(esc_prefix(b"\x7f")),
        NamedKey::Escape => return Some(esc_prefix(b"\x1b")),
        NamedKey::Space if ctrl => return Some(esc_prefix(b"\x00")),
        NamedKey::Space => return Some(esc_prefix(b" ")),
        _ => {}
    }
    if let Some((code, fin)) = kitty_legacy_form(named) {
        let letter = fin.is_ascii_alphabetic();
        if plain {
            // DECCKM: unmodified cursor keys (and Home / End, as in
            // xterm) use SS3. F1-F4 always do when unmodified.
            let ss3 = match named {
                NamedKey::F(1..=4) => true,
                NamedKey::ArrowUp
                | NamedKey::ArrowDown
                | NamedKey::ArrowLeft
                | NamedKey::ArrowRight
                | NamedKey::Home
                | NamedKey::End => modes.app_cursor,
                _ => false,
            };
            return Some(if ss3 {
                vec![0x1b, b'O', fin]
            } else if letter {
                vec![0x1b, b'[', fin]
            } else {
                format!("\x1b[{code}~").into_bytes()
            });
        }
        // Modified: always the CSI form with the modifier parameter.
        return Some(format!("\x1b[{code};{mod_param}{}", fin as char).into_bytes());
    }
    None
}

/// xterm / kitty legacy modifier parameter: 1 + shift(1) + alt(2) +
/// ctrl(4) + super(8). Lock modifiers are left out.
fn legacy_modifier_param(m: Modifiers) -> u32 {
    kitty_modifiers(m & (Modifiers::SHIFT | Modifiers::ALT | Modifiers::CONTROL | Modifiers::SUPER))
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

/// Event-type sub-field: none for a press (`1` is the default, and
/// kitty never sends it), `2` for a repeat, `3` for a release.
fn kitty_event_suffix(state: KeyState, repeat: bool) -> Option<&'static str> {
    match (state, repeat) {
        (KeyState::Pressed, false) => None,
        (KeyState::Pressed, true) => Some("2"),
        (KeyState::Released, _) => Some("3"),
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

/// The kitty progressive-enhancement encoder. Matches kitty 0.49.2
/// (checked live, key by key, for flags 1, 2, 3, 4, 8, 9, 16, 24 and 31):
///
/// - Enter, Tab and Backspace keep their legacy bytes when unmodified,
///   and never report a release, unless report-all (8) is on, so a shell
///   stays usable after a TUI dies with the flags set (spec). Modified,
///   they're `CSI code;mods u`.
/// - Text keys (letters, digits, Space, ...) without Ctrl / Alt / Super
///   send their text unless report-all is on; Shift doesn't matter.
/// - With Ctrl / Alt / Super, or Esc, a key is `CSI ... u` under
///   disambiguate (1). Under report-events (2) alone the press keeps its
///   legacy bytes and only the release is an escape code.
/// - Report-all (8) sends every key as an escape code; with report-text
///   (16) text keys carry their text, and with report-alternates (4) the
///   shifted key.
#[allow(clippy::too_many_lines)]
fn encode_key_kitty(
    logical: &LogicalKey,
    text: Option<&str>,
    modifiers: Modifiers,
    flags: u8,
    state: KeyState,
    repeat: bool,
    modes: LegacyModes,
) -> Option<Vec<u8>> {
    let report_events = flags & KITTY_FLAG_REPORT_EVENTS != 0;
    let disambiguate = flags & KITTY_FLAG_DISAMBIGUATE != 0;
    let report_all = flags & KITTY_FLAG_REPORT_ALL_AS_ESC != 0;
    let report_text = report_all && flags & KITTY_FLAG_REPORT_TEXT != 0;
    let report_alternates = flags & KITTY_FLAG_REPORT_ALTERNATE != 0;
    let released = state == KeyState::Released;

    // Releases are only reported when bit 2 is on.
    if released && !report_events {
        return None;
    }

    // `modifiers` is the modifier state at the time of *this* event (winit
    // refreshes it from the X11 core `state` field / XKB before every key
    // event), which is what kitty reports too. So Ctrl held across the
    // release of `c` gives `CSI 99;5:3u`, while releasing Ctrl first
    // correctly gives `CSI 99;1:3u`. Note `xdotool key ctrl+c` releases
    // Ctrl *before* `c` (verified with xev), so it always produces the
    // latter — that is not a modifier-tracking bug.
    let m = kitty_modifiers(modifiers);
    let event_suffix = if report_events {
        kitty_event_suffix(state, repeat)
    } else {
        None
    };
    let csi_mods = modifiers.intersects(Modifiers::CONTROL | Modifiers::ALT | Modifiers::SUPER);
    let any_mods = csi_mods || modifiers.contains(Modifiers::SHIFT);
    let legacy = || encode_key_legacy(logical, text, modifiers, modes);

    // Everything below is a text key (letters, digits, Space, ...).
    let code = match logical {
        LogicalKey::Named(named @ (NamedKey::Enter | NamedKey::Tab | NamedKey::Backspace)) => {
            let code = kitty_named_keycode(*named)?;
            // Unmodified: legacy bytes, and no release (spec). Modified:
            // an escape code, release included.
            if !report_all && !any_mods {
                if released {
                    return None;
                }
                return legacy();
            }
            return Some(format_kitty(code, m, event_suffix, None, None));
        }
        LogicalKey::Named(NamedKey::Escape) => {
            // Report-events alone keeps only a plain Esc press legacy;
            // a modified Esc, a repeat and a release are escape codes.
            if !disambiguate && !report_all && !released && !repeat && !any_mods {
                return legacy();
            }
            return Some(format_kitty(27, m, event_suffix, None, None));
        }
        LogicalKey::Named(NamedKey::Space) => 32,
        LogicalKey::Named(named) => {
            // Keys with a legacy CSI letter/tilde form stay in that form
            // even under kitty progressive enhancement — that's what the
            // spec mandates and what every other terminal does. crossterm's
            // parser also expects this; sending PUA `CSI 57352 u` for Up
            // would be interpreted as a literal U+E008 character.
            let (code, final_byte) = match kitty_legacy_form(*named)? {
                // F3 is `CSI 13 ~` under the kitty flags: its legacy
                // `CSI R` / `CSI 1;mR` collides with a CPR reply.
                (1, b'R') => (13, b'~'),
                form => form,
            };
            return Some(format_kitty_legacy(code, m, event_suffix, final_byte));
        }
        LogicalKey::Character(s) => {
            let c = s.chars().next()?;
            // Use lowercase codepoint for ASCII letters (kitty wants the
            // un-shifted base character). For other Unicode characters,
            // pass through.
            let c = if c.is_ascii_uppercase() {
                c.to_ascii_lowercase()
            } else {
                c
            };
            u32::from(c)
        }
        LogicalKey::Unidentified => {
            return text
                .filter(|t| !t.is_empty() && !released)
                .map(|t| t.as_bytes().to_vec());
        }
    };
    // The key's printable text, if any (control characters don't count).
    let printable = text.filter(|t| !t.is_empty() && !t.chars().any(char::is_control));
    // Report-alternates (4): the shifted key, on every event that is an
    // escape code (not only under report-all): ctrl+shift+a is
    // `CSI 97:65;6u` under 1|4.
    let shifted = if report_alternates && modifiers.contains(Modifiers::SHIFT) {
        let single = |t: &str| {
            let mut chars = t.chars();
            let ch = chars.next()?;
            chars.next().is_none().then_some(u32::from(ch))
        };
        // The shifted logical key (winit gives `A` for shift+a), else
        // the typed text when it is a single printable character.
        let from_logical = match logical {
            LogicalKey::Character(s) => single(s),
            _ => None,
        };
        from_logical
            .filter(|&ch| ch != code)
            .or_else(|| printable.and_then(single).filter(|&ch| ch != code))
    } else {
        None
    };
    if !report_all {
        if csi_mods {
            // Report-events alone: a first press keeps its legacy
            // bytes; repeats and releases are escape codes (kitty).
            if disambiguate || released || repeat {
                return Some(format_kitty(code, m, event_suffix, shifted, None));
            }
            return legacy();
        }
        if released {
            return Some(format_kitty(code, m, event_suffix, shifted, None));
        }
        // Plain / shifted text key: just the text.
        if let Some(t) = printable {
            return Some(t.as_bytes().to_vec());
        }
        if matches!(logical, LogicalKey::Named(NamedKey::Space)) {
            return Some(b" ".to_vec());
        }
        return Some(format_kitty(code, m, event_suffix, None, None));
    }
    // Report-all: always an escape code.
    // Text is only attached when the key typed it: not with Ctrl / Alt /
    // Super held (kitty: alt+a is `CSI 97;3u`).
    let text_field = if report_text && !released && !csi_mods {
        printable.map(|t| {
            t.chars()
                .map(|ch| u32::from(ch).to_string())
                .collect::<Vec<_>>()
                .join(":")
        })
    } else {
        None
    };
    Some(format_kitty(
        code,
        m,
        event_suffix,
        shifted,
        text_field.as_deref(),
    ))
}

/// Format a key in the legacy `CSI [code][;mod[:event]] <final>` form
/// used for arrows, F-keys, Home/End, PageUp/PageDown, Insert/Delete.
///
/// - Letter-final keys (A/B/C/D/H/F/P/Q/R/S): the leading code is omitted
///   when no modifier and no event-type suffix are present (`CSI A`), so
///   apps that only handle the bare legacy form still see it.
/// - Tilde-final keys: the leading code is always emitted (`CSI 5 ~`),
///   since `CSI ~` alone isn't a valid functional-key encoding.
fn format_kitty_legacy(
    code: u32,
    modifiers: u32,
    event_suffix: Option<&str>,
    final_byte: u8,
) -> Vec<u8> {
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

fn format_kitty(
    code: u32,
    modifiers: u32,
    event_suffix: Option<&str>,
    shifted: Option<u32>,
    text: Option<&str>,
) -> Vec<u8> {
    // Kitty CSI u form:
    //   CSI keycode[:shifted] [; modifiers [: event-type] [; text]] u
    // The modifier field is written when it's non-default or carries an
    // event type; it's left empty (`CSI 97;;97u`) when only the text
    // field follows, as kitty does.
    let mut out = format!("\x1b[{code}");
    if let Some(sh) = shifted {
        out.push(':');
        out.push_str(&sh.to_string());
    }
    let mods_field = modifiers != 1 || event_suffix.is_some();
    if mods_field || text.is_some() {
        out.push(';');
    }
    if mods_field {
        out.push_str(&modifiers.to_string());
        if let Some(suf) = event_suffix {
            out.push(':');
            out.push_str(suf);
        }
    }
    if let Some(t) = text {
        out.push(';');
        out.push_str(t);
    }
    out.push('u');
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
        // A press is the default type (1), which kitty never writes.
        assert_eq!(kitty_event_suffix(KeyState::Pressed, false), None);
        assert_eq!(kitty_event_suffix(KeyState::Pressed, true), Some("2"));
        assert_eq!(kitty_event_suffix(KeyState::Released, false), Some("3"));
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
        // A press carries no event type (kitty never writes `:1`).
        assert_eq!(press.as_deref(), Some(b"\x1b[97;5u".as_ref()));
        assert_eq!(repeat.as_deref(), Some(b"\x1b[97;5:2u".as_ref()));
        assert_eq!(release.as_deref(), Some(b"\x1b[97;5:3u".as_ref()));
    }

    #[test]
    fn kitty_release_reports_modifiers_at_release_time() {
        // Audit #14: Ctrl still held when `c` goes up -> Ctrl bit kept;
        // Ctrl released first (what `xdotool key ctrl+c` does) -> none.
        let held = enc_kitty(
            &char_key("c"),
            Some("c"),
            Modifiers::CONTROL,
            7,
            KeyState::Released,
            false,
        );
        assert_eq!(held.as_deref(), Some(b"\x1b[99;5:3u".as_ref()));
        let gone = enc_kitty(
            &char_key("c"),
            Some("c"),
            Modifiers::empty(),
            7,
            KeyState::Released,
            false,
        );
        assert_eq!(gone.as_deref(), Some(b"\x1b[99;1:3u".as_ref()));
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

    /// KBD-1: unmodified Enter / Tab / Backspace keep their legacy bytes
    /// (and never report a release) unless report-all (8) is set, so a
    /// shell stays usable after a TUI dies with the flags on. Modified,
    /// they're `CSI code;mods u`. kitty 0.49.2, checked live.
    #[test]
    fn kitty_enter_tab_backspace_stay_legacy_unless_report_all() {
        let press = |k: NamedKey, m: Modifiers, flags: u8| {
            enc_kitty(&named(k), None, m, flags, KeyState::Pressed, false)
        };
        let none = Modifiers::empty();
        for (k, legacy, code) in [
            (NamedKey::Enter, &b"\r"[..], 13),
            (NamedKey::Tab, b"\t", 9),
            (NamedKey::Backspace, b"\x7f", 127),
        ] {
            for flags in [1, 2, 3, 1 | 4, 1 | 16] {
                assert_eq!(
                    press(k, none, flags).as_deref(),
                    Some(legacy),
                    "{k:?} {flags}"
                );
                let release = enc_kitty(&named(k), None, none, flags, KeyState::Released, false);
                assert_eq!(release, None, "{k:?} {flags}: no release");
            }
            for flags in [1, 2, 3] {
                let want = format!("\x1b[{code};2u");
                assert_eq!(
                    press(k, Modifiers::SHIFT, flags).as_deref(),
                    Some(want.as_bytes()),
                    "{k:?} shift {flags}"
                );
            }
            let want = format!("\x1b[{code}u");
            assert_eq!(
                press(k, none, 8).as_deref(),
                Some(want.as_bytes()),
                "{k:?} 8"
            );
            let rel = enc_kitty(&named(k), None, none, 8 | 2, KeyState::Released, false);
            let want = format!("\x1b[{code};1:3u");
            assert_eq!(rel.as_deref(), Some(want.as_bytes()), "{k:?} release 10");
        }
    }

    #[test]
    fn kitty_text_keys_send_text_unless_report_all() {
        // Space and letters are text keys: plain or shifted they type
        // their text under 1 / 2; only report-all makes them escapes.
        let sp = named(NamedKey::Space);
        for flags in [1, 2, 3] {
            let got = enc_kitty(
                &sp,
                Some(" "),
                Modifiers::empty(),
                flags,
                KeyState::Pressed,
                false,
            );
            assert_eq!(got.as_deref(), Some(&b" "[..]), "space {flags}");
            let got = enc_kitty(
                &sp,
                Some(" "),
                Modifiers::SHIFT,
                flags,
                KeyState::Pressed,
                false,
            );
            assert_eq!(got.as_deref(), Some(&b" "[..]), "shift+space {flags}");
            let got = enc_kitty(
                &char_key("a"),
                Some("A"),
                Modifiers::SHIFT,
                flags,
                KeyState::Pressed,
                false,
            );
            assert_eq!(got.as_deref(), Some(&b"A"[..]), "shift+a {flags}");
        }
        // Report-events: the release of a text key is an escape code.
        let rel = enc_kitty(
            &sp,
            Some(" "),
            Modifiers::empty(),
            2,
            KeyState::Released,
            false,
        );
        assert_eq!(rel.as_deref(), Some(&b"\x1b[32;1:3u"[..]));
    }

    #[test]
    fn kitty_report_all_text_and_alternates() {
        let sp = named(NamedKey::Space);
        // Report-all, plain and with associated text (16) and the
        // shifted alternate (4).
        let got = enc_kitty(
            &sp,
            Some(" "),
            Modifiers::empty(),
            8,
            KeyState::Pressed,
            false,
        );
        assert_eq!(got.as_deref(), Some(&b"\x1b[32u"[..]));
        let got = enc_kitty(
            &char_key("a"),
            Some("a"),
            Modifiers::empty(),
            24,
            KeyState::Pressed,
            false,
        );
        assert_eq!(got.as_deref(), Some(&b"\x1b[97;;97u"[..]));
        let got = enc_kitty(
            &char_key("a"),
            Some("A"),
            Modifiers::SHIFT,
            24,
            KeyState::Pressed,
            false,
        );
        assert_eq!(got.as_deref(), Some(&b"\x1b[97;2;65u"[..]));
        let got = enc_kitty(
            &char_key("a"),
            Some("A"),
            Modifiers::SHIFT,
            28,
            KeyState::Pressed,
            false,
        );
        assert_eq!(got.as_deref(), Some(&b"\x1b[97:65;2;65u"[..]));
        // Alt / Ctrl: no text field (kitty).
        let got = enc_kitty(
            &char_key("a"),
            Some("a"),
            Modifiers::ALT,
            24,
            KeyState::Pressed,
            false,
        );
        assert_eq!(got.as_deref(), Some(&b"\x1b[97;3u"[..]));
        let got = enc_kitty(
            &char_key("a"),
            Some("\x01"),
            Modifiers::CONTROL,
            24,
            KeyState::Pressed,
            false,
        );
        assert_eq!(got.as_deref(), Some(&b"\x1b[97;5u"[..]));
        // Releases carry no text.
        let got = enc_kitty(
            &char_key("a"),
            Some("a"),
            Modifiers::empty(),
            26,
            KeyState::Released,
            false,
        );
        assert_eq!(got.as_deref(), Some(&b"\x1b[97;1:3u"[..]));
    }

    #[test]
    fn kitty_report_events_alone_keeps_legacy_presses() {
        // Flag 2 without disambiguate: Ctrl / Alt keys and Esc press as
        // in legacy mode; their releases are escape codes. kitty 0.49.2.
        let p = |k: &LogicalKey, t: Option<&str>, m: Modifiers| {
            enc_kitty(k, t, m, 2, KeyState::Pressed, false)
        };
        assert_eq!(
            p(&char_key("a"), Some("\x01"), Modifiers::CONTROL).as_deref(),
            Some(&b"\x01"[..])
        );
        assert_eq!(
            p(&char_key("a"), Some("a"), Modifiers::ALT).as_deref(),
            Some(&b"\x1ba"[..])
        );
        assert_eq!(
            p(&named(NamedKey::Escape), None, Modifiers::empty()).as_deref(),
            Some(&b"\x1b"[..])
        );
        let rel = enc_kitty(
            &char_key("a"),
            Some("a"),
            Modifiers::empty(),
            2,
            KeyState::Released,
            false,
        );
        assert_eq!(rel.as_deref(), Some(&b"\x1b[97;1:3u"[..]));
        // Functional keys use the CSI form (F1 is `CSI P`, not SS3).
        assert_eq!(
            p(&named(NamedKey::F(1)), None, Modifiers::empty()).as_deref(),
            Some(&b"\x1b[P"[..])
        );
    }

    /// #43 review: F3, report-alternates without report-all, releases
    /// of modified Enter / Tab / Backspace, and report-events repeats.
    #[test]
    fn kitty_review_43_cases() {
        let p = KeyState::Pressed;
        let r = KeyState::Released;
        let f3 = named(NamedKey::F(3));
        let enc =
            |k: &LogicalKey, t: Option<&str>, m: Modifiers, f: u8, st: KeyState, rep: bool| {
                encode_key(k, t, m, f, st, rep)
            };
        // F3 is `CSI 13 ~` under the kitty flags (`CSI R` is a CPR);
        // legacy mode keeps xterm's SS3 / `CSI 1;mR`.
        assert_eq!(
            enc(&f3, None, Modifiers::empty(), 1, p, false).as_deref(),
            Some(&b"\x1b[13~"[..])
        );
        assert_eq!(
            enc(&f3, None, Modifiers::CONTROL, 1, p, false).as_deref(),
            Some(&b"\x1b[13;5~"[..])
        );
        assert_eq!(
            enc(&f3, None, Modifiers::empty(), 0, p, false).as_deref(),
            Some(&b"\x1bOR"[..])
        );
        assert_eq!(
            enc(&f3, None, Modifiers::CONTROL, 0, p, false).as_deref(),
            Some(&b"\x1b[1;5R"[..])
        );
        // Report-alternates applies to every escape-coded event.
        let a = char_key("A");
        let cs = Modifiers::CONTROL | Modifiers::SHIFT;
        assert_eq!(
            enc(&a, Some("\x01"), cs, 5, p, false).as_deref(),
            Some(&b"\x1b[97:65;6u"[..])
        );
        let als = Modifiers::ALT | Modifiers::SHIFT;
        assert_eq!(
            enc(&a, Some("A"), als, 7, p, false).as_deref(),
            Some(&b"\x1b[97:65;4u"[..])
        );
        assert_eq!(
            enc(&a, Some("A"), Modifiers::SHIFT, 7, r, false).as_deref(),
            Some(&b"\x1b[97:65;2:3u"[..])
        );
        // Modified Enter / Tab / Backspace report their release.
        for (k, code) in [
            (NamedKey::Enter, 13),
            (NamedKey::Tab, 9),
            (NamedKey::Backspace, 127),
        ] {
            let want = format!("\x1b[{code};2:3u");
            let got = enc(&named(k), None, Modifiers::SHIFT, 2, r, false);
            assert_eq!(got.as_deref(), Some(want.as_bytes()), "{k:?}");
            assert_eq!(
                enc(&named(k), None, Modifiers::empty(), 2, r, false),
                None,
                "{k:?}"
            );
        }
        // Report-events alone: Ctrl / Alt repeats and modified Esc are
        // escape codes; a plain Esc press stays legacy.
        let ca = char_key("a");
        assert_eq!(
            enc(&ca, Some("\x01"), Modifiers::CONTROL, 2, p, true).as_deref(),
            Some(&b"\x1b[97;5:2u"[..])
        );
        assert_eq!(
            enc(&ca, Some("a"), Modifiers::ALT, 2, p, true).as_deref(),
            Some(&b"\x1b[97;3:2u"[..])
        );
        assert_eq!(
            enc(&ca, Some("\x01"), Modifiers::CONTROL, 2, p, false).as_deref(),
            Some(&b"\x01"[..])
        );
        let esc = named(NamedKey::Escape);
        assert_eq!(
            enc(&esc, None, Modifiers::CONTROL, 2, p, false).as_deref(),
            Some(&b"\x1b[27;5u"[..])
        );
        assert_eq!(
            enc(&esc, None, Modifiers::empty(), 2, p, false).as_deref(),
            Some(&b"\x1b"[..])
        );
    }

    #[test]
    fn kitty_alternates_or_text_alone_are_legacy() {
        // 4 alone (and 16 without 8, undefined in the spec) change
        // nothing: kitty sends the legacy bytes.
        for flags in [4, 16, 4 | 16] {
            let got = encode_key(
                &named(NamedKey::Escape),
                None,
                Modifiers::empty(),
                flags,
                KeyState::Pressed,
                false,
            );
            assert_eq!(got.as_deref(), Some(&b"\x1b"[..]), "{flags}");
            let got = encode_key(
                &named(NamedKey::Tab),
                None,
                Modifiers::SHIFT,
                flags,
                KeyState::Pressed,
                false,
            );
            assert_eq!(got.as_deref(), Some(&b"\x1b[Z"[..]), "{flags}");
            let rel = encode_key(
                &char_key("a"),
                Some("a"),
                Modifiers::empty(),
                flags,
                KeyState::Released,
                false,
            );
            assert_eq!(rel, None, "{flags}");
        }
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
        let bytes = format_kitty(97, 1, None, None, None);
        assert_eq!(bytes, b"\x1b[97u".to_vec());
    }

    #[test]
    fn format_kitty_includes_event_suffix_when_present() {
        let bytes = format_kitty(97, 1, Some("3"), None, None);
        // When event-type is present, modifiers must be emitted too.
        assert_eq!(bytes, b"\x1b[97;1:3u".to_vec());
    }

    #[test]
    fn format_kitty_text_and_shifted_fields() {
        // Empty modifier field when only text follows (kitty).
        assert_eq!(format_kitty(97, 1, None, None, Some("97")), b"\x1b[97;;97u");
        assert_eq!(
            format_kitty(97, 2, None, Some(65), Some("65")),
            b"\x1b[97:65;2;65u"
        );
    }
}

#[cfg(test)]
mod legacy_mode_tests {
    use super::*;

    fn named(k: NamedKey) -> LogicalKey {
        LogicalKey::Named(k)
    }

    fn enc(k: &LogicalKey, text: Option<&str>, m: Modifiers, modes: LegacyModes) -> Vec<u8> {
        encode_key_with_modes(k, text, m, 0, KeyState::Pressed, false, modes).unwrap()
    }

    const APP: LegacyModes = LegacyModes {
        app_cursor: true,
        alt_escape: true,
    };

    #[test]
    fn legacy_modified_named_keys_match_xterm_and_kitty() {
        let enc =
            |k: NamedKey, m: Modifiers| encode_key(&named(k), None, m, 0, KeyState::Pressed, false);
        assert_eq!(
            enc(NamedKey::Tab, Modifiers::SHIFT).as_deref(),
            Some(&b"\x1b[Z"[..])
        );
        assert_eq!(
            enc(NamedKey::Backspace, Modifiers::CONTROL).as_deref(),
            Some(&b"\x08"[..])
        );
        assert_eq!(
            enc(NamedKey::Space, Modifiers::CONTROL).as_deref(),
            Some(&b"\x00"[..])
        );
        assert_eq!(
            enc(NamedKey::Enter, Modifiers::ALT).as_deref(),
            Some(&b"\x1b\r"[..])
        );
        assert_eq!(
            enc(NamedKey::Enter, Modifiers::SHIFT).as_deref(),
            Some(&b"\r"[..])
        );
        assert_eq!(
            enc(NamedKey::Enter, Modifiers::CONTROL).as_deref(),
            Some(&b"\r"[..])
        );
        assert_eq!(
            enc(NamedKey::ArrowUp, Modifiers::SHIFT).as_deref(),
            Some(&b"\x1b[1;2A"[..])
        );
        assert_eq!(
            enc(NamedKey::F(1), Modifiers::CONTROL | Modifiers::SHIFT).as_deref(),
            Some(&b"\x1b[1;6P"[..])
        );
        assert_eq!(
            enc(NamedKey::PageUp, Modifiers::ALT).as_deref(),
            Some(&b"\x1b[5;3~"[..])
        );
        assert_eq!(
            enc(NamedKey::F(5), Modifiers::empty()).as_deref(),
            Some(&b"\x1b[15~"[..])
        );
        // Lock modifiers aren't reported in legacy mode.
        assert_eq!(
            enc(NamedKey::ArrowUp, Modifiers::NUM_LOCK).as_deref(),
            Some(&b"\x1b[A"[..])
        );
    }

    #[test]
    fn decckm_switches_cursor_keys_to_ss3() {
        for (k, fin) in [
            (NamedKey::ArrowUp, b'A'),
            (NamedKey::ArrowDown, b'B'),
            (NamedKey::ArrowRight, b'C'),
            (NamedKey::ArrowLeft, b'D'),
            (NamedKey::Home, b'H'),
            (NamedKey::End, b'F'),
        ] {
            assert_eq!(
                enc(&named(k), None, Modifiers::empty(), APP),
                vec![0x1b, b'O', fin]
            );
            assert_eq!(
                enc(&named(k), None, Modifiers::empty(), LegacyModes::default()),
                vec![0x1b, b'[', fin]
            );
            // A modifier switches to the CSI form with the xterm
            // modifier parameter (`CSI 1 ; 5 A` for Ctrl+Up).
            let mut want = b"\x1b[1;5".to_vec();
            want.push(fin);
            assert_eq!(enc(&named(k), None, Modifiers::CONTROL, APP), want);
        }
        // Other named keys are unaffected.
        assert_eq!(
            enc(&named(NamedKey::PageUp), None, Modifiers::empty(), APP),
            b"\x1b[5~"
        );
    }

    #[test]
    fn decckm_does_not_affect_kitty_encoding() {
        let got = encode_key_with_modes(
            &named(NamedKey::ArrowUp),
            None,
            Modifiers::empty(),
            KITTY_FLAG_DISAMBIGUATE,
            KeyState::Pressed,
            false,
            APP,
        );
        assert_eq!(got.as_deref(), Some(&b"\x1b[A"[..]));
    }

    #[test]
    fn alt_escape_off_sends_plain_text() {
        let off = LegacyModes {
            app_cursor: false,
            alt_escape: false,
        };
        let a = LogicalKey::Character("a".into());
        assert_eq!(enc(&a, Some("a"), Modifiers::ALT, off), b"a");
        assert_eq!(
            enc(&a, Some("a"), Modifiers::ALT | Modifiers::CONTROL, off),
            [1]
        );
        assert_eq!(
            enc(&a, Some("a"), Modifiers::ALT, LegacyModes::default()),
            b"\x1ba"
        );
    }

    #[test]
    fn app_keypad_digits_follow_numlock_from_logical_key() {
        let nav = named(NamedKey::End); // KP_1 with NumLock off
        assert_eq!(
            encode_app_keypad("Numpad1", &nav, Modifiers::empty(), true).as_deref(),
            Some(&b"\x1bOq"[..])
        );
        let digit = LogicalKey::Character("1".into());
        assert_eq!(
            encode_app_keypad("Numpad1", &digit, Modifiers::empty(), false),
            None
        );
        for (code, fin) in [
            ("Numpad0", b'p'),
            ("Numpad9", b'y'),
            ("NumpadDecimal", b'n'),
        ] {
            assert_eq!(
                encode_app_keypad(code, &nav, Modifiers::empty(), true),
                Some(vec![0x1b, b'O', fin])
            );
        }
    }

    #[test]
    fn app_keypad_operators_use_numlock_hint() {
        let plus = LogicalKey::Character("+".into());
        assert_eq!(
            encode_app_keypad("NumpadAdd", &plus, Modifiers::empty(), true),
            None
        );
        for (code, fin) in [
            ("NumpadEnter", b'M'),
            ("NumpadMultiply", b'j'),
            ("NumpadAdd", b'k'),
            ("NumpadComma", b'l'),
            ("NumpadSubtract", b'm'),
            ("NumpadDivide", b'o'),
            ("NumpadEqual", b'X'),
        ] {
            assert_eq!(
                encode_app_keypad(code, &plus, Modifiers::empty(), false),
                Some(vec![0x1b, b'O', fin])
            );
        }
    }

    #[test]
    fn app_keypad_ignores_modified_and_non_keypad_keys() {
        let nav = named(NamedKey::End);
        assert_eq!(
            encode_app_keypad("Numpad1", &nav, Modifiers::CONTROL, false),
            None
        );
        assert_eq!(
            encode_app_keypad("Digit1", &nav, Modifiers::empty(), false),
            None
        );
        assert_eq!(
            encode_app_keypad("NumpadBackspace", &nav, Modifiers::empty(), false),
            None
        );
    }

    #[test]
    fn numlock_hint_from_keypad_digits() {
        let digit = LogicalKey::Character("5".into());
        assert_eq!(numlock_hint("Numpad5", &digit), Some(true));
        assert_eq!(
            numlock_hint("Numpad5", &named(NamedKey::Other)),
            Some(false)
        );
        assert_eq!(numlock_hint("NumpadAdd", &digit), None);
        assert_eq!(numlock_hint("KeyA", &digit), None);
    }
}
