//! OSC 52 — programmable clipboard.
//!
//! Sequence shape:
//!
//! ```text
//! OSC 52 ; <selection> ; <payload> ST
//! ```
//!
//! - `<selection>`: zero or more characters from `c` (clipboard), `p`
//!   (primary), `q` (secondary), `s` (selection), `0..=7` (cut
//!   buffers). We honour `c` and `p` individually, so `cp` targets
//!   both. xterm treats an empty selection as `s 0`, where `s` is "the
//!   configurable primary/clipboard selection"; we map `s` (and the
//!   empty selection) to *both* clipboard and primary on write so the
//!   copied text lands wherever the user pastes from. `q` and the cut
//!   buffers have no modern OS equivalent and are ignored. See
//!   [`SelectionChars::write_targets`] / [`SelectionChars::read_target`].
//! - `<payload>`: base64 (STANDARD alphabet, not URL-safe) of the bytes
//!   to write, or the literal `?` to ask the terminal to read its
//!   clipboard and reply with `OSC 52 ; <selection> ; <base64> ST`.
//!   Padding is optional (several emitters omit it). Per xterm
//!   ctlseqs, "if the second parameter is neither a base64 string nor
//!   `?`, then the selection is cleared" — an empty or undecodable
//!   payload parses to [`Osc52Op::Clear`].
//!
//! ## Security
//!
//! Write (Set / Clear) and read (Query) are guarded by the user's
//! `[security]` config — the term crate checks the flags before
//! queueing a parsed op.

#![allow(clippy::doc_markdown)]

use base64::{
    Engine, alphabet,
    engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig, general_purpose::STANDARD},
};

/// STANDARD alphabet decoder that accepts both padded and unpadded
/// input (`aGVsbG8=` and `aGVsbG8`). The base64 crate's default
/// engine insists on canonical padding, which silently dropped
/// writes from emitters that strip the trailing `=`.
const LENIENT: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// Parsed OSC 52 operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Osc52Op {
    /// Write `payload` bytes to the selection(s) named by `selection`.
    Set {
        selection: SelectionChars,
        payload: Vec<u8>,
    },
    /// Clear the selection(s) named by `selection`. Sent as an empty
    /// or non-base64 payload (xterm semantics).
    Clear { selection: SelectionChars },
    /// Read the system clipboard and reply.
    Query { selection: SelectionChars },
}

/// One OS-level selection buffer OSC 52 can address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClipboardTarget {
    /// The explicit-copy clipboard (`CLIPBOARD` on X11/Wayland, the
    /// pasteboard on macOS).
    Clipboard,
    /// The X11/Wayland `PRIMARY` selection (select-to-copy,
    /// middle-click-to-paste). Platforms without one fall back to the
    /// clipboard in the binary.
    Primary,
}

/// Set of selection buffers an OSC 52 write applies to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClipboardTargets {
    pub clipboard: bool,
    pub primary: bool,
}

impl ClipboardTargets {
    /// True when no supported buffer was named (e.g. only `q` or cut
    /// buffers) — the request is a no-op.
    #[must_use]
    pub fn is_empty(self) -> bool {
        !self.clipboard && !self.primary
    }
}

/// Raw selection characters as transmitted. Stored verbatim so the
/// reply matches whatever the app asked for (some apps echo back the
/// selection bytes literally).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionChars(pub Vec<u8>);

impl SelectionChars {
    /// Return the canonical reply selection char. We always reply with
    /// the first selection byte the app sent, defaulting to `c` so the
    /// app's parser doesn't choke on an empty selection.
    #[must_use]
    pub fn reply_char(&self) -> u8 {
        self.0.first().copied().unwrap_or(b'c')
    }

    /// Buffers a write (or clear) should touch.
    ///
    /// - `c` → clipboard, `p` → primary; multiple characters combine.
    /// - `s`, or an empty selection (xterm's `s 0` default) → both.
    /// - `q` and `0..=7` are accepted but ignored.
    #[must_use]
    pub fn write_targets(&self) -> ClipboardTargets {
        if self.0.is_empty() {
            return ClipboardTargets {
                clipboard: true,
                primary: true,
            };
        }
        let mut t = ClipboardTargets::default();
        for &b in &self.0 {
            match b {
                b'c' => t.clipboard = true,
                b'p' => t.primary = true,
                b's' => {
                    t.clipboard = true;
                    t.primary = true;
                }
                _ => {}
            }
        }
        t
    }

    /// Buffer a query should read: the first supported character in
    /// the order the app listed them (`c`/`s` → clipboard, `p` →
    /// primary). Empty selection reads the clipboard. `None` when
    /// only unsupported characters were sent.
    #[must_use]
    pub fn read_target(&self) -> Option<ClipboardTarget> {
        if self.0.is_empty() {
            return Some(ClipboardTarget::Clipboard);
        }
        self.0.iter().find_map(|&b| match b {
            b'c' | b's' => Some(ClipboardTarget::Clipboard),
            b'p' => Some(ClipboardTarget::Primary),
            _ => None,
        })
    }
}

/// Parse the payload past `OSC 52 ;` into an [`Osc52Op`].
///
/// `payload` is the joined `<selection>;<base64-or-?>` bytes. Returns
/// `None` only when the `;` separating selection from data is missing;
/// any data that isn't `?` or valid base64 yields [`Osc52Op::Clear`].
#[must_use]
pub fn parse(payload: &[u8]) -> Option<Osc52Op> {
    let mut iter = payload.splitn(2, |&b| b == b';');
    let selection_bytes = iter.next()?.to_vec();
    let data = iter.next()?;
    let selection = SelectionChars(selection_bytes);
    if data == b"?" {
        return Some(Osc52Op::Query { selection });
    }
    // Tolerate stray ASCII whitespace inside the base64 segment (some
    // shells line-wrap long pastes).
    let cleaned: Vec<u8> = data
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    match LENIENT.decode(&cleaned) {
        Ok(decoded) if !decoded.is_empty() => Some(Osc52Op::Set {
            selection,
            payload: decoded,
        }),
        // Empty payload or invalid base64: xterm clears the selection.
        _ => Some(Osc52Op::Clear { selection }),
    }
}

/// Encode an OSC 52 read reply: `ESC ] 52 ; <selection> ; <base64> ST`.
///
/// `bell_terminated` mirrors the request's terminator, as xterm does:
/// a query ending in BEL gets a BEL-terminated reply, otherwise the
/// reply ends in `ESC \`. Some emitters only parse the terminator
/// they sent.
#[must_use]
pub fn encode_reply(selection: &SelectionChars, data: &[u8], bell_terminated: bool) -> Vec<u8> {
    let encoded = STANDARD.encode(data);
    let sel_char = char::from(selection.reply_char());
    let st = if bell_terminated { "\x07" } else { "\x1b\\" };
    format!("\x1b]52;{sel_char};{encoded}{st}").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(s: &str) -> SelectionChars {
        SelectionChars(s.as_bytes().to_vec())
    }

    #[test]
    fn parses_set_with_base64() {
        // base64 of "hello" is "aGVsbG8="
        let op = parse(b"c;aGVsbG8=").unwrap();
        match op {
            Osc52Op::Set { selection, payload } => {
                assert_eq!(selection.0, b"c");
                assert_eq!(payload, b"hello");
            }
            other => panic!("expected Set, got {other:?}"),
        }
    }

    #[test]
    fn parses_unpadded_base64() {
        for data in [&b"c;aGVsbG8"[..], b"c;aGk", b"c;YQ"] {
            match parse(data).unwrap() {
                Osc52Op::Set { .. } => {}
                other => panic!("expected Set for {data:?}, got {other:?}"),
            }
        }
        assert_eq!(
            parse(b"c;aGVsbG8").unwrap(),
            Osc52Op::Set {
                selection: sel("c"),
                payload: b"hello".to_vec()
            }
        );
    }

    #[test]
    fn parses_query() {
        let op = parse(b"c;?").unwrap();
        match op {
            Osc52Op::Query { selection } => assert_eq!(selection.0, b"c"),
            other => panic!("expected Query, got {other:?}"),
        }
    }

    #[test]
    fn parses_multi_char_selection() {
        let op = parse(b"cs;aGVsbG8=").unwrap();
        match op {
            Osc52Op::Set { selection, .. } => assert_eq!(selection.0, b"cs"),
            other => panic!("expected Set, got {other:?}"),
        }
    }

    #[test]
    fn malformed_base64_clears() {
        // `!!` is not valid base64 STANDARD; xterm clears the selection.
        assert_eq!(
            parse(b"c;!!").unwrap(),
            Osc52Op::Clear {
                selection: sel("c")
            }
        );
        // A lone sextet can't encode a byte either.
        assert_eq!(
            parse(b"p;a").unwrap(),
            Osc52Op::Clear {
                selection: sel("p")
            }
        );
    }

    #[test]
    fn empty_payload_clears() {
        assert_eq!(
            parse(b"c;").unwrap(),
            Osc52Op::Clear {
                selection: sel("c")
            }
        );
        assert_eq!(parse(b";").unwrap(), Osc52Op::Clear { selection: sel("") });
    }

    #[test]
    fn missing_separator_returns_none() {
        assert!(parse(b"c").is_none());
    }

    #[test]
    fn tolerates_whitespace_in_base64() {
        let op = parse(b"c;aGVs\nbG8=").unwrap();
        if let Osc52Op::Set { payload, .. } = op {
            assert_eq!(payload, b"hello");
        } else {
            panic!("expected Set");
        }
    }

    #[test]
    fn write_targets_per_char() {
        let ct = |c, p| ClipboardTargets {
            clipboard: c,
            primary: p,
        };
        assert_eq!(sel("c").write_targets(), ct(true, false));
        assert_eq!(sel("p").write_targets(), ct(false, true));
        assert_eq!(sel("cp").write_targets(), ct(true, true));
        assert_eq!(sel("pc").write_targets(), ct(true, true));
        assert_eq!(sel("s").write_targets(), ct(true, true));
        assert_eq!(sel("").write_targets(), ct(true, true));
        assert_eq!(sel("s0").write_targets(), ct(true, true));
        assert_eq!(sel("p0").write_targets(), ct(false, true));
        assert!(sel("q").write_targets().is_empty());
        assert!(sel("01").write_targets().is_empty());
        assert!(!sel("c").write_targets().is_empty());
    }

    #[test]
    fn read_target_first_supported_char() {
        assert_eq!(sel("c").read_target(), Some(ClipboardTarget::Clipboard));
        assert_eq!(sel("p").read_target(), Some(ClipboardTarget::Primary));
        assert_eq!(sel("s").read_target(), Some(ClipboardTarget::Clipboard));
        assert_eq!(sel("").read_target(), Some(ClipboardTarget::Clipboard));
        assert_eq!(sel("pc").read_target(), Some(ClipboardTarget::Primary));
        assert_eq!(sel("cp").read_target(), Some(ClipboardTarget::Clipboard));
        assert_eq!(sel("0p").read_target(), Some(ClipboardTarget::Primary));
        assert_eq!(sel("q").read_target(), None);
    }

    #[test]
    fn encode_reply_round_trips() {
        let bytes = encode_reply(&sel("c"), b"hello", false);
        let s = std::str::from_utf8(&bytes).unwrap();
        assert_eq!(s, "\x1b]52;c;aGVsbG8=\x1b\\");
    }

    #[test]
    fn encode_reply_mirrors_bel_terminator() {
        let bytes = encode_reply(&sel("p"), b"hello", true);
        assert_eq!(bytes, b"\x1b]52;p;aGVsbG8=\x07");
    }

    #[test]
    fn encode_reply_uses_first_selection_byte() {
        // `cs` selection → reply with just `c`.
        let bytes = encode_reply(&sel("cs"), b"x", false);
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.starts_with("\x1b]52;c;"));
    }

    #[test]
    fn encode_reply_empty_selection_defaults_to_c() {
        let bytes = encode_reply(&sel(""), b"x", false);
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.starts_with("\x1b]52;c;"));
    }
}
