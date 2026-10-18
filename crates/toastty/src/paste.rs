//! Paste payload preparation: sanitising and bracketed paste (DECSET 2004).
//!
//! When the user pastes from the system clipboard, the text goes through
//! two steps before it reaches the PTY:
//!
//! 1. **Sanitise** ([`sanitize_paste`]). The clipboard is untrusted input —
//!    it may hold text copied from a web page, a `cat`'d file or a remote
//!    host — but whatever we write to the PTY is indistinguishable from the
//!    user typing it. We therefore follow xterm's default
//!    `allowPasteControls: false` behaviour and drop every C0 control
//!    except HT, CR and LF, plus DEL (`0x7f`) and the C1 controls
//!    (U+0080–U+009F). That stops a hostile clipboard from sending Ctrl-C,
//!    Ctrl-D, Ctrl-U, cursor keys (`ESC [ A`) or any other escape sequence
//!    to the foreground app. Line breaks are then normalised to CR (`\r`):
//!    CRLF and lone LF both become a single CR, matching xterm, kitty and
//!    Alacritty, because CR is what the Enter key sends — a raw-mode app
//!    without bracketed paste would otherwise see Ctrl-J.
//! 2. **Wrap** ([`wrap_for_paste`]). If the foreground app has opted in via
//!    `\x1b[?2004h`, the sanitised payload is wrapped in
//!    `\x1b[200~ ... \x1b[201~`. Apps that haven't opted in get the
//!    sanitised text alone — same as typing it character by character.
//!
//! Why bracketed paste matters: shells (bash, zsh, fish) and editors (vim,
//! neovim, helix) use the `200~`/`201~` boundaries to suppress auto-indent,
//! completion and line execution while pasted text streams through, which
//! would otherwise mangle multi-line snippets. See `xterm/ctlseqs`
//! "Bracketed Paste Mode".
//!
//! Why sanitising also closes the bracketed-paste injection hole: the end
//! marker can only be forged with ESC (`0x1b`) or its 8-bit C1 equivalent
//! CSI (U+009B), and both are stripped. Without that, a payload such as
//! `ls\x1b[201~\x15touch /tmp/PWNED\r` would end the paste early and let the
//! rest run as typed input. Filtering works on whole characters and drops
//! them outright (it never splices bytes together), so removal cannot
//! re-form a marker from fragments like `\x1b\x00[201~`; the remaining
//! printable text can contain `[201~` but never the introducer.

/// Bracketed-paste start marker (`ESC [ 200 ~`).
pub const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
/// Bracketed-paste end marker (`ESC [ 201 ~`).
pub const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";

/// True for characters that must never reach the PTY from a paste: C0
/// controls other than HT, LF and CR; DEL; and the C1 controls
/// U+0080–U+009F (which an 8-bit-aware or UTF-8 C1-honouring app would
/// treat like `ESC`-prefixed sequences, e.g. U+009B = CSI).
///
/// Mirrors xterm's `allowPasteControls: false` default.
fn is_disallowed_paste_char(c: char) -> bool {
    match c {
        '\t' | '\n' | '\r' => false,
        c => c.is_ascii_control() || ('\u{80}'..='\u{9f}').contains(&c),
    }
}

/// Sanitise clipboard `text` for writing to the PTY.
///
/// Drops disallowed control characters (see the module docs), then
/// normalises line breaks: CRLF and lone LF become CR, and existing lone
/// CRs are kept. Filtering happens *before* line-break normalisation, so a
/// CR and LF separated only by stripped controls (`\r\x00\n`) still collapse
/// to one CR rather than producing a spurious blank line.
#[must_use]
pub fn sanitize_paste(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text
        .chars()
        .filter(|&c| !is_disallowed_paste_char(c))
        .peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                // CRLF -> CR: swallow the LF half of the pair.
                chars.next_if_eq(&'\n');
                out.push('\r');
            }
            '\n' => out.push('\r'),
            c => out.push(c),
        }
    }
    out
}

/// Prepare `text` for transmission to the PTY: sanitise it with
/// [`sanitize_paste`] and, when `bracketed` is `true`, wrap the result as
/// `\x1b[200~<text>\x1b[201~`.
///
/// Sanitising applies in both modes: unbracketed pastes are where a stray
/// Ctrl-C or escape sequence does the most damage, and in bracketed mode it
/// is what guarantees the payload cannot contain a forged
/// [`BRACKETED_PASTE_END`]. The caller has already decided which mode to
/// use based on [`toastty_term::Term::bracketed_paste`].
#[must_use]
pub fn wrap_for_paste(text: &str, bracketed: bool) -> Vec<u8> {
    let clean = sanitize_paste(text);
    if !bracketed {
        return clean.into_bytes();
    }
    let mut out =
        Vec::with_capacity(BRACKETED_PASTE_START.len() + clean.len() + BRACKETED_PASTE_END.len());
    out.extend_from_slice(BRACKETED_PASTE_START);
    out.extend_from_slice(clean.as_bytes());
    out.extend_from_slice(BRACKETED_PASTE_END);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Count occurrences of `needle` in `hay`.
    fn count(hay: &[u8], needle: &[u8]) -> usize {
        hay.windows(needle.len()).filter(|w| *w == needle).count()
    }

    #[test]
    fn unbracketed_returns_plain_bytes() {
        assert_eq!(wrap_for_paste("hello", false), b"hello".to_vec());
    }

    #[test]
    fn bracketed_wraps_with_csi_markers() {
        assert_eq!(
            wrap_for_paste("hello", true),
            b"\x1b[200~hello\x1b[201~".to_vec()
        );
    }

    #[test]
    fn empty_string_bracketed() {
        // Edge case: even an empty paste sends the markers, so apps observe
        // the boundary.
        assert_eq!(wrap_for_paste("", true), b"\x1b[200~\x1b[201~".to_vec());
    }

    #[test]
    fn empty_string_unbracketed() {
        assert_eq!(wrap_for_paste("", false), Vec::<u8>::new());
    }

    #[test]
    fn multiline_paste_converts_lf_to_cr() {
        assert_eq!(
            wrap_for_paste("line1\nline2\n", true),
            b"\x1b[200~line1\rline2\r\x1b[201~".to_vec()
        );
        assert_eq!(
            wrap_for_paste("line1\nline2\n", false),
            b"line1\rline2\r".to_vec()
        );
    }

    #[test]
    fn crlf_collapses_to_single_cr() {
        assert_eq!(sanitize_paste("a\r\nb\r\n"), "a\rb\r");
        assert_eq!(wrap_for_paste("a\r\nb", false), b"a\rb".to_vec());
    }

    #[test]
    fn lone_cr_is_kept() {
        assert_eq!(sanitize_paste("a\rb\r"), "a\rb\r");
    }

    #[test]
    fn mixed_line_endings() {
        // LF CR is two breaks (LF -> CR, then the lone CR), CR CR LF is two
        // breaks (lone CR, then CRLF), and a blank line survives as CR CR.
        assert_eq!(sanitize_paste("a\n\rb"), "a\r\rb");
        assert_eq!(sanitize_paste("a\r\r\nb"), "a\r\rb");
        assert_eq!(sanitize_paste("a\n\nb"), "a\r\rb");
        assert_eq!(sanitize_paste("a\r\n\r\nb"), "a\r\rb");
    }

    #[test]
    fn crlf_split_by_stripped_control_still_collapses() {
        // Filtering runs before newline normalisation.
        assert_eq!(sanitize_paste("a\r\x00\nb"), "a\rb");
        assert_eq!(sanitize_paste("a\r\x1b\x7f\u{9b}\nb"), "a\rb");
    }

    #[test]
    fn tabs_are_kept() {
        assert_eq!(sanitize_paste("\tif x:\n\t\tpass"), "\tif x:\r\t\tpass");
    }

    #[test]
    fn c0_controls_are_stripped() {
        for b in 0u8..0x20 {
            let c = char::from(b);
            let got = sanitize_paste(&format!("a{c}b"));
            match c {
                '\t' => assert_eq!(got, "a\tb"),
                '\n' | '\r' => assert_eq!(got, "a\rb"),
                _ => assert_eq!(got, "ab", "C0 {b:#04x} should be stripped"),
            }
        }
    }

    #[test]
    fn del_is_stripped() {
        assert_eq!(sanitize_paste("ab\x7fc"), "abc");
    }

    #[test]
    fn c1_controls_are_stripped() {
        for cp in 0x80u32..=0x9f {
            let c = char::from_u32(cp).unwrap();
            assert_eq!(
                sanitize_paste(&format!("a{c}b")),
                "ab",
                "C1 U+{cp:04X} should be stripped"
            );
        }
        // Boundaries: U+007E and U+00A0 (NBSP) are ordinary text.
        assert_eq!(sanitize_paste("~\u{a0}"), "~\u{a0}");
    }

    #[test]
    fn audit_control_bytes_are_stripped_unbracketed() {
        // Audit #10: Ctrl-C, Ctrl-D, cursor-up and DEL used to arrive
        // verbatim when bracketed paste was off.
        assert_eq!(
            wrap_for_paste("x\x03y\x04z\x1b[A\x7f", false),
            b"xyz[A".to_vec()
        );
    }

    #[test]
    fn audit_injection_payload_is_neutralised() {
        // Audit #1: an embedded end marker must not terminate the paste
        // early (and the Ctrl-U line-kill must not get through either).
        let got = wrap_for_paste("ls\x1b[201~\x15touch /tmp/PWNED\r", true);
        assert_eq!(got, b"\x1b[200~ls[201~touch /tmp/PWNED\r\x1b[201~".to_vec());
        assert_eq!(count(&got, BRACKETED_PASTE_END), 1);
        assert!(got.ends_with(BRACKETED_PASTE_END));
    }

    #[test]
    fn embedded_start_marker_is_neutralised() {
        let got = wrap_for_paste("a\x1b[200~b", true);
        assert_eq!(got, b"\x1b[200~a[200~b\x1b[201~".to_vec());
        assert_eq!(count(&got, BRACKETED_PASTE_START), 1);
    }

    #[test]
    fn c1_csi_marker_is_neutralised() {
        // U+009B is the 8-bit CSI; `CSI 201 ~` is an end marker to apps
        // that honour C1 in UTF-8.
        let got = wrap_for_paste("a\u{9b}201~b", true);
        assert_eq!(got, b"\x1b[200~a201~b\x1b[201~".to_vec());
    }

    #[test]
    fn removal_cannot_reform_a_marker() {
        // Every way of interleaving stripped characters into a marker must
        // leave no ESC (and hence no marker) behind.
        let junk = ["\x00", "\x1b", "\x7f", "\u{9b}", "\u{85}", "\x15"];
        for j in junk {
            for payload in [
                format!("\x1b{j}[201~"),
                format!("{j}\x1b[201~{j}"),
                format!("\x1b[{j}201~"),
                "\x1b\x1b[201~[201~".to_string(),
                "\x1b[20\x1b[201~1~".to_string(),
            ] {
                for bracketed in [false, true] {
                    let got = wrap_for_paste(&payload, bracketed);
                    let inner = if bracketed {
                        &got[BRACKETED_PASTE_START.len()..got.len() - BRACKETED_PASTE_END.len()]
                    } else {
                        &got[..]
                    };
                    assert!(
                        !inner.contains(&0x1b),
                        "ESC survived in {payload:?} -> {inner:?}"
                    );
                    assert_eq!(count(&got, BRACKETED_PASTE_END), usize::from(bracketed));
                }
            }
        }
    }

    #[test]
    fn utf8_paste_passes_through() {
        let s = "café 🥐 日本語 \u{301}\u{200d}\u{fe0f}";
        let got = wrap_for_paste(s, true);
        assert_eq!(
            got,
            [BRACKETED_PASTE_START, s.as_bytes(), BRACKETED_PASTE_END].concat()
        );
    }

    #[test]
    fn markers_are_canonical() {
        assert_eq!(BRACKETED_PASTE_START, b"\x1b[200~");
        assert_eq!(BRACKETED_PASTE_END, b"\x1b[201~");
    }
}
