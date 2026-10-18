//! Plain-text URL detection for click-to-open.
//!
//! OSC 8 hyperlinks carry their target explicitly; everything else a
//! program prints (`echo https://…`, compiler output, `git log`, man
//! pages in `less`) is just text. Like kitty, Alacritty, `WezTerm` and
//! iTerm2 we recognise URLs in that text with a conservative scanner so
//! the user can Ctrl+click (Cmd+click on macOS) them.
//!
//! The scanner works on one *logical* line of text (the binary joins
//! soft-wrapped rows first) and returns byte ranges. The heuristics follow
//! the common terminal conventions (Alacritty's default URL regex and
//! kitty's `url_excluded_characters` / trailing-punctuation rules):
//!
//! - A URL starts at a known scheme — see [`SCHEMES`] — that isn't glued
//!   to a preceding word character (`xhttp://` is not a URL).
//! - It extends over characters that may appear in a URL: printable ASCII
//!   except `<`, `>`, `"`, backtick, `{`, `|`, `}`, `^` and `\`, and any
//!   non-ASCII character except whitespace, controls and the angle
//!   brackets `⟨⟩` (RFC 3987 IRIs allow Unicode). Whitespace and controls always end it.
//! - `(…)` and `[…]` must balance inside the URL: an unmatched closer ends
//!   it. That keeps Wikipedia links (`…/Rust_(programming_language)`) and
//!   IPv6 hosts (`http://[::1]:8080/`) whole while dropping the closer of
//!   `(see https://example.com)` and of Markdown `[text](https://…)`.
//! - Trailing sentence punctuation (`. , ; : ! ? ' "`) and unmatched
//!   trailing openers are stripped: `Visit https://example.com.` links
//!   `https://example.com`.
//! - Something meaningful must follow the scheme (at least one
//!   alphanumeric), so a bare `https://` is not a link.
//!
//! Only the schemes in [`SCHEMES`] are ever detected, so `javascript:` /
//! `data:` text can never become clickable. For OSC 8, whose URL comes
//! from the application, [`is_safe_to_open`] applies a denylist of
//! schemes that execute code or smuggle content into a browser.

use std::ops::Range;

/// Scheme prefixes recognised in plain text, matched ASCII
/// case-insensitively. `mailto:` has no `//`; the rest are hierarchical.
pub const SCHEMES: &[&str] = &[
    "https://", "http://", "file://", "ftp://", "ftps://", "sftp://", "ssh://", "git://", "mailto:",
];

/// Schemes [`is_safe_to_open`] refuses: they run script in the browser
/// (`javascript:`, `vbscript:`), inline arbitrary content (`data:`,
/// `blob:`), or address browser internals (`about:`, `chrome:`, …).
const DENIED_SCHEMES: &[&str] = &[
    "javascript",
    "vbscript",
    "data",
    "blob",
    "about",
    "chrome",
    "view-source",
    "jar",
];

/// Characters stripped from the end of a candidate URL: sentence
/// punctuation that is far more often prose than part of the link.
const TRAILING_PUNCT: &[char] = &['.', ',', ';', ':', '!', '?', '\'', '"'];

/// True for characters that may appear inside a detected URL.
#[must_use]
pub fn is_url_char(c: char) -> bool {
    if c.is_whitespace() || c.is_control() {
        return false;
    }
    if c.is_ascii() {
        return c.is_ascii_graphic()
            && !matches!(c, '<' | '>' | '"' | '`' | '{' | '|' | '}' | '^' | '\\');
    }
    !matches!(c, '\u{27E8}' | '\u{27E9}' | '\u{200B}' | '\u{FEFF}')
}

/// True when the character before a scheme glues it to a word, so the
/// scheme isn't the start of a URL (`xhttp://`, `git+ssh://`).
fn glues_to_scheme(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '+' | '-' | '.' | '_')
}

/// Length in bytes of the scheme prefix starting at `text[at..]`, if any.
fn scheme_len_at(text: &str, at: usize) -> Option<usize> {
    let rest = text.as_bytes().get(at..)?;
    SCHEMES
        .iter()
        .find(|s| rest.len() >= s.len() && rest[..s.len()].eq_ignore_ascii_case(s.as_bytes()))
        .map(|s| s.len())
}

/// Scan the URL body starting at byte `start` (just past the scheme) and
/// return the exclusive end byte after bracket balancing (before
/// trailing-punctuation trimming).
fn scan_body(text: &str, start: usize) -> usize {
    let mut stack: Vec<char> = Vec::new();
    for (i, c) in text[start..].char_indices() {
        if !is_url_char(c) {
            return start + i;
        }
        match c {
            '(' | '[' => stack.push(c),
            ')' | ']' => {
                let opener = if c == ')' { '(' } else { '[' };
                if stack.last() == Some(&opener) {
                    stack.pop();
                } else {
                    return start + i;
                }
            }
            _ => {}
        }
    }
    text.len()
}

/// Trim trailing punctuation and unmatched trailing openers from
/// `text[start..end]`, returning the new end.
fn trim_end(text: &str, start: usize, mut end: usize) -> usize {
    while let Some(c) = text[start..end].chars().next_back() {
        if TRAILING_PUNCT.contains(&c) || c == '(' || c == '[' {
            end -= c.len_utf8();
        } else {
            break;
        }
    }
    end
}

/// Every URL in `text`, as non-overlapping byte ranges in order.
#[must_use]
pub fn find_urls(text: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut prev: Option<char> = None;
    let mut i = 0;
    while i < text.len() {
        let c = text[i..].chars().next().unwrap_or(' ');
        if !prev.is_some_and(glues_to_scheme)
            && let Some(len) = scheme_len_at(text, i)
        {
            let body = i + len;
            let end = trim_end(text, body, scan_body(text, body));
            if text[body..end].chars().any(char::is_alphanumeric) {
                out.push(i..end);
                prev = text[..end].chars().next_back();
                i = end;
                continue;
            }
        }
        prev = Some(c);
        i += c.len_utf8();
    }
    out
}

/// The URL in `text` whose byte range contains byte offset `byte`.
#[must_use]
pub fn url_at(text: &str, byte: usize) -> Option<Range<usize>> {
    find_urls(text).into_iter().find(|r| r.contains(&byte))
}

/// RFC 3986 §3.1 scheme of `url` (`ALPHA *( ALPHA / DIGIT / "+" / "-" /
/// "." )` before the first `:`), lowercased.
#[must_use]
pub fn scheme_of(url: &str) -> Option<String> {
    let (scheme, _) = url.split_once(':')?;
    let mut chars = scheme.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic()
        || !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return None;
    }
    Some(scheme.to_ascii_lowercase())
}

/// Whether `url` may be handed to the system opener. Requires a valid
/// scheme that isn't in the denylist (script / inline-content / browser
/// internal schemes). Leading whitespace or controls — which browsers
/// strip, turning ` javascript:` into script — make it unsafe too.
#[must_use]
pub fn is_safe_to_open(url: &str) -> bool {
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return false;
    }
    scheme_of(url).is_some_and(|s| !DENIED_SCHEMES.contains(&s.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: [&str; 0] = [];

    /// All detected URLs as strings.
    fn urls(text: &str) -> Vec<&str> {
        find_urls(text).into_iter().map(|r| &text[r]).collect()
    }

    fn one(text: &str) -> &str {
        let v = urls(text);
        assert_eq!(v.len(), 1, "expected one URL in {text:?}, got {v:?}");
        v[0]
    }

    #[test]
    fn plain_http_and_https() {
        assert_eq!(one("http://example.com"), "http://example.com");
        assert_eq!(one("see https://example.com now"), "https://example.com");
    }

    #[test]
    fn every_scheme_is_detected() {
        for s in SCHEMES {
            let text = format!("x {s}host/path y");
            assert_eq!(urls(&text), vec![format!("{s}host/path")], "{s}");
        }
    }

    #[test]
    fn scheme_is_case_insensitive() {
        assert_eq!(one("HTTPS://Example.COM/A"), "HTTPS://Example.COM/A");
        assert_eq!(one("MailTo:me@x.org"), "MailTo:me@x.org");
    }

    #[test]
    fn unknown_and_dangerous_schemes_are_not_detected() {
        assert_eq!(urls("javascript:alert(1)"), NONE);
        assert_eq!(urls("data:text/html,<script>"), NONE);
        assert_eq!(urls("vbscript:msgbox"), NONE);
        assert_eq!(urls("gopher://x.org"), NONE);
        assert_eq!(urls("www.example.com"), NONE);
    }

    #[test]
    fn scheme_glued_to_word_is_not_a_url() {
        assert_eq!(urls("xhttp://example.com"), NONE);
        assert_eq!(urls("git+ssh://host/repo"), NONE);
        assert_eq!(urls("a.http://b"), NONE);
        assert_eq!(urls("_https://b"), NONE);
        // Punctuation before the scheme is fine.
        assert_eq!(one("=https://a.b"), "https://a.b");
        assert_eq!(one("(https://a.b)"), "https://a.b");
    }

    #[test]
    fn bare_scheme_is_not_a_url() {
        assert_eq!(urls("https://"), NONE);
        assert_eq!(urls("https:// x"), NONE);
        assert_eq!(urls("http://..."), NONE);
        assert_eq!(urls("mailto:"), NONE);
        assert_eq!(urls("http://-/"), NONE);
    }

    #[test]
    fn ports_query_and_fragment() {
        assert_eq!(
            one("GET http://localhost:8080/api/v1?x=1&y=two#frag ok"),
            "http://localhost:8080/api/v1?x=1&y=two#frag"
        );
        assert_eq!(
            one("https://a.b/?q=a%20b+c;d=e"),
            "https://a.b/?q=a%20b+c;d=e"
        );
        assert_eq!(
            one("https://user:pw@host:22/~me/"),
            "https://user:pw@host:22/~me/"
        );
    }

    #[test]
    fn ipv6_hosts() {
        assert_eq!(one("http://[::1]:8080/x"), "http://[::1]:8080/x");
        assert_eq!(
            one("open http://[2001:db8::1]/ please"),
            "http://[2001:db8::1]/"
        );
        assert_eq!(one("http://[fe80::1%25eth0]"), "http://[fe80::1%25eth0]");
    }

    #[test]
    fn trailing_punctuation_is_stripped() {
        for p in [
            ".", ",", ";", ":", "!", "?", "'", "\"", "...", "?!", ".'", "'.",
        ] {
            let text = format!("go to https://example.com/a{p}");
            assert_eq!(one(&text), "https://example.com/a", "{p:?}");
        }
    }

    #[test]
    fn interior_punctuation_is_kept() {
        assert_eq!(
            one("https://a.b/c.d,e;f:g!h?i'j"),
            "https://a.b/c.d,e;f:g!h?i'j"
        );
    }

    #[test]
    fn balanced_parentheses_are_kept() {
        assert_eq!(
            one("https://en.wikipedia.org/wiki/Rust_(programming_language)"),
            "https://en.wikipedia.org/wiki/Rust_(programming_language)"
        );
        assert_eq!(one("https://a.b/f(x(y))z"), "https://a.b/f(x(y))z");
        assert_eq!(one("https://a.b/[1][2]"), "https://a.b/[1][2]");
    }

    #[test]
    fn wikipedia_link_in_parens_and_with_period() {
        assert_eq!(
            one("(see https://en.wikipedia.org/wiki/Foo_(bar))."),
            "https://en.wikipedia.org/wiki/Foo_(bar)"
        );
    }

    #[test]
    fn unmatched_closers_end_the_url() {
        assert_eq!(one("(https://example.com)"), "https://example.com");
        assert_eq!(one("[https://example.com]"), "https://example.com");
        assert_eq!(one("https://a.b/x)y"), "https://a.b/x");
        // Mismatched pair: `(` then `]`.
        assert_eq!(one("https://a.b/(x]"), "https://a.b/(x");
    }

    #[test]
    fn unmatched_trailing_opener_is_stripped() {
        assert_eq!(one("https://a.b/x("), "https://a.b/x");
        assert_eq!(one("https://a.b/x[."), "https://a.b/x");
    }

    #[test]
    fn angle_brackets_and_quotes() {
        assert_eq!(one("<https://example.com/a>"), "https://example.com/a");
        assert_eq!(one("\"https://example.com/a\""), "https://example.com/a");
        assert_eq!(one("'https://example.com/a'"), "https://example.com/a");
        assert_eq!(one("`https://example.com/a`"), "https://example.com/a");
        assert_eq!(one("{https://example.com/a}"), "https://example.com/a");
    }

    #[test]
    fn markdown_link() {
        let text = "[the docs](https://docs.rs/winit/latest)";
        assert_eq!(one(text), "https://docs.rs/winit/latest");
        let text = "[https://a.org](https://b.org)";
        assert_eq!(urls(text), vec!["https://a.org", "https://b.org"]);
    }

    #[test]
    fn stops_at_whitespace_and_invalid_chars() {
        assert_eq!(one("https://a.b/c d"), "https://a.b/c");
        assert_eq!(one("https://a.b/c\td"), "https://a.b/c");
        assert_eq!(one("https://a.b/c|grep"), "https://a.b/c");
        assert_eq!(one("https://a.b/c\\d"), "https://a.b/c");
        assert_eq!(one("https://a.b/c^d"), "https://a.b/c");
        assert_eq!(one("https://a.b/c\u{7}d"), "https://a.b/c");
        assert_eq!(one("https://a.b/c\u{00A0}d"), "https://a.b/c");
        assert_eq!(one("https://a.b/c\u{3000}d"), "https://a.b/c");
        assert_eq!(one("https://a.b/c\u{27E8}d"), "https://a.b/c");
        assert_eq!(one("https://a.b/c\u{200B}d"), "https://a.b/c");
    }

    #[test]
    fn unicode_iri() {
        assert_eq!(
            one("https://例え.jp/パス?q=値 x"),
            "https://例え.jp/パス?q=値"
        );
        assert_eq!(
            one("https://de.wikipedia.org/wiki/Größe."),
            "https://de.wikipedia.org/wiki/Größe"
        );
        assert_eq!(one("→https://a.b/😀←"), "https://a.b/😀←");
        // Combining mark stays with its base.
        assert_eq!(one("https://a.b/e\u{301}"), "https://a.b/e\u{301}");
    }

    #[test]
    fn unicode_letter_before_scheme_glues() {
        assert_eq!(urls("éhttps://a.b"), NONE);
    }

    #[test]
    fn multiple_urls_per_line() {
        assert_eq!(
            urls("a http://one.com, b https://two.org/x; c file:///tmp/f."),
            vec!["http://one.com", "https://two.org/x", "file:///tmp/f"]
        );
        assert_eq!(
            urls("http://a.b http://c.d"),
            vec!["http://a.b", "http://c.d"]
        );
    }

    #[test]
    fn embedded_url_in_query_is_part_of_outer_url() {
        assert_eq!(
            urls("https://a.b/?next=https://c.d/e"),
            vec!["https://a.b/?next=https://c.d/e"]
        );
    }

    #[test]
    fn comma_joined_urls_stay_one_url() {
        // `,` is a valid URL character mid-URL, so this is one link;
        // `<>` delimiters do split.
        assert_eq!(
            urls("https://a.b,https://c.d"),
            vec!["https://a.b,https://c.d"]
        );
        assert_eq!(
            urls("<https://a.b><https://c.d>"),
            vec!["https://a.b", "https://c.d"]
        );
    }

    #[test]
    fn file_and_mailto() {
        assert_eq!(
            one("file:///home/me/a%20b.txt"),
            "file:///home/me/a%20b.txt"
        );
        assert_eq!(
            one("file://localhost/etc/hosts"),
            "file://localhost/etc/hosts"
        );
        assert_eq!(one("Mail mailto:me@example.org."), "mailto:me@example.org");
        assert_eq!(
            one("mailto:a@b.c?subject=hi%20there"),
            "mailto:a@b.c?subject=hi%20there"
        );
    }

    #[test]
    fn url_at_end_of_text() {
        let text = "end https://a.b/c";
        assert_eq!(find_urls(text), vec![4..text.len()]);
    }

    #[test]
    fn empty_and_no_url_text() {
        assert_eq!(find_urls(""), vec![]);
        assert_eq!(find_urls("just words: http and https"), vec![]);
        assert_eq!(find_urls("h"), vec![]);
    }

    #[test]
    fn url_at_finds_containing_range() {
        let text = "a https://x.y/z b http://q.r";
        assert_eq!(url_at(text, 2), Some(2..15));
        assert_eq!(url_at(text, 14), Some(2..15));
        assert_eq!(url_at(text, 15), None);
        assert_eq!(url_at(text, 0), None);
        assert_eq!(url_at(text, 20), Some(18..text.len()));
        assert_eq!(url_at(text, 999), None);
    }

    #[test]
    fn is_url_char_classes() {
        assert!(is_url_char('a'));
        assert!(is_url_char('~'));
        assert!(is_url_char('é'));
        assert!(!is_url_char(' '));
        assert!(!is_url_char('\n'));
        assert!(!is_url_char('\u{85}'));
        assert!(!is_url_char('<'));
        assert!(!is_url_char('}'));
    }

    #[test]
    fn scheme_parsing() {
        assert_eq!(scheme_of("HTTPS://x").as_deref(), Some("https"));
        assert_eq!(scheme_of("git+ssh://x").as_deref(), Some("git+ssh"));
        assert_eq!(scheme_of("vscode://file/x").as_deref(), Some("vscode"));
        assert_eq!(scheme_of("no-colon"), None);
        assert_eq!(scheme_of(":x"), None);
        assert_eq!(scheme_of("1http://x"), None);
        assert_eq!(scheme_of("ht tp://x"), None);
    }

    #[test]
    fn safe_to_open() {
        assert!(is_safe_to_open("https://example.com"));
        assert!(is_safe_to_open("file:///tmp/x"));
        assert!(is_safe_to_open("mailto:a@b.c"));
        assert!(is_safe_to_open("vscode://file/a"));
        assert!(!is_safe_to_open("javascript:alert(1)"));
        assert!(!is_safe_to_open("JavaScript:alert(1)"));
        assert!(!is_safe_to_open("data:text/html,x"));
        assert!(!is_safe_to_open("vbscript:x"));
        assert!(!is_safe_to_open("blob:https://a"));
        assert!(!is_safe_to_open("about:config"));
        assert!(!is_safe_to_open(" javascript:alert(1)"));
        assert!(!is_safe_to_open("java\tscript:alert(1)"));
        assert!(!is_safe_to_open("https://a.b/\nx"));
        assert!(!is_safe_to_open("relative/path"));
        assert!(!is_safe_to_open(""));
    }
}
