//! Decoder from on-wire bytes to fully-resident RGBA8 [`ImageData`].
//!
//! Supports three formats from the Kitty graphics protocol:
//! - `f=24` raw RGB (3 bytes per pixel).
//! - `f=32` raw RGBA (4 bytes per pixel).
//! - `f=100` PNG. Decoded via the `image` crate (zero-config — we don't
//!   need to autodetect, the wire format dictates the codec).
//!
//! Raw data is checked against `width * height * bpp` the way kitty
//! 0.49.2 checks it: fewer bytes is `ENODATA`, up to [`RAW_DATA_SLACK`]
//! extra bytes are ignored, more is `EFBIG`.
//!
//! Each [`DecodeError`] carries kitty's reply code and message
//! ([`DecodeError::code`], `Display`), checked live against kitty:
//! - `EINVAL:Unknown image format: N`, `EINVAL:Zero width/height not
//!   allowed`.
//! - `ENODATA:Insufficient image data: GOT < EXPECTED`.
//! - `EFBIG:Too much data`.
//! - `EBADPNG:Not a PNG file`, `EBADPNG:PNG data is truncated: …`, or
//!   `EBADPNG:<decoder message>` for other PNG errors.
//! - `ENOMEM:PNG image is too large`: the PNG's IHDR declares an image
//!   larger than the caller's quota. Checked before any pixel data is
//!   decoded (SEC-7).

use thiserror::Error;

use crate::registry::ImageData;

use super::header::Format;
use super::reply::ErrorCode;

/// Extra bytes past `width * height * bpp` that kitty accepts (and
/// ignores) in a raw upload. Measured on kitty 0.49.2: 1x1 RGBA takes up
/// to 14 bytes, 10x10 up to 410.
pub const RAW_DATA_SLACK: usize = 10;

/// kitty's message for a PNG that ends early.
const PNG_TRUNCATED: &str = "PNG data is truncated: not enough bytes to satisfy read request";

/// Decode errors. The caller maps these onto Kitty error replies with
/// [`DecodeError::code`]; `Display` is kitty's message.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum DecodeError {
    /// `f=` is not 24, 32 or 100.
    #[error("Unknown image format: {0}")]
    UnknownFormat(u32),
    /// Width or height is zero for a raw format.
    #[error("Zero width/height not allowed")]
    MissingDims,
    /// Fewer raw bytes than `width * height * bpp`.
    #[error("Insufficient image data: {got} < {expected}")]
    InsufficientData { got: usize, expected: usize },
    /// More than [`RAW_DATA_SLACK`] bytes past `width * height * bpp`.
    #[error("Too much data")]
    TooMuchData,
    /// PNG decode failed; the message is kitty's where known.
    #[error("{0}")]
    BadPng(String),
    /// The PNG header declares more than `max_bytes` of RGBA8 pixels.
    /// The message matches kitty's `ENOMEM:PNG image is too large`.
    #[error("PNG image is too large")]
    PngTooLarge { width: u32, height: u32 },
}

impl DecodeError {
    /// The reply code kitty uses for this error.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            DecodeError::UnknownFormat(_) | DecodeError::MissingDims => ErrorCode::Einval,
            DecodeError::InsufficientData { .. } => ErrorCode::Enodata,
            DecodeError::TooMuchData => ErrorCode::Efbig,
            DecodeError::BadPng(_) => ErrorCode::Ebadpng,
            DecodeError::PngTooLarge { .. } => ErrorCode::Enomem,
        }
    }
}

/// `width * height * bpp` for a raw format, after kitty's format and
/// dimension checks. `Ok(None)` for PNG (it carries its own size).
pub fn raw_len(format: Format, w: u32, h: u32) -> Result<Option<u64>, DecodeError> {
    let bpp = match format {
        Format::Rgb24 => 3,
        Format::Rgba32 => 4,
        Format::Png => return Ok(None),
        Format::Unknown(n) => return Err(DecodeError::UnknownFormat(n)),
    };
    if w == 0 || h == 0 {
        return Err(DecodeError::MissingDims);
    }
    Ok(Some(u64::from(w) * u64::from(h) * bpp))
}

/// Decode `body` according to `format`. `decl_w` / `decl_h` are the
/// declared source dimensions from the header; for PNG they are
/// ignored (the PNG carries its own size) but for raw formats they are
/// mandatory.
///
/// `max_bytes` bounds the decoded RGBA8 size of a PNG: its IHDR is read
/// first and an image over the limit is rejected with
/// [`DecodeError::PngTooLarge`] before anything is decoded. (Raw formats
/// are already bounded by the size of `body`.)
pub fn decode(
    format: Format,
    body: &[u8],
    decl_w: u32,
    decl_h: u32,
    max_bytes: u64,
) -> Result<ImageData, DecodeError> {
    let Some(expected) = raw_len(format, decl_w, decl_h)? else {
        return decode_png(body, max_bytes);
    };
    let expected = usize::try_from(expected).unwrap_or(usize::MAX);
    if body.len() < expected {
        return Err(DecodeError::InsufficientData {
            got: body.len(),
            expected,
        });
    }
    if body.len() - expected > RAW_DATA_SLACK {
        return Err(DecodeError::TooMuchData);
    }
    let raw = &body[..expected];
    let pixels = if matches!(format, Format::Rgb24) {
        // Expand to RGBA8.
        let mut rgba = Vec::with_capacity(raw.len() / 3 * 4);
        for chunk in raw.chunks_exact(3) {
            rgba.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 0xFF]);
        }
        rgba
    } else {
        raw.to_vec()
    };
    Ok(ImageData {
        width: decl_w,
        height: decl_h,
        pixels,
    })
}

/// Map an `image` error onto kitty's PNG messages.
fn png_error(e: &image::ImageError) -> DecodeError {
    let msg = e.to_string();
    let truncated = matches!(e, image::ImageError::IoError(io) if io.kind() == std::io::ErrorKind::UnexpectedEof)
        || msg.contains("end of file")
        || msg.contains("unexpected EOF");
    DecodeError::BadPng(if truncated {
        PNG_TRUNCATED.to_string()
    } else {
        msg
    })
}

fn decode_png(body: &[u8], max_bytes: u64) -> Result<ImageData, DecodeError> {
    use image::ImageDecoder;
    use std::io::Cursor;
    const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
    // kitty: fewer than 8 bytes is "truncated", a wrong signature is
    // "Not a PNG file".
    if body.len() < SIGNATURE.len() {
        return Err(DecodeError::BadPng(PNG_TRUNCATED.to_string()));
    }
    if body[..SIGNATURE.len()] != SIGNATURE {
        return Err(DecodeError::BadPng("Not a PNG file".to_string()));
    }
    // Build a decoder (reads only the header), and check the *native*
    // decoded size before touching pixel data. `total_bytes()` accounts
    // for bit depth and channel count, so a 16-bit image isn't
    // undercounted the way `w * h * 4` would be (SEC-7; reviewer item 5).
    // A lying IHDR that claims a huge size is caught here too. `max_alloc`
    // makes the image crate's own allocation limit agree with the quota,
    // so an image right at the cap is rejected as ENOMEM rather than a
    // decode error.
    let mut reader = image::ImageReader::with_format(Cursor::new(body), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(max_bytes);
    reader.limits(limits);
    let decoder = reader.into_decoder().map_err(|e| png_error(&e))?;
    let (width, height) = decoder.dimensions();
    if decoder.total_bytes() > max_bytes {
        return Err(DecodeError::PngTooLarge { width, height });
    }
    let img = image::DynamicImage::from_decoder(decoder).map_err(|e| png_error(&e))?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Ok(ImageData {
        width: w,
        height: h,
        pixels: rgba.into_raw(),
    })
}

/// Hand-made PNGs shared by the decode and handler tests.
#[cfg(test)]
pub(crate) mod test_png {
    /// A PNG whose IHDR claims `w`x`h` but carries one pixel of data: a
    /// real 1x1 PNG with its IHDR patched (and the CRC recomputed). Only
    /// a header-first check can tell it is too large; decoding it would
    /// fail on the short image data instead.
    pub(crate) fn claiming(w: u32, h: u32) -> Vec<u8> {
        use image::ImageEncoder;
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&[0, 0, 0, 255], 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        // 8-byte signature, then IHDR: len(4) "IHDR"(4) data(13) crc(4).
        png[16..20].copy_from_slice(&w.to_be_bytes());
        png[20..24].copy_from_slice(&h.to_be_bytes());
        let mut crc = flate2::Crc::new();
        crc.update(&png[12..29]);
        png[29..33].copy_from_slice(&crc.sum().to_be_bytes());
        png
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tiny 2x2 RGBA fixture: all red.
    fn rgba_2x2() -> Vec<u8> {
        vec![
            255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255,
        ]
    }

    /// Tiny 2x2 PNG fixture; encoded at test time so we don't need a
    /// disk blob.
    fn png_2x2() -> Vec<u8> {
        use image::ImageEncoder;
        let mut out = Vec::new();
        let buf = rgba_2x2();
        let encoder = image::codecs::png::PngEncoder::new(&mut out);
        encoder
            .write_image(&buf, 2, 2, image::ExtendedColorType::Rgba8)
            .unwrap();
        out
    }

    #[test]
    fn decode_rgba32_round_trips() {
        let buf = rgba_2x2();
        let img = decode(Format::Rgba32, &buf, 2, 2, u64::MAX).unwrap();
        assert_eq!(img.width, 2);
        assert_eq!(img.height, 2);
        assert_eq!(img.pixels, buf);
    }

    #[test]
    fn decode_rgb24_expands_to_rgba() {
        let buf = vec![255, 0, 0, 0, 255, 0, 0, 0, 255, 128, 128, 128];
        let img = decode(Format::Rgb24, &buf, 2, 2, u64::MAX).unwrap();
        assert_eq!(img.width, 2);
        assert_eq!(img.height, 2);
        assert_eq!(img.pixels.len(), 2 * 2 * 4);
        // First pixel red, alpha 0xFF.
        assert_eq!(&img.pixels[0..4], &[255, 0, 0, 255]);
        // Third pixel blue.
        assert_eq!(&img.pixels[8..12], &[0, 0, 255, 255]);
    }

    /// WIRE-7, against kitty 0.49.2: short raw data is
    /// `ENODATA:Insufficient image data: 7 < 16`.
    #[test]
    fn decode_rgba32_short_is_enodata() {
        let buf = vec![0u8; 7]; // Should be 16.
        let err = decode(Format::Rgba32, &buf, 2, 2, u64::MAX).unwrap_err();
        assert_eq!(
            err,
            DecodeError::InsufficientData {
                got: 7,
                expected: 16
            }
        );
        assert_eq!(err.code(), ErrorCode::Enodata);
        assert_eq!(err.to_string(), "Insufficient image data: 7 < 16");
    }

    /// kitty ignores up to 10 trailing bytes and rejects more with
    /// `EFBIG:Too much data`.
    #[test]
    fn decode_raw_slack_matches_kitty() {
        for (w, h, n, ok) in [
            (1, 1, 14, true),
            (1, 1, 15, false),
            (2, 2, 26, true),
            (2, 2, 27, false),
            (10, 10, 410, true),
            (10, 10, 411, false),
        ] {
            let res = decode(Format::Rgba32, &vec![7u8; n], w, h, u64::MAX);
            if ok {
                assert_eq!(res.unwrap().pixels.len(), (w * h * 4) as usize);
            } else {
                assert_eq!(res.unwrap_err(), DecodeError::TooMuchData);
            }
        }
        assert!(decode(Format::Rgb24, &[0u8; 13], 1, 1, u64::MAX).is_ok());
        let err = decode(Format::Rgb24, &[0u8; 14], 1, 1, u64::MAX).unwrap_err();
        assert_eq!(
            (err.code(), err.to_string().as_str()),
            (ErrorCode::Efbig, "Too much data")
        );
    }

    #[test]
    fn decode_rgba32_missing_dims_is_einval() {
        let buf = vec![0u8; 16];
        for (w, h) in [(0, 2), (2, 0)] {
            let err = decode(Format::Rgba32, &buf, w, h, u64::MAX).unwrap_err();
            assert_eq!(err, DecodeError::MissingDims);
            assert_eq!(err.code(), ErrorCode::Einval);
            assert_eq!(err.to_string(), "Zero width/height not allowed");
        }
    }

    #[test]
    fn decode_unknown_format_is_einval() {
        let err = decode(Format::Unknown(8), &[0u8; 4], 1, 1, u64::MAX).unwrap_err();
        assert_eq!(err.code(), ErrorCode::Einval);
        assert_eq!(err.to_string(), "Unknown image format: 8");
    }

    #[test]
    fn decode_png_round_trips() {
        let buf = png_2x2();
        let img = decode(Format::Png, &buf, 0, 0, u64::MAX).unwrap();
        assert_eq!(img.width, 2);
        assert_eq!(img.height, 2);
        assert_eq!(img.pixels.len(), 2 * 2 * 4);
    }

    /// kitty's PNG messages: a wrong signature, a short or cut-off file.
    #[test]
    fn decode_png_errors_use_kitty_messages() {
        let cases: [(Vec<u8>, &str); 4] = [
            (vec![0xFFu8; 32], "Not a PNG file"),
            (b"junk".to_vec(), PNG_TRUNCATED),
            (png_2x2()[..40].to_vec(), PNG_TRUNCATED),
            (png_2x2()[..20].to_vec(), PNG_TRUNCATED),
        ];
        for (buf, msg) in cases {
            let err = decode(Format::Png, &buf, 0, 0, u64::MAX).unwrap_err();
            assert_eq!(err.code(), ErrorCode::Ebadpng);
            assert_eq!(err.to_string(), msg, "{} bytes", buf.len());
        }
        // Trailing junk after IEND is fine, as in kitty.
        let mut buf = png_2x2();
        buf.extend_from_slice(&[0u8; 100]);
        assert!(decode(Format::Png, &buf, 0, 0, u64::MAX).is_ok());
    }

    #[test]
    fn decode_rgb24_short_is_enodata() {
        // 2x2 RGB = 12 bytes; pass 11.
        let buf = vec![0u8; 11];
        let err = decode(Format::Rgb24, &buf, 2, 2, u64::MAX).unwrap_err();
        assert_eq!(err.to_string(), "Insufficient image data: 11 < 12");
    }
    #[test]
    fn decode_png_over_quota_is_rejected_from_the_header() {
        let err = decode(
            Format::Png,
            &test_png::claiming(12000, 12000),
            0,
            0,
            320 << 20,
        )
        .unwrap_err();
        assert_eq!(
            err,
            DecodeError::PngTooLarge {
                width: 12000,
                height: 12000
            }
        );
        assert_eq!(err.to_string(), "PNG image is too large");
    }

    #[test]
    fn decode_png_quota_boundary() {
        let buf = png_2x2();
        assert!(decode(Format::Png, &buf, 0, 0, 16).is_ok());
        assert!(matches!(
            decode(Format::Png, &buf, 0, 0, 15),
            Err(DecodeError::PngTooLarge { .. })
        ));
    }

    /// A 16-bit PNG decodes to 2 bytes per channel, so its quota check
    /// must use the native decoded size (`total_bytes`), not `w*h*4`
    /// (reviewer item 5). A 2x2 16-bit RGBA image is 32 bytes decoded;
    /// `w*h*4` would under-count it as 16.
    #[test]
    fn decode_png_16bit_uses_native_size_for_quota() {
        use image::ImageEncoder;
        let mut buf = Vec::new();
        image::codecs::png::PngEncoder::new(&mut buf)
            .write_image(
                &[0u8; 2 * 2 * 4 * 2],
                2,
                2,
                image::ExtendedColorType::Rgba16,
            )
            .unwrap();
        // Native size is 32 bytes. A 16-byte quota (what `w*h*4` sees)
        // must reject it; 32 accepts it.
        assert!(matches!(
            decode(Format::Png, &buf, 0, 0, 16),
            Err(DecodeError::PngTooLarge { .. })
        ));
        assert!(decode(Format::Png, &buf, 0, 0, 32).is_ok());
    }
}
