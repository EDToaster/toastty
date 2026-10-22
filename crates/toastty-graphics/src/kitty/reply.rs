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
    /// I/O failure (couldn't read a file).
    Ebadf,
    /// Referenced image doesn't exist.
    Enoent,
    /// Feature not supported by this terminal (e.g. animation).
    Enotsup,
    /// Image too large to fit under the memory cap.
    Efbig,
    /// No room for more animation frame data (kitty: `ENOSPC:Cache size
    /// exceeded cannot add new frames`).
    Enospc,
    /// Not enough image data for the declared size
    /// (`ENODATA:Insufficient image data: GOT < EXPECTED`).
    Enodata,
    /// PNG data could not be decoded.
    Ebadpng,
    /// A chunked query arrived in the middle of an upload (kitty's
    /// `EILSEQ:More payload loading refers to non-existent image`).
    Eilseq,
    /// Decoded image would exceed the image quota (kitty's reply to an
    /// oversized PNG, checked on the IHDR before decoding).
    Enomem,
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
            ErrorCode::Enospc => "ENOSPC",
            ErrorCode::Enomem => "ENOMEM",
            ErrorCode::Enodata => "ENODATA",
            ErrorCode::Ebadpng => "EBADPNG",
            ErrorCode::Eilseq => "EILSEQ",
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
///
/// `placement_id` is echoed as `p=` when non-zero, matching the spec
/// example `ESC_Gi=<id>,p=<placement>;OK ESC\`.
#[must_use]
pub fn encode_ok(image_id: u32, image_number: u32, placement_id: u32) -> Vec<u8> {
    encode(image_id, image_number, placement_id, "OK")
}

/// Encode an error response.
///
/// The body is `"<CODE>:<detail>"` when `detail` is non-empty, just
/// `"<CODE>"` otherwise. Clients commonly match on the leading code
/// only, so the colon-separated detail is informational.
#[must_use]
pub fn encode_error(
    image_id: u32,
    image_number: u32,
    placement_id: u32,
    code: ErrorCode,
    detail: &str,
) -> Vec<u8> {
    let body = if detail.is_empty() {
        code.as_str().to_string()
    } else {
        format!("{}:{}", code.as_str(), detail)
    };
    encode(image_id, image_number, placement_id, &body)
}

/// Encode an `OK` reply to an animation-frame command (`a=f`). kitty
/// tags it with the frame that was created or edited: `i=1,r=2;OK`.
#[must_use]
pub fn encode_frame_ok(image_id: u32, image_number: u32, placement_id: u32, frame: u32) -> Vec<u8> {
    encode_keys(image_id, image_number, placement_id, frame, "OK")
}

/// Encode an error reply to an `a=f` command. Like [`encode_frame_ok`]
/// it carries `r=<frame>`, the frame the command was aimed at:
/// `i=1,r=2;EINVAL:No frame with number: 3 found`.
#[must_use]
pub fn encode_frame_error(
    image_id: u32,
    image_number: u32,
    placement_id: u32,
    frame: u32,
    code: ErrorCode,
    detail: &str,
) -> Vec<u8> {
    let body = if detail.is_empty() {
        code.as_str().to_string()
    } else {
        format!("{}:{}", code.as_str(), detail)
    };
    encode_keys(image_id, image_number, placement_id, frame, &body)
}

/// Lower-level common encoder for `OK` and error bodies.
fn encode(image_id: u32, image_number: u32, placement_id: u32, body: &str) -> Vec<u8> {
    encode_keys(image_id, image_number, placement_id, 0, body)
}

/// Keys in kitty's order: `i`, `I`, `p`, then `r` (frame replies only).
///
/// kitty 0.49.2 writes every key after `i=` with a leading comma, so its
/// reply to an `I=`-only command starts with a stray one (`,I=7;…`). We
/// leave that comma out (see `docs/decisions/kitty-replies.md`).
fn encode_keys(
    image_id: u32,
    image_number: u32,
    placement_id: u32,
    frame: u32,
    body: &str,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + body.len());
    out.extend_from_slice(b"\x1b_G");
    let mut keys = Vec::with_capacity(4);
    if image_id != 0 {
        keys.push(format!("i={image_id}"));
    }
    if image_number != 0 {
        keys.push(format!("I={image_number}"));
    }
    if placement_id != 0 {
        keys.push(format!("p={placement_id}"));
    }
    if frame != 0 {
        keys.push(format!("r={frame}"));
    }
    out.extend_from_slice(keys.join(",").as_bytes());
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
        let bytes = encode_ok(42, 0, 0);
        assert_eq!(bytes, b"\x1b_Gi=42;OK\x1b\\");
    }

    #[test]
    fn ok_reply_with_image_number_only() {
        let bytes = encode_ok(0, 7, 0);
        assert_eq!(bytes, b"\x1b_GI=7;OK\x1b\\");
    }

    #[test]
    fn frame_replies_carry_r() {
        assert_eq!(encode_frame_ok(1, 0, 0, 2), b"\x1b_Gi=1,r=2;OK\x1b\\");
        assert_eq!(encode_frame_ok(0, 7, 0, 2), b"\x1b_GI=7,r=2;OK\x1b\\");
        assert_eq!(encode_frame_ok(1, 0, 5, 2), b"\x1b_Gi=1,p=5,r=2;OK\x1b\\");
        assert_eq!(
            encode_frame_error(
                1,
                0,
                0,
                3,
                ErrorCode::Einval,
                "No frame with number: 9 found"
            ),
            b"\x1b_Gi=1,r=3;EINVAL:No frame with number: 9 found\x1b\\"
        );
    }

    #[test]
    fn ok_reply_with_both_keys() {
        let bytes = encode_ok(5, 7, 0);
        assert_eq!(bytes, b"\x1b_Gi=5,I=7;OK\x1b\\");
    }

    #[test]
    fn ok_reply_no_keys() {
        // Both zero: still legal; the reply just has no `i=`/`I=`.
        let bytes = encode_ok(0, 0, 0);
        assert_eq!(bytes, b"\x1b_G;OK\x1b\\");
    }

    #[test]
    fn ok_reply_echoes_placement_id() {
        // p= is echoed after i= when the client supplied a placement id.
        let bytes = encode_ok(5, 0, 3);
        assert_eq!(bytes, b"\x1b_Gi=5,p=3;OK\x1b\\");
    }

    #[test]
    fn ok_reply_placement_id_with_all_keys() {
        // Field order is i, I, p.
        let bytes = encode_ok(5, 7, 3);
        assert_eq!(bytes, b"\x1b_Gi=5,I=7,p=3;OK\x1b\\");
    }

    #[test]
    fn error_reply_einval() {
        let bytes = encode_error(1, 0, 0, ErrorCode::Einval, "");
        assert_eq!(bytes, b"\x1b_Gi=1;EINVAL\x1b\\");
    }

    #[test]
    fn error_reply_ebadf() {
        let bytes = encode_error(1, 0, 0, ErrorCode::Ebadf, "bad PNG");
        assert_eq!(bytes, b"\x1b_Gi=1;EBADF:bad PNG\x1b\\");
    }

    #[test]
    fn error_reply_enoent() {
        let bytes = encode_error(99, 0, 0, ErrorCode::Enoent, "");
        assert_eq!(bytes, b"\x1b_Gi=99;ENOENT\x1b\\");
    }

    #[test]
    fn error_reply_enotsup() {
        let bytes = encode_error(0, 0, 0, ErrorCode::Enotsup, "animation");
        assert_eq!(bytes, b"\x1b_G;ENOTSUP:animation\x1b\\");
    }

    #[test]
    fn error_reply_efbig() {
        let bytes = encode_error(3, 0, 0, ErrorCode::Efbig, "");
        assert_eq!(bytes, b"\x1b_Gi=3;EFBIG\x1b\\");
    }

    #[test]
    fn error_reply_echoes_placement_id() {
        let bytes = encode_error(1, 0, 9, ErrorCode::Einval, "");
        assert_eq!(bytes, b"\x1b_Gi=1,p=9;EINVAL\x1b\\");
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
