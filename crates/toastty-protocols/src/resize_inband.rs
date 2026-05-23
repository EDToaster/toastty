//! DECSET 2048 — in-band resize notifications.
//!
//! When mode 2048 is enabled, the terminal emits a resize report on
//! the PTY's read side every time the geometry changes:
//!
//! ```text
//! CSI 48 ; <rows> ; <cols> ; <pixel_h> ; <pixel_w> t
//! ```
//!
//! This replaces SIGWINCH for apps that opt in. SIGWINCH still fires
//! through the kernel — mode 2048 just adds a synchronous in-stream
//! variant so apps see the new dimensions in order with everything
//! else, no race.
//!
//! The encoder is purely a `Vec<u8>` builder; the binary calls it from
//! `Event::Resize` after the term/PTY have already been resized.

/// Encode the in-band resize report for mode 2048. Returns `None` when
/// the mode isn't enabled — the caller can `if let Some(bytes) =
/// encode_resize_report(...)` and skip the PTY write otherwise.
///
/// `rows` / `cols` are the new cell-grid dimensions; `pixel_height` /
/// `pixel_width` are the physical pixel dimensions of the surface.
#[must_use]
pub fn encode_resize_report(
    rows: u16,
    cols: u16,
    pixel_height: u16,
    pixel_width: u16,
    enabled: bool,
) -> Option<Vec<u8>> {
    if !enabled {
        return None;
    }
    // `CSI 48 ; rows ; cols ; pixel_h ; pixel_w t`. We build the
    // string directly — the format strings are simple enough that the
    // allocator cost is well below the PTY-write cost.
    let body = format!(
        "\x1b[48;{rows};{cols};{pixel_height};{pixel_width}t",
        rows = rows,
        cols = cols,
        pixel_height = pixel_height,
        pixel_width = pixel_width,
    );
    Some(body.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_returns_none() {
        assert!(encode_resize_report(24, 80, 480, 640, false).is_none());
    }

    #[test]
    fn enabled_emits_csi_48_t_with_dims() {
        let out = encode_resize_report(24, 80, 480, 640, true).expect("Some when enabled");
        assert!(out.starts_with(b"\x1b[48;"));
        assert!(out.ends_with(b"t"));
    }

    #[test]
    fn dimensions_are_rendered_decimal() {
        let out = encode_resize_report(7, 11, 13, 17, true).unwrap();
        let s = std::str::from_utf8(&out).unwrap();
        assert_eq!(s, "\x1b[48;7;11;13;17t");
    }

    #[test]
    fn zero_dims_still_round_trip() {
        // Defensive: 0 dims are bogus but the encoder still emits a
        // well-formed sequence — terminating drivers tolerate it.
        let out = encode_resize_report(0, 0, 0, 0, true).unwrap();
        assert_eq!(std::str::from_utf8(&out).unwrap(), "\x1b[48;0;0;0;0t");
    }

    #[test]
    fn max_dims_render_correctly() {
        let out = encode_resize_report(u16::MAX, u16::MAX, u16::MAX, u16::MAX, true).unwrap();
        let s = std::str::from_utf8(&out).unwrap();
        assert_eq!(s, "\x1b[48;65535;65535;65535;65535t");
    }

    #[test]
    fn term_then_encode_round_trip_via_decset() {
        // End-to-end-ish: feed DECSET 2048 into a Term, then ask the
        // encoder for a resize report using that Term's
        // `inband_resize_mode()` flag. The encoder must produce bytes
        // post-enable and `None` post-disable.
        use toastty_parser::Parser;
        use toastty_term::Term;

        let mut t = Term::new(2, 4, 0);
        let mut p = Parser::new();
        // Enable.
        p.advance(&mut t, b"\x1b[?2048h");
        let report =
            encode_resize_report(24, 80, 480, 640, t.inband_resize_mode()).expect("Some when on");
        assert_eq!(
            std::str::from_utf8(&report).unwrap(),
            "\x1b[48;24;80;480;640t"
        );
        // Disable.
        p.advance(&mut t, b"\x1b[?2048l");
        assert!(encode_resize_report(24, 80, 480, 640, t.inband_resize_mode()).is_none());
    }
}
