//! OSC 4 — extended palette (xterm 256-color) query + override.
//!
//! Sequence shape:
//!
//! ```text
//! OSC 4 ; <idx> ; <spec> [; <idx> ; <spec> ]* ST
//! ```
//!
//! `<idx>` is the 0..256 palette index. `<spec>` is either an `rgb:R/G/B`
//! color spec or the literal `?` to query the current value.
//!
//! - Query (`<spec>` == `?`): the terminal replies with
//!   `OSC 4 ; <idx> ; rgb:RRRR/GGGG/BBBB ST` using the 4-digit-per-channel
//!   form xterm uses (each 8-bit channel doubled into 16 bits as
//!   `0xAB → "ABAB"`).
//! - Set: the override is recorded and used by the renderer in place of
//!   the built-in xterm 256-color table for that index.
//!
//! Multi-pair: a single OSC 4 sequence can carry many `idx ; spec` pairs.
//! The caller (Term) walks `params[1..]` in steps of two.
//!
//! The parsing surface here is intentionally tiny:
//!   - [`parse_pair`] consumes one index + spec, returning [`Osc4Op`].
//!   - [`encode_query_reply`] formats a reply for one index.
//!   - [`default_xterm_256`] gives the canonical xterm sRGB triple for
//!     an index — used by the renderer when no override has been set.

#![allow(clippy::doc_markdown)]

/// Parsed result of one `(idx, spec)` pair from an OSC 4 sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Osc4Op {
    /// `OSC 4 ; <idx> ; ?` — the app wants to know the current value.
    Query { idx: u8 },
    /// `OSC 4 ; <idx> ; rgb:R/G/B` — override the palette entry.
    Set { idx: u8, rgb: [u8; 3] },
}

/// Parse one `(idx, spec)` pair into an [`Osc4Op`]. Returns `None` for any
/// malformed input (non-numeric index, unknown spec, out-of-range hex).
#[must_use]
pub fn parse_pair(idx_bytes: &[u8], spec_bytes: &[u8]) -> Option<Osc4Op> {
    let idx: u16 = std::str::from_utf8(idx_bytes).ok()?.trim().parse().ok()?;
    let idx = u8::try_from(idx).ok()?;
    // Trim leading whitespace from the spec — apps sometimes pad.
    let spec = trim_ascii_ws(spec_bytes);
    if spec == b"?" {
        return Some(Osc4Op::Query { idx });
    }
    let rgb = parse_rgb_spec(spec)?;
    Some(Osc4Op::Set { idx, rgb })
}

/// Format the OSC 4 query reply for index `idx` with sRGB `[R, G, B]`.
///
/// The reply uses the 4-digit-per-channel xterm form: each 8-bit channel
/// is doubled into a 16-bit value (`0xAB → "ABAB"`). The returned bytes
/// include the OSC introducer and ST terminator:
///
/// ```text
/// ESC ] 4 ; <idx> ; rgb:RRRR/GGGG/BBBB ESC \
/// ```
#[must_use]
pub fn encode_query_reply(idx: u8, rgb: [u8; 3]) -> Vec<u8> {
    let r = u16::from(rgb[0]) * 0x101;
    let g = u16::from(rgb[1]) * 0x101;
    let b = u16::from(rgb[2]) * 0x101;
    format!("\x1b]4;{idx};rgb:{r:04x}/{g:04x}/{b:04x}\x1b\\").into_bytes()
}

/// Built-in xterm 256-color sRGB triple for index `idx`.
///
/// - `0..16`: the standard 16-color palette (the same constants xterm
///   uses by default).
/// - `16..232`: the 6×6×6 RGB cube at xterm levels
///   `[0, 95, 135, 175, 215, 255]`.
/// - `232..256`: the 24-step grayscale ramp at `8 + 10 * step`.
#[must_use]
pub fn default_xterm_256(idx: u8) -> [u8; 3] {
    if idx < 16 {
        return XTERM_BASE_16[idx as usize];
    }
    if idx < 232 {
        let n = idx - 16;
        let r = CUBE_LEVELS[(n / 36) as usize];
        let g = CUBE_LEVELS[((n / 6) % 6) as usize];
        let b = CUBE_LEVELS[(n % 6) as usize];
        return [r, g, b];
    }
    let step = idx - 232;
    let v = 8 + 10 * step;
    [v, v, v]
}

/// xterm 6×6×6 cube levels.
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// xterm default sRGB triples for the first 16 colors. Same as the
/// canonical xterm.terminfo table.
const XTERM_BASE_16: [[u8; 3]; 16] = [
    [0x00, 0x00, 0x00], // 0  black
    [0x80, 0x00, 0x00], // 1  red
    [0x00, 0x80, 0x00], // 2  green
    [0x80, 0x80, 0x00], // 3  yellow (olive)
    [0x00, 0x00, 0x80], // 4  blue
    [0x80, 0x00, 0x80], // 5  magenta
    [0x00, 0x80, 0x80], // 6  cyan
    [0xc0, 0xc0, 0xc0], // 7  white
    [0x80, 0x80, 0x80], // 8  bright black (dark gray)
    [0xff, 0x00, 0x00], // 9  bright red
    [0x00, 0xff, 0x00], // 10 bright green
    [0xff, 0xff, 0x00], // 11 bright yellow
    [0x00, 0x00, 0xff], // 12 bright blue
    [0xff, 0x00, 0xff], // 13 bright magenta
    [0x00, 0xff, 0xff], // 14 bright cyan
    [0xff, 0xff, 0xff], // 15 bright white
];

/// Parse an `rgb:R/G/B` color spec. Each channel is 1–4 hex digits;
/// xterm canonically writes 4 (`0xRRRR`), but 8-bit (`0xRR`) is also
/// widely emitted. We rescale to 8 bits.
fn parse_rgb_spec(spec: &[u8]) -> Option<[u8; 3]> {
    const PREFIX: &[u8] = b"rgb:";
    if !spec.starts_with(PREFIX) {
        return None;
    }
    let rest = &spec[PREFIX.len()..];
    let mut parts = rest.split(|&b| b == b'/');
    let r = parse_hex_channel(parts.next()?)?;
    let g = parse_hex_channel(parts.next()?)?;
    let b = parse_hex_channel(parts.next()?)?;
    if parts.next().is_some() {
        // More than 3 components — malformed.
        return None;
    }
    Some([r, g, b])
}

/// Parse one hex channel of arbitrary width (1..=4 digits) and rescale
/// to 8 bits.
fn parse_hex_channel(bytes: &[u8]) -> Option<u8> {
    if bytes.is_empty() || bytes.len() > 4 {
        return None;
    }
    let mut v: u32 = 0;
    for &b in bytes {
        v = (v << 4) | u32::from(hex_value(b)?);
    }
    // Rescale to 8 bits depending on the source width. `0xAB` → 0xAB;
    // `0xABCD` → top byte (the standard xterm short-form mapping).
    let bits = bytes.len() * 4;
    let rescaled = if bits >= 8 {
        v >> (bits - 8)
    } else {
        v << (8 - bits)
    };
    u8::try_from(rescaled & 0xff).ok()
}

const fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn trim_ascii_ws(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < bytes.len() && bytes[start].is_ascii_whitespace() {
        start += 1;
    }
    let mut end = bytes.len();
    while end > start && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    &bytes[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_query() {
        assert_eq!(parse_pair(b"5", b"?"), Some(Osc4Op::Query { idx: 5 }));
    }

    #[test]
    fn parses_set_with_4_digit_channels() {
        assert_eq!(
            parse_pair(b"1", b"rgb:abab/cdcd/efef"),
            Some(Osc4Op::Set {
                idx: 1,
                rgb: [0xab, 0xcd, 0xef],
            })
        );
    }

    #[test]
    fn parses_set_with_2_digit_channels() {
        assert_eq!(
            parse_pair(b"1", b"rgb:ab/cd/ef"),
            Some(Osc4Op::Set {
                idx: 1,
                rgb: [0xab, 0xcd, 0xef],
            })
        );
    }

    #[test]
    fn parses_set_with_1_digit_channels() {
        // `rgb:f/0/0` — rescales to 0xf0 / 0x00 / 0x00.
        assert_eq!(
            parse_pair(b"7", b"rgb:f/0/0"),
            Some(Osc4Op::Set {
                idx: 7,
                rgb: [0xf0, 0x00, 0x00],
            })
        );
    }

    #[test]
    fn malformed_returns_none() {
        // Bad index.
        assert_eq!(parse_pair(b"xx", b"?"), None);
        // Out-of-range index (u8 overflow).
        assert_eq!(parse_pair(b"500", b"?"), None);
        // Unknown spec.
        assert_eq!(parse_pair(b"5", b"yellow"), None);
        // Bad rgb format.
        assert_eq!(parse_pair(b"5", b"rgb:zz/00/00"), None);
        // Too few channels.
        assert_eq!(parse_pair(b"5", b"rgb:00/00"), None);
        // Too many channels.
        assert_eq!(parse_pair(b"5", b"rgb:00/00/00/00"), None);
    }

    #[test]
    fn encode_query_reply_uses_4_digit_per_channel() {
        let bytes = encode_query_reply(5, [0xab, 0x12, 0x34]);
        let s = std::str::from_utf8(&bytes).unwrap();
        assert_eq!(s, "\x1b]4;5;rgb:abab/1212/3434\x1b\\");
    }

    #[test]
    fn default_xterm_256_known_indices() {
        // Cube corners.
        assert_eq!(default_xterm_256(16), [0, 0, 0]);
        assert_eq!(default_xterm_256(231), [255, 255, 255]);
        // Grayscale ramp first / last.
        assert_eq!(default_xterm_256(232), [8, 8, 8]);
        assert_eq!(default_xterm_256(255), [238, 238, 238]);
        // Named (red).
        assert_eq!(default_xterm_256(1), [0x80, 0, 0]);
    }
}
