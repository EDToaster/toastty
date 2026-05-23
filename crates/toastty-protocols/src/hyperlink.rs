//! OSC 8 — hyperlinks.
//!
//! Sequence shape (Gnome / iTerm2 / xterm extension):
//!
//! ```text
//! OSC 8 ; <params> ; <url> ST
//! ```
//!
//! `<params>` is a `:`-separated `key=value` list. Most common is
//! `id=<some-string>` for grouping cells that should be treated as a
//! single hyperlink. `<url>` is the destination URL; an empty URL closes
//! the active hyperlink (`OSC 8 ; ; ST`).
//!
//! We parse both fields. The `id=` value is currently ignored — our
//! intern table keys hyperlinks by URL, which naturally dedupes cells
//! that belong to the same link.

/// Result of parsing an OSC 8 payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HyperlinkParams<'a> {
    /// Optional `id=...` value parsed from the params slot. Currently
    /// unused (URL is the dedup key) but exposed for future use.
    pub id: Option<&'a str>,
    /// Destination URL. Empty `&""` means "close current hyperlink".
    pub url: &'a str,
}

/// Parse the payload past `OSC 8 ;`.
///
/// `payload` is the byte slice for the `<params>;<url>` portion — i.e.
/// for `OSC 8 ; id=foo ; https://example.com` you'd pass
/// `b"id=foo;https://example.com"`. Returns `None` if the payload isn't
/// valid UTF-8.
#[must_use]
pub fn parse(payload: &[u8]) -> Option<HyperlinkParams<'_>> {
    let s = std::str::from_utf8(payload).ok()?;
    // Split at the first ';' separator between params and URL. Anything
    // past that (a stray ';' inside a URL — rare but legal in some
    // form-encoded URLs) is treated as part of the URL.
    let (params, url) = match s.split_once(';') {
        Some((p, u)) => (p, u),
        None => ("", s),
    };
    let id = parse_id(params);
    Some(HyperlinkParams { id, url })
}

fn parse_id(params: &str) -> Option<&str> {
    // Params is `key=value[:key=value]*`. Walk the `:`-separated bits;
    // first `id=...` wins.
    for kv in params.split(':') {
        if let Some(v) = kv.strip_prefix("id=") {
            return Some(v);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_url_only() {
        let p = parse(b";https://example.com").unwrap();
        assert_eq!(p.id, None);
        assert_eq!(p.url, "https://example.com");
    }

    #[test]
    fn parses_id_and_url() {
        let p = parse(b"id=foo;https://example.com").unwrap();
        assert_eq!(p.id, Some("foo"));
        assert_eq!(p.url, "https://example.com");
    }

    #[test]
    fn closer_has_empty_url() {
        let p = parse(b";").unwrap();
        assert_eq!(p.url, "");
        // Single-segment payload — taken as URL.
        let p = parse(b"").unwrap();
        assert_eq!(p.url, "");
    }

    #[test]
    fn extra_params_ignored() {
        let p = parse(b"id=foo:custom=bar;https://example.com").unwrap();
        assert_eq!(p.id, Some("foo"));
        assert_eq!(p.url, "https://example.com");
    }

    #[test]
    fn semicolon_in_url_is_preserved() {
        // `split_once(';')` cuts on the first separator only — anything
        // after stays in the URL.
        let p = parse(b";https://x.com/?a=1;b=2").unwrap();
        assert_eq!(p.url, "https://x.com/?a=1;b=2");
    }

    #[test]
    fn rejects_invalid_utf8() {
        let bytes = [0xff_u8, 0xfe, 0xfd];
        assert!(parse(&bytes).is_none());
    }
}
