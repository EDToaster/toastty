//! OSC 133 — semantic prompt markers (Final Term / iTerm2 extension).
//!
//! Shells emit these markers around the prompt + command + output so the
//! terminal can implement command-level navigation ("jump to previous
//! prompt"). The protocol uses four marker kinds, all on `OSC 133`:
//!
//! - `OSC 133 ; A ; <opts> ST` — start of prompt
//! - `OSC 133 ; B ; <opts> ST` — end of prompt / start of user input
//! - `OSC 133 ; C ; <opts> ST` — start of command output (after Enter)
//! - `OSC 133 ; D ; [exit_code] ; <opts> ST` — command finished
//!
//! We parse the marker kind and (for `D`) the optional integer exit code.
//! All `;` separated options after the kind/exit-code are ignored — those
//! are free-form key/value pairs that downstream features can pick up if
//! needed.

/// Semantic prompt marker kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    /// `OSC 133 ; A` — start of prompt.
    PromptStart,
    /// `OSC 133 ; B` — end of prompt / start of user input.
    PromptEnd,
    /// `OSC 133 ; C` — start of command output.
    CommandStart,
    /// `OSC 133 ; D ; [exit_code]` — command finished. `None` when the
    /// exit code was missing or not a valid integer.
    CommandFinished(Option<i32>),
}

/// Parse the payload following `OSC 133 ; ` into a [`PromptKind`].
///
/// `payload` should be the bytes after the `133;` prefix — i.e. for
/// `OSC 133 ; D ; 0` it would be `b"D;0"` (or `b"D"` if the exit code is
/// absent). Returns `None` if the kind byte is unrecognised.
#[must_use]
pub fn parse(payload: &[u8]) -> Option<PromptKind> {
    let mut parts = payload.split(|&b| b == b';');
    let kind = parts.next()?;
    // The kind is a single ASCII letter; tolerate trailing whitespace.
    let kind_byte = kind.iter().copied().find(|b| !b.is_ascii_whitespace())?;
    match kind_byte {
        b'A' => Some(PromptKind::PromptStart),
        b'B' => Some(PromptKind::PromptEnd),
        b'C' => Some(PromptKind::CommandStart),
        b'D' => {
            // Optional exit code as the next semicolon-separated field.
            // Best-effort parse — non-integer is treated as None so a
            // shell emitting `D;` doesn't trip us up.
            let exit = parts
                .next()
                .and_then(|s| std::str::from_utf8(s).ok())
                .and_then(|s| s.trim().parse::<i32>().ok());
            Some(PromptKind::CommandFinished(exit))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_prompt_start() {
        assert_eq!(parse(b"A"), Some(PromptKind::PromptStart));
    }

    #[test]
    fn parses_prompt_end() {
        assert_eq!(parse(b"B"), Some(PromptKind::PromptEnd));
    }

    #[test]
    fn parses_command_start() {
        assert_eq!(parse(b"C"), Some(PromptKind::CommandStart));
    }

    #[test]
    fn parses_command_finished_with_exit_code() {
        assert_eq!(
            parse(b"D;0"),
            Some(PromptKind::CommandFinished(Some(0)))
        );
        assert_eq!(
            parse(b"D;127"),
            Some(PromptKind::CommandFinished(Some(127)))
        );
    }

    #[test]
    fn parses_command_finished_without_exit_code() {
        assert_eq!(parse(b"D"), Some(PromptKind::CommandFinished(None)));
        // Trailing semicolon with no code → also None.
        assert_eq!(parse(b"D;"), Some(PromptKind::CommandFinished(None)));
    }

    #[test]
    fn parses_command_finished_with_non_integer_exit_falls_through() {
        // Non-numeric exit code (e.g. some shells use "INT" for SIGINT) →
        // treat as None rather than rejecting the marker.
        assert_eq!(
            parse(b"D;INT"),
            Some(PromptKind::CommandFinished(None))
        );
    }

    #[test]
    fn extra_options_after_kind_are_ignored() {
        // `A;cwd=/tmp` — the cwd= field is free-form; we still recognise A.
        assert_eq!(parse(b"A;cwd=/tmp"), Some(PromptKind::PromptStart));
    }

    #[test]
    fn unknown_kind_is_none() {
        assert_eq!(parse(b"Z"), None);
    }

    #[test]
    fn empty_payload_is_none() {
        assert_eq!(parse(b""), None);
    }

    #[test]
    fn whitespace_tolerated() {
        assert_eq!(parse(b" A"), Some(PromptKind::PromptStart));
        assert_eq!(parse(b"D; 42"), Some(PromptKind::CommandFinished(Some(42))));
    }
}
