//! OSC 7 — current working directory.
//!
//! Shells (zsh/bash/fish) emit `OSC 7 ; file://<host>/<percent-encoded-path> ST`
//! to advertise the current working directory. We parse the `file://` URL into
//! a plain UTF-8 path string. The host segment is discarded (we don't
//! validate against the local hostname — that's the caller's problem if it
//! cares).
//!
//! The grammar we accept is permissive on purpose:
//!
//! ```text
//! file://[host]/[percent-encoded-path]
//! ```
//!
//! - The leading `file://` is required.
//! - Everything up to the next `/` is treated as the host and ignored.
//! - The remaining bytes are percent-decoded (each `%XX` → byte) and
//!   returned as a UTF-8 string (lossy on bad UTF-8, so a single rogue byte
//!   can't crash the terminal).
//!
//! Returns `None` if the payload doesn't start with `file://` or if the
//! percent-decoded bytes are empty.

#![allow(clippy::doc_markdown)]

/// Parse a `file://` URL payload into a plain path string.
///
/// Returns `None` if the payload doesn't start with `file://`.
#[must_use]
pub fn parse_file_url(payload: &[u8]) -> Option<String> {
    const PREFIX: &[u8] = b"file://";
    if !payload.starts_with(PREFIX) {
        return None;
    }
    let after_scheme = &payload[PREFIX.len()..];
    // Drop the host segment: everything up to the next '/'. If there's no
    // '/', the URL points at a hostless target which is invalid — bail.
    let slash_idx = after_scheme.iter().position(|&b| b == b'/')?;
    let path_bytes = &after_scheme[slash_idx..];
    if path_bytes.is_empty() {
        return None;
    }
    let decoded = percent_decode(path_bytes);
    if decoded.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&decoded).into_owned())
}

/// Hand-rolled `%XX` decoder. Bytes that aren't part of a `%XX` triplet are
/// copied through unchanged. Malformed `%` sequences at the tail of the
/// input (e.g. trailing `%` or `%X`) are passed through literally — they
/// can't cause a panic.
fn percent_decode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%'
            && i + 2 < input.len()
            && let (Some(hi), Some(lo)) = (hex_value(input[i + 1]), hex_value(input[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(input[i]);
        i += 1;
    }
    out
}

const fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_path() {
        assert_eq!(
            parse_file_url(b"file://localhost/home/user").as_deref(),
            Some("/home/user")
        );
    }

    #[test]
    fn parses_no_host_segment() {
        // `file:///home/user` — empty host. The first `/` after the scheme
        // is the path root.
        assert_eq!(
            parse_file_url(b"file:///home/user").as_deref(),
            Some("/home/user")
        );
    }

    #[test]
    fn rejects_non_file_url() {
        assert_eq!(parse_file_url(b"http://example.com/"), None);
    }

    #[test]
    fn rejects_missing_path_slash() {
        // `file://host` has no path — we can't tell where the host ends.
        assert_eq!(parse_file_url(b"file://host"), None);
    }

    #[test]
    fn percent_decodes_path() {
        assert_eq!(
            parse_file_url(b"file:///home/space%20user/x").as_deref(),
            Some("/home/space user/x")
        );
    }

    #[test]
    fn percent_decodes_multibyte_utf8() {
        // `%E4%BD%A0` is U+4F60 ("你") in UTF-8.
        let bytes = b"file:///tmp/%E4%BD%A0";
        let decoded = parse_file_url(bytes).expect("ok");
        assert_eq!(decoded, "/tmp/你");
    }

    #[test]
    fn percent_decode_passes_through_bad_triplets() {
        // Trailing `%` with nothing after — should not panic; should leave
        // the `%` in place.
        let out = percent_decode(b"a%");
        assert_eq!(out, b"a%");
        let out = percent_decode(b"a%X");
        assert_eq!(out, b"a%X");
        // Mid-string malformed: `%GG` (G is not hex) — passes through.
        let out = percent_decode(b"%GG");
        assert_eq!(out, b"%GG");
    }

    #[test]
    fn empty_payload_returns_none() {
        assert_eq!(parse_file_url(b""), None);
    }
}
