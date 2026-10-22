//! Decoder from on-wire bytes to fully-resident RGBA8 [`ImageData`].
//!
//! Supports three formats from the Kitty graphics protocol:
//! - `f=24` raw RGB (3 bytes per pixel). Validated against
//!   `width * height * 3`.
//! - `f=32` raw RGBA (4 bytes per pixel). Validated against
//!   `width * height * 4`.
//! - `f=100` PNG. Decoded via the `image` crate (zero-config — we don't
//!   need to autodetect, the wire format dictates the codec).
//!
//! Errors map to Kitty's standard reply codes:
//! - `Einval` — dimension/length mismatch, missing dims for raw formats.
//! - `Ebadf` — PNG could not be decoded.
//! - `Enomem` — the PNG's IHDR declares an image larger than the caller's
//!   quota. Checked before any pixel data is decoded (SEC-7).
//! - `Efbig` — the caller's responsibility (we just decode here).

use thiserror::Error;

use crate::registry::ImageData;

use super::header::Format;

/// Decode errors. The caller maps these onto Kitty error replies.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum DecodeError {
    /// Raw RGB/RGBA bytes don't match the declared `(width, height)`.
    #[error("expected {expected} bytes for raw {fmt} {width}x{height}; got {got}")]
    LengthMismatch {
        fmt: &'static str,
        width: u32,
        height: u32,
        expected: usize,
        got: usize,
    },
    /// Width or height is zero where required.
    #[error("missing required dimensions (width={width}, height={height})")]
    MissingDims { width: u32, height: u32 },
    /// PNG decode failed.
    #[error("PNG decode failed: {0}")]
    BadPng(String),
    /// The PNG header declares more than `max_bytes` of RGBA8 pixels.
    /// The message matches kitty's `ENOMEM:PNG image is too large`.
    #[error("PNG image is too large")]
    PngTooLarge { width: u32, height: u32 },
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
    match format {
        Format::Rgba32 => decode_raw(body, decl_w, decl_h, 4, "RGBA"),
        Format::Rgb24 => {
            let rgb = decode_raw_passthrough(body, decl_w, decl_h, 3, "RGB")?;
            // Expand to RGBA8.
            let mut rgba = Vec::with_capacity(rgb.len() / 3 * 4);
            for chunk in rgb.chunks_exact(3) {
                rgba.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 0xFF]);
            }
            Ok(ImageData {
                width: decl_w,
                height: decl_h,
                pixels: rgba,
            })
        }
        Format::Png => decode_png(body, max_bytes),
    }
}

fn decode_raw(
    body: &[u8],
    w: u32,
    h: u32,
    bpp: usize,
    label: &'static str,
) -> Result<ImageData, DecodeError> {
    let raw = decode_raw_passthrough(body, w, h, bpp, label)?;
    Ok(ImageData {
        width: w,
        height: h,
        pixels: raw.to_vec(),
    })
}

fn decode_raw_passthrough<'a>(
    body: &'a [u8],
    w: u32,
    h: u32,
    bpp: usize,
    label: &'static str,
) -> Result<&'a [u8], DecodeError> {
    if w == 0 || h == 0 {
        return Err(DecodeError::MissingDims {
            width: w,
            height: h,
        });
    }
    let expected = (w as usize)
        .checked_mul(h as usize)
        .and_then(|p| p.checked_mul(bpp))
        .ok_or(DecodeError::LengthMismatch {
            fmt: label,
            width: w,
            height: h,
            expected: usize::MAX,
            got: body.len(),
        })?;
    if body.len() != expected {
        return Err(DecodeError::LengthMismatch {
            fmt: label,
            width: w,
            height: h,
            expected,
            got: body.len(),
        });
    }
    Ok(body)
}

fn decode_png(body: &[u8], max_bytes: u64) -> Result<ImageData, DecodeError> {
    use std::io::Cursor;
    let reader = || image::ImageReader::with_format(Cursor::new(body), image::ImageFormat::Png);
    // Header-only pass: a 12000x12000 PNG would otherwise be decoded
    // (~1.3 GB peak) before the registry rejected it.
    let (width, height) = reader()
        .into_dimensions()
        .map_err(|e| DecodeError::BadPng(e.to_string()))?;
    if u64::from(width) * u64::from(height) * 4 > max_bytes {
        return Err(DecodeError::PngTooLarge { width, height });
    }
    let reader = reader();
    let img = reader
        .decode()
        .map_err(|e| DecodeError::BadPng(e.to_string()))?;
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

    #[test]
    fn decode_rgba32_length_mismatch_is_einval() {
        let buf = vec![0u8; 7]; // Should be 16.
        let err = decode(Format::Rgba32, &buf, 2, 2, u64::MAX).unwrap_err();
        assert!(matches!(err, DecodeError::LengthMismatch { .. }));
    }

    #[test]
    fn decode_rgba32_missing_dims_is_einval() {
        let buf = vec![0u8; 16];
        let err = decode(Format::Rgba32, &buf, 0, 2, u64::MAX).unwrap_err();
        assert!(matches!(err, DecodeError::MissingDims { .. }));
        let err = decode(Format::Rgba32, &buf, 2, 0, u64::MAX).unwrap_err();
        assert!(matches!(err, DecodeError::MissingDims { .. }));
    }

    #[test]
    fn decode_png_round_trips() {
        let buf = png_2x2();
        let img = decode(Format::Png, &buf, 0, 0, u64::MAX).unwrap();
        assert_eq!(img.width, 2);
        assert_eq!(img.height, 2);
        assert_eq!(img.pixels.len(), 2 * 2 * 4);
    }

    #[test]
    fn decode_png_garbage_is_ebadf() {
        let buf = vec![0xFFu8; 32]; // not a PNG.
        let err = decode(Format::Png, &buf, 0, 0, u64::MAX).unwrap_err();
        assert!(matches!(err, DecodeError::BadPng(_)));
    }

    #[test]
    fn decode_rgb24_length_mismatch() {
        // 2x2 RGB = 12 bytes; pass 11.
        let buf = vec![0u8; 11];
        let err = decode(Format::Rgb24, &buf, 2, 2, u64::MAX).unwrap_err();
        assert!(matches!(err, DecodeError::LengthMismatch { .. }));
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
}
