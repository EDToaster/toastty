//! Encoder for Kitty graphics protocol response payloads.
//!
//! Replies are themselves APC sequences: `ESC _ G <key>=<value>,...
//! ;<message> ESC \`. The `i=` / `I=` keys echo the originating image
//! id/number so the client knows which transmit succeeded; the body is
//! `OK` for success or a human-readable error message for failure.
//!
//! Reference: <https://sw.kovidgoyal.net/kitty/graphics-protocol/#response-from-the-terminal>.

/// Standardized error codes the terminal can return.
///
/// Names mirror kitty's literal strings — these go on the wire in the
/// body of the reply and clients string-match against them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// Invalid input (bad parameter combo, dimension mismatch).
    Einval,
    /// I/O failure (couldn't read file, bad PNG decode).
    Ebadf,
    /// Referenced image doesn't exist.
    Enoent,
    /// Feature not supported by this terminal (e.g. animation).
    Enotsup,
    /// Image too large to fit under the memory cap.
    Efbig,
    /// Relative placement references a parent image/placement that does
    /// not exist.
    Enoparent,
    /// Relative placement would create a cycle in the parent chain.
    Ecycle,
    /// Relative placement chain exceeds the maximum allowed depth.
    Etoodeep,
}

impl ErrorCode {
    /// The literal string kitty's reference docs assign to each code.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::Einval => "EINVAL",
            ErrorCode::Ebadf => "EBADF",
            ErrorCode::Enoent => "ENOENT",
            ErrorCode::Enotsup => "ENOTSUP",
            ErrorCode::Efbig => "EFBIG",
            ErrorCode::Enoparent => "ENOPARENT",
            ErrorCode::Ecycle => "ECYCLE",
            ErrorCode::Etoodeep => "ETOODEEP",
        }
    }
}

/// Encode an `OK` response for a successful transmit.
///
/// At least one of `image_id` or `image_number` should be non-zero
/// (kitty's spec says the reply should echo whichever identifier the
/// client used). We emit both keys when both are non-zero — kitty's
/// docs are explicit that the terminal MAY do that.
#[must_use]
pub fn encode_ok(image_id: u32, image_number: u32) -> Vec<u8> {
    encode(image_id, image_number, "OK")
}

/// Encode an error response.
///
/// The body is `"<CODE>:<detail>"` when `detail` is non-empty, just
/// `"<CODE>"` otherwise. Clients commonly match on the leading code
/// only, so the colon-separated detail is informational.
#[must_use]
pub fn encode_error(image_id: u32, image_number: u32, code: ErrorCode, detail: &str) -> Vec<u8> {
    let body = if detail.is_empty() {
        code.as_str().to_string()
    } else {
        format!("{}:{}", code.as_str(), detail)
    };
    encode(image_id, image_number, &body)
}

/// Lower-level common encoder for `OK` and error bodies.
fn encode(image_id: u32, image_number: u32, body: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + body.len());
    out.extend_from_slice(b"\x1b_G");
    let mut first = true;
    if image_id != 0 {
        out.extend_from_slice(format!("i={image_id}").as_bytes());
        first = false;
    }
    if image_number != 0 {
        if !first {
            out.push(b',');
        }
        out.extend_from_slice(format!("I={image_number}").as_bytes());
    }
    out.push(b';');
    out.extend_from_slice(body.as_bytes());
    out.extend_from_slice(b"\x1b\\");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_reply_basic() {
        let bytes = encode_ok(42, 0);
        assert_eq!(bytes, b"\x1b_Gi=42;OK\x1b\\");
    }

    #[test]
    fn ok_reply_with_image_number_only() {
        let bytes = encode_ok(0, 7);
        assert_eq!(bytes, b"\x1b_GI=7;OK\x1b\\");
    }

    #[test]
    fn ok_reply_with_both_keys() {
        let bytes = encode_ok(5, 7);
        assert_eq!(bytes, b"\x1b_Gi=5,I=7;OK\x1b\\");
    }

    #[test]
    fn ok_reply_no_keys() {
        // Both zero: still legal; the reply just has no `i=`/`I=`.
        let bytes = encode_ok(0, 0);
        assert_eq!(bytes, b"\x1b_G;OK\x1b\\");
    }

    #[test]
    fn error_reply_einval() {
        let bytes = encode_error(1, 0, ErrorCode::Einval, "");
        assert_eq!(bytes, b"\x1b_Gi=1;EINVAL\x1b\\");
    }

    #[test]
    fn error_reply_ebadf() {
        let bytes = encode_error(1, 0, ErrorCode::Ebadf, "bad PNG");
        assert_eq!(bytes, b"\x1b_Gi=1;EBADF:bad PNG\x1b\\");
    }

    #[test]
    fn error_reply_enoent() {
        let bytes = encode_error(99, 0, ErrorCode::Enoent, "");
        assert_eq!(bytes, b"\x1b_Gi=99;ENOENT\x1b\\");
    }

    #[test]
    fn error_reply_enotsup() {
        let bytes = encode_error(0, 0, ErrorCode::Enotsup, "animation");
        assert_eq!(bytes, b"\x1b_G;ENOTSUP:animation\x1b\\");
    }

    #[test]
    fn error_reply_efbig() {
        let bytes = encode_error(3, 0, ErrorCode::Efbig, "");
        assert_eq!(bytes, b"\x1b_Gi=3;EFBIG\x1b\\");
    }

    #[test]
    fn error_code_str_matches_kitty_spec() {
        assert_eq!(ErrorCode::Einval.as_str(), "EINVAL");
        assert_eq!(ErrorCode::Ebadf.as_str(), "EBADF");
        assert_eq!(ErrorCode::Enoent.as_str(), "ENOENT");
        assert_eq!(ErrorCode::Enotsup.as_str(), "ENOTSUP");
        assert_eq!(ErrorCode::Efbig.as_str(), "EFBIG");
        assert_eq!(ErrorCode::Enoparent.as_str(), "ENOPARENT");
        assert_eq!(ErrorCode::Ecycle.as_str(), "ECYCLE");
        assert_eq!(ErrorCode::Etoodeep.as_str(), "ETOODEEP");
    }
}
