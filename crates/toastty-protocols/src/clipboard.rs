//! OSC 52 — programmable clipboard.
//!
//! Sequence shape:
//!
//! ```text
//! OSC 52 ; <selection> ; <payload> ST
//! ```
//!
//! - `<selection>`: one or more characters from `c` (clipboard), `p`
//!   (primary), `q` (secondary), `s` (selection — alias for the
//!   default), `0..=7` (cut buffers). We coalesce all variants to
//!   "clipboard" — the OS distinguishes only on Linux, and even there
//!   `c` is by far the most common.
//! - `<payload>`: base64 (STANDARD alphabet, not URL-safe) of the bytes
//!   to write, or the literal `?` to ask the terminal to read its
//!   clipboard and reply with `OSC 52 ; <selection> ; <base64> ST`.
//!
//! ## Security
//!
//! Both write (Set) and read (Query) are guarded by the user's
//! `[security]` config. Defaults are **off** — the binary checks the
//! flags before acting on a parsed op.

#![allow(clippy::doc_markdown)]

use base64::{Engine, engine::general_purpose::STANDARD};

/// Parsed OSC 52 operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Osc52Op {
    /// Write `payload` bytes to the system clipboard.
    Set {
        selection: SelectionChars,
        payload: Vec<u8>,
    },
    /// Read the system clipboard and reply.
    Query { selection: SelectionChars },
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
}

/// Parse the payload past `OSC 52 ;` into an [`Osc52Op`].
///
/// `payload` is the joined `<selection>;<base64-or-?>` bytes. Returns
/// `None` for malformed input.
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
    let decoded = STANDARD.decode(&cleaned).ok()?;
    Some(Osc52Op::Set {
        selection,
        payload: decoded,
    })
}

/// Encode an OSC 52 read reply: `ESC ] 52 ; <selection> ; <base64> ESC \`.
#[must_use]
pub fn encode_reply(selection: &SelectionChars, data: &[u8]) -> Vec<u8> {
    let encoded = STANDARD.encode(data);
    let sel_char = char::from(selection.reply_char());
    format!("\x1b]52;{sel_char};{encoded}\x1b\\").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_set_with_base64() {
        // base64 of "hello" is "aGVsbG8="
        let op = parse(b"c;aGVsbG8=").unwrap();
        match op {
            Osc52Op::Set { selection, payload } => {
                assert_eq!(selection.0, b"c");
                assert_eq!(payload, b"hello");
            }
            Osc52Op::Query { .. } => panic!("expected Set"),
        }
    }

    #[test]
    fn parses_query() {
        let op = parse(b"c;?").unwrap();
        match op {
            Osc52Op::Query { selection } => assert_eq!(selection.0, b"c"),
            Osc52Op::Set { .. } => panic!("expected Query"),
        }
    }

    #[test]
    fn parses_multi_char_selection() {
        // `cs` — write to both clipboard and selection. We pass the
        // full byte string through; the binary handles it as one.
        let op = parse(b"cs;aGVsbG8=").unwrap();
        match op {
            Osc52Op::Set { selection, .. } => assert_eq!(selection.0, b"cs"),
            Osc52Op::Query { .. } => panic!("expected Set"),
        }
    }

    #[test]
    fn malformed_base64_returns_none() {
        // `!!` is not valid base64 STANDARD.
        assert!(parse(b"c;!!").is_none());
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
    fn encode_reply_round_trips() {
        let sel = SelectionChars(b"c".to_vec());
        let bytes = encode_reply(&sel, b"hello");
        let s = std::str::from_utf8(&bytes).unwrap();
        assert_eq!(s, "\x1b]52;c;aGVsbG8=\x1b\\");
    }

    #[test]
    fn encode_reply_uses_first_selection_byte() {
        // `cs` selection → reply with just `c`.
        let sel = SelectionChars(b"cs".to_vec());
        let bytes = encode_reply(&sel, b"x");
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.starts_with("\x1b]52;c;"));
    }

    #[test]
    fn encode_reply_empty_selection_defaults_to_c() {
        let sel = SelectionChars(Vec::new());
        let bytes = encode_reply(&sel, b"x");
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.starts_with("\x1b]52;c;"));
    }
}
