//! Ground-state UTF-8 clean-up in front of vte (WIRE-14).
//!
//! vte 0.15 decodes ground text itself, but it hands a bare byte in
//! `0x80..=0x9F` (invalid UTF-8) to `execute` as an 8-bit C1 control,
//! so it vanishes, and it only partly follows the "maximal subpart"
//! rule for longer invalid runs. kitty 0.49.2 (checked live) shows one
//! U+FFFD for every maximal invalid subpart, exactly as
//! [`String::from_utf8_lossy`] does, bare C1 bytes included. Properly
//! encoded C1 code points (`\xc2\x9c`) stay controls in both.
//!
//! [`Utf8Sanitizer`] rewrites ground text to valid UTF-8 that way
//! before vte sees it. An incomplete sequence at the end of a chunk is
//! held back (at most 3 bytes) until the next chunk completes or breaks
//! it; one cut off by ESC becomes U+FFFD, as in vte and kitty.
//!
//! Cost: ASCII text costs one `is_ascii` scan. Other text is checked with
//! SIMD (`simdutf8`) up to its last character boundary, which is valid
//! in almost every chunk (a 4096-byte read of CJK nearly always ends
//! partway through a character, so validating the whole chunk would
//! fail). Only the 0..=3-byte tail, or a prefix that really is invalid,
//! goes through the slower `std::str::from_utf8` loop.

/// UTF-8 encoding of U+FFFD REPLACEMENT CHARACTER.
const REPLACEMENT: &[u8] = "\u{FFFD}".as_bytes();

/// See the module docs.
#[derive(Debug, Default)]
pub(crate) struct Utf8Sanitizer {
    /// Valid but incomplete sequence carried over from the last chunk.
    tail: [u8; 3],
    tail_len: usize,
}

/// Length of the longest prefix of `bytes` that doesn't end partway
/// through a character: looks back at most 3 bytes for a lead byte whose
/// sequence runs past the end. Says nothing about validity.
fn complete_prefix_len(bytes: &[u8]) -> usize {
    let n = bytes.len();
    for back in 1..=n.min(3) {
        let b = bytes[n - back];
        if b < 0x80 {
            return n;
        }
        if b >= 0xC0 {
            let need = if b >= 0xF0 {
                4
            } else if b >= 0xE0 {
                3
            } else {
                2
            };
            return if need > back { n - back } else { n };
        }
    }
    n
}

impl Utf8Sanitizer {
    /// True while an incomplete sequence from an earlier chunk is held.
    pub(crate) fn holds_partial(&self) -> bool {
        self.tail_len > 0
    }

    /// The span `[start, end)` of `bytes` (one parser input chunk) whose
    /// ground text can skip this sanitizer: valid UTF-8 that starts and
    /// ends on character boundaries. Checked once per chunk (`is_ascii`,
    /// else SIMD), so escape-heavy output doesn't pay a check for every
    /// short run between escapes.
    ///
    /// - `start` skips the continuation bytes that complete a held
    ///   character: the sanitizer must see those first. After them it
    ///   holds nothing, unless they are all of `bytes`. If a held
    ///   character is broken instead (a non-continuation byte comes
    ///   first), the span is empty and the whole chunk is sanitized.
    /// - `end` stops before a trailing partial character, which the
    ///   sanitizer must hold.
    /// - An invalid chunk gives an empty span: all of it is sanitized.
    pub(crate) fn clean_span(&self, bytes: &[u8]) -> (usize, usize) {
        let start = if self.tail_len == 0 {
            0
        } else {
            let lead = self.tail[0];
            let need: usize = if lead >= 0xF0 {
                4
            } else if lead >= 0xE0 {
                3
            } else {
                2
            };
            let want = need.saturating_sub(self.tail_len);
            let cont = bytes
                .iter()
                .take(want)
                .take_while(|b| (0x80..0xC0).contains(*b))
                .count();
            if cont < want && cont < bytes.len() {
                return (0, 0); // Broken across chunks: rare, sanitize all.
            }
            cont
        };
        // One SIMD pass covers ASCII too (simdutf8 has its own ASCII
        // fast path, cheaper than `is_ascii`).
        let rest = &bytes[start..];
        let cut = complete_prefix_len(rest);
        if simdutf8::basic::from_utf8(&rest[..cut]).is_ok() {
            (start, start + cut)
        } else {
            (start, start)
        }
    }

    /// Clean `bytes` (ground text) and pass the result to `out`, in
    /// order, possibly in several pieces. `cut_by_esc` says an ESC
    /// follows directly, so an incomplete trailing sequence is final.
    pub(crate) fn feed(&mut self, mut bytes: &[u8], cut_by_esc: bool, out: &mut impl FnMut(&[u8])) {
        if self.tail_len > 0 {
            bytes = self.finish_tail(bytes, cut_by_esc, out);
            if self.tail_len > 0 {
                return; // Still incomplete; `bytes` was used up.
            }
        }
        if bytes.is_ascii() {
            if !bytes.is_empty() {
                out(bytes);
            }
            return;
        }
        // Fast path: SIMD-validate everything up to the last character
        // boundary; usually that is all of it but a partial character.
        let cut = complete_prefix_len(bytes);
        if simdutf8::basic::from_utf8(&bytes[..cut]).is_ok() {
            if cut > 0 {
                out(&bytes[..cut]);
            }
            bytes = &bytes[cut..];
            if bytes.is_empty() {
                return;
            }
        }
        loop {
            match std::str::from_utf8(bytes) {
                Ok(_) => {
                    if !bytes.is_empty() {
                        out(bytes);
                    }
                    return;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    if valid > 0 {
                        out(&bytes[..valid]);
                    }
                    if let Some(len) = e.error_len() {
                        out(REPLACEMENT);
                        bytes = &bytes[valid + len..];
                    } else {
                        // Incomplete sequence at the end (1..=3 bytes).
                        let rest = &bytes[valid..];
                        if cut_by_esc {
                            out(REPLACEMENT);
                        } else {
                            self.tail[..rest.len()].copy_from_slice(rest);
                            self.tail_len = rest.len();
                        }
                        return;
                    }
                }
            }
        }
    }

    /// Complete (or break) the held-back sequence with the first bytes of
    /// `bytes`. Returns what's left of `bytes`.
    fn finish_tail<'b>(
        &mut self,
        bytes: &'b [u8],
        cut_by_esc: bool,
        out: &mut impl FnMut(&[u8]),
    ) -> &'b [u8] {
        let held = self.tail_len;
        let mut buf = [0u8; 4];
        buf[..held].copy_from_slice(&self.tail[..held]);
        let take = bytes.len().min(4 - held);
        buf[held..held + take].copy_from_slice(&bytes[..take]);
        let joined = &buf[..held + take];
        let (emit_len, used) = match std::str::from_utf8(joined) {
            Ok(s) => {
                let n = s.chars().next().map_or(0, char::len_utf8);
                (n, n)
            }
            Err(e) if e.valid_up_to() > 0 => {
                let n = std::str::from_utf8(&joined[..e.valid_up_to()])
                    .ok()
                    .and_then(|s| s.chars().next())
                    .map_or(0, char::len_utf8);
                (n, n)
            }
            Err(e) => {
                let Some(len) = e.error_len() else {
                    // Still incomplete; everything in `bytes` was taken.
                    if cut_by_esc {
                        out(REPLACEMENT);
                        self.tail_len = 0;
                    } else {
                        self.tail[..held + take].copy_from_slice(joined);
                        self.tail_len = held + take;
                    }
                    return &bytes[take..];
                };
                // Broken: one U+FFFD for the maximal subpart, which may
                // reach into `bytes`.
                (0, len)
            }
        };
        if emit_len > 0 {
            out(&joined[..emit_len]);
        } else {
            out(REPLACEMENT);
        }
        self.tail_len = 0;
        // `used` counts the held bytes too; the rest came from `bytes`.
        &bytes[used.saturating_sub(held)..]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed `chunks` (ground text, no ESC) and return the cleaned bytes.
    fn clean(chunks: &[&[u8]]) -> Vec<u8> {
        let mut s = Utf8Sanitizer::default();
        let mut v = Vec::new();
        for c in chunks {
            s.feed(c, false, &mut |b| v.extend_from_slice(b));
        }
        v
    }

    /// The kitty 0.49.2 results from the live matrix, as text.
    #[test]
    fn matches_kitty_replacement_counts() {
        let r = "\u{FFFD}";
        let cases: &[(&[u8], String)] = &[
            (b"\x80", r.into()),
            (b"\x9c", r.into()),
            (b"\x9f", r.into()),
            (b"\xa0", r.into()),
            (b"\xff", r.into()),
            (b"\x9c\x9c", r.repeat(2)),
            (b"\x90x", format!("{r}x")),
            (b"\x9b31m", format!("{r}31m")),
            (b"\xc0\xaf", r.repeat(2)),
            (b"\xe2\x82A", format!("{r}A")),
            (b"\xed\xa0\x80", r.repeat(3)),
            (b"\xf4\x90\x80\x80", r.repeat(4)),
            (b"\xf0\x9fB", format!("{r}B")),
            (b"\xe2\x82\xac", "\u{20AC}".into()),
            // Encoded C1 stays as is (vte executes it; a no-op).
            (b"\xc2\x9c", "\u{9C}".into()),
        ];
        for (input, want) in cases {
            assert_eq!(clean(&[input]), want.as_bytes(), "input {input:x?}");
            assert_eq!(
                String::from_utf8_lossy(input).as_bytes(),
                want.as_bytes(),
                "same rule as from_utf8_lossy, {input:x?}"
            );
        }
    }

    #[test]
    fn split_sequences_match_the_unsplit_result() {
        let inputs: &[&[u8]] = &[
            b"a\xe2\x82\xacb",
            b"a\xe2\x82Ab",
            b"\xf0\x9f\x98\x80!",
            b"\xf0\x9fB",
            b"\xf4\x90\x80\x80",
            b"\xed\xa0\x80",
            b"x\xc2\x9cy",
        ];
        for input in inputs {
            let whole = clean(&[input]);
            for cut in 1..input.len() {
                let (a, b) = input.split_at(cut);
                assert_eq!(clean(&[a, b]), whole, "{input:x?} cut {cut}");
            }
            // Byte by byte.
            let bytes: Vec<&[u8]> = input.chunks(1).collect();
            assert_eq!(clean(&bytes), whole, "{input:x?} bytewise");
        }
    }

    #[test]
    fn incomplete_sequence_cut_by_esc_is_replaced() {
        let mut s = Utf8Sanitizer::default();
        let mut v = Vec::new();
        s.feed(b"a\xe2\x82", true, &mut |b| v.extend_from_slice(b));
        assert_eq!(v, "a\u{FFFD}".as_bytes());
        // Held across a chunk, then the ESC arrives.
        let mut s = Utf8Sanitizer::default();
        let mut v = Vec::new();
        s.feed(b"\xe2", false, &mut |b| v.extend_from_slice(b));
        assert!(v.is_empty());
        s.feed(b"", true, &mut |b| v.extend_from_slice(b));
        assert_eq!(v, "\u{FFFD}".as_bytes());
        s.feed(b"ok", false, &mut |b| v.extend_from_slice(b));
        assert_eq!(v, "\u{FFFD}ok".as_bytes());
    }

    #[test]
    fn complete_prefix_stops_before_a_partial_character() {
        assert_eq!(complete_prefix_len(b""), 0);
        assert_eq!(complete_prefix_len(b"abc"), 3);
        assert_eq!(complete_prefix_len("a€".as_bytes()), 4);
        assert_eq!(complete_prefix_len(b"a\xe2\x82"), 1);
        assert_eq!(complete_prefix_len(b"a\xf0\x9f\x98"), 1);
        assert_eq!(complete_prefix_len("😀".as_bytes()), 4);
        // Not a validity check: stray continuations and bad leads pass.
        assert_eq!(complete_prefix_len(b"a\x80\x80\x80"), 4);
        assert_eq!(complete_prefix_len(b"a\xff"), 1);
    }

    #[test]
    fn clean_span_covers_valid_text_up_to_the_last_boundary() {
        let s = Utf8Sanitizer::default();
        assert_eq!(s.clean_span(b"plain ascii"), (0, 11));
        assert_eq!(s.clean_span("한글".as_bytes()), (0, 6));
        assert_eq!(
            s.clean_span(b"\xed\x95\x9c\xea"),
            (0, 3),
            "partial last char"
        );
        assert_eq!(
            s.clean_span(b"bad \x9c here"),
            (0, 0),
            "invalid: all sanitized"
        );
    }

    #[test]
    fn clean_span_starts_after_completing_a_held_character() {
        let mut s = Utf8Sanitizer::default();
        let mut v = Vec::new();
        s.feed(b"\xe2\x82", false, &mut |b| v.extend_from_slice(b));
        assert!(s.holds_partial());
        // The held € finishes with 0xAC; the span starts after it.
        assert_eq!(s.clean_span(b"\xacxyz"), (1, 4));
        // Broken by a non-continuation byte: sanitize the whole chunk.
        assert_eq!(s.clean_span(b"xyz"), (0, 0));
        // Too short to finish it: nothing left to skip.
        let mut s = Utf8Sanitizer::default();
        s.feed(b"\xf0", false, &mut |b| v.extend_from_slice(b));
        assert_eq!(s.clean_span(b"\x9f\x98"), (2, 2));
    }
}
