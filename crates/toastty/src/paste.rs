//! Bracketed paste (DECSET 2004).
//!
//! When the user pastes from the system clipboard, we wrap the payload in
//! `\x1b[200~ ... \x1b[201~` if the foreground app has opted in via
//! `\x1b[?2004h`. Apps that haven't opted in get the raw text — same as
//! typing it character by character.
//!
//! Why bracketed paste matters: shells (zsh, fish) and editors (vim,
//! neovim, helix) use the `200~`/`201~` boundaries to suppress auto-indent
//! and completion while pasted text streams through, which would otherwise
//! mangle multi-line snippets. See `xterm/ctlseqs` "Bracketed Paste Mode".

/// Bracketed-paste start marker (`ESC [ 200 ~`).
pub const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
/// Bracketed-paste end marker (`ESC [ 201 ~`).
pub const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";

/// Wrap `text` for transmission to the PTY, honouring the app's bracketed
/// paste preference.
///
/// When `bracketed` is `true`, the result is `\x1b[200~<text>\x1b[201~`.
/// When `false`, the bytes of `text` are passed through unchanged. The
/// caller has already decided which to use based on
/// [`toastty_term::Term::bracketed_paste`].
///
/// Embedded `\x1b[201~` markers in the pasted text are *not* sanitised here
/// — that's a subtle anti-injection concern that real terminals (xterm,
/// kitty) handle by stripping or filtering. We document this as a known
/// gap and leave a TODO; see `docs/milestones/m07-modern-input.md`.
#[must_use]
pub fn wrap_for_paste(text: &str, bracketed: bool) -> Vec<u8> {
    if !bracketed {
        return text.as_bytes().to_vec();
    }
    let mut out = Vec::with_capacity(BRACKETED_PASTE_START.len() + text.len() + BRACKETED_PASTE_END.len());
    out.extend_from_slice(BRACKETED_PASTE_START);
    // TODO(paste-injection): strip embedded `\x1b[201~` so the host app
    // can't be tricked into exiting paste mode mid-payload.
    out.extend_from_slice(text.as_bytes());
    out.extend_from_slice(BRACKETED_PASTE_END);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(wrap_for_paste("", false).is_empty());
    }

    #[test]
    fn multiline_paste_preserves_newlines() {
        let got = wrap_for_paste("line1\nline2\n", true);
        assert!(got.starts_with(b"\x1b[200~line1\nline2\n"));
        assert!(got.ends_with(b"\x1b[201~"));
    }

    #[test]
    fn utf8_paste_passes_through() {
        let s = "café 🥐";
        let got = wrap_for_paste(s, true);
        assert_eq!(
            got,
            [
                BRACKETED_PASTE_START,
                s.as_bytes(),
                BRACKETED_PASTE_END
            ]
            .concat()
        );
    }

    #[test]
    fn markers_are_canonical() {
        assert_eq!(BRACKETED_PASTE_START, b"\x1b[200~");
        assert_eq!(BRACKETED_PASTE_END, b"\x1b[201~");
    }
}
