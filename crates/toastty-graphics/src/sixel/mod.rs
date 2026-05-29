//! Sixel (DCS) image decoder. Wraps `icy_sixel` and produces RGBA8
//! `ImageData` for the shared image registry/placement path.

use crate::registry::ImageData;

/// Default cap on decoded sixel pixel count (~100 megapixels).
pub const DEFAULT_MAX_SIXEL_PIXELS: u64 = 100_000_000;

/// Number of sixel color registers we advertise (`icy_sixel`'s palette max).
pub const SIXEL_MAX_COLORS: u16 = 256;

/// Parsed DCS sixel header params (P1/P2/P3) plus the raw body bytes
/// between the `q` and the ST terminator.
#[derive(Debug, Clone, Default)]
pub struct SixelDcs {
    pub p1: Option<u16>, // pixel aspect ratio
    pub p2: Option<u16>, // background / transparency mode
    pub p3: Option<u16>, // grid size
    pub buf: Vec<u8>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SixelError {
    #[error("sixel decode failed: {0}")]
    Decode(String),
    #[error("sixel image too large: {pixels} px exceeds cap {cap}")]
    TooLarge { pixels: u64, cap: u64 },
    #[error("empty sixel image")]
    Empty,
}

#[derive(Debug, Clone)]
pub struct SixelHandler {
    max_pixels: u64,
}

impl Default for SixelHandler {
    fn default() -> Self {
        Self {
            max_pixels: DEFAULT_MAX_SIXEL_PIXELS,
        }
    }
}

impl SixelHandler {
    pub fn new(max_pixels: u64) -> Self {
        Self { max_pixels }
    }

    /// Decode a DCS sixel body into RGBA8 `ImageData`. Pure function —
    /// no registration/placement (the caller owns that).
    pub fn decode(&self, dcs: &SixelDcs) -> Result<ImageData, SixelError> {
        // icy_sixel takes the raw body (everything after `q`, before ST)
        // plus the parsed P1/P2/P3 params as explicit settings.
        let settings = icy_sixel::DcsSettings::new(dcs.p1, dcs.p2, dcs.p3);
        let image = icy_sixel::SixelImage::decode_from_dcs(&dcs.buf, settings)
            .map_err(|e| SixelError::Decode(e.to_string()))?;

        // A zero dimension means the body carried no pixels.
        if image.width == 0 || image.height == 0 {
            return Err(SixelError::Empty);
        }

        let pixels = image.width as u64 * image.height as u64;
        if pixels > self.max_pixels {
            return Err(SixelError::TooLarge {
                pixels,
                cap: self.max_pixels,
            });
        }

        // `image.pixels` is already tightly-packed RGBA8 — move it out
        // directly, no conversion.
        Ok(ImageData {
            width: image.width as u32,
            height: image.height as u32,
            pixels: image.pixels,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode an RGBA image through `icy_sixel`, then split the full
    /// `ESC P <params> q <body> ESC \` sequence into just the body bytes
    /// so we can feed `SixelDcs.buf` (which holds everything between the
    /// `q` and the ST terminator).
    fn encode_body(pixels: Vec<u8>, w: usize, h: usize) -> Vec<u8> {
        let seq = icy_sixel::SixelImage::from_rgba(pixels, w, h)
            .encode()
            .expect("encode");
        let bytes = seq.into_bytes();
        // Body starts right after the first `q` (the DCS final byte).
        let q = bytes.iter().position(|&b| b == b'q').expect("q in DCS");
        let mut body = &bytes[q + 1..];
        // Strip the trailing ST (`ESC \`).
        if body.ends_with(b"\x1b\\") {
            body = &body[..body.len() - 2];
        }
        body.to_vec()
    }

    #[test]
    fn decode_round_trips() {
        // 4x2 image with distinct, saturated corner colors so quantization
        // keeps them recognizable.
        let w = 4;
        let h = 2;
        let mut pixels = vec![0u8; w * h * 4];
        // Top-left red, top-right green, bottom-left blue, bottom-right white.
        let set = |p: &mut [u8], x: usize, y: usize, rgba: [u8; 4]| {
            let i = (y * w + x) * 4;
            p[i..i + 4].copy_from_slice(&rgba);
        };
        set(&mut pixels, 0, 0, [255, 0, 0, 255]);
        set(&mut pixels, 3, 0, [0, 255, 0, 255]);
        set(&mut pixels, 0, 1, [0, 0, 255, 255]);
        set(&mut pixels, 3, 1, [255, 255, 255, 255]);

        let dcs = SixelDcs {
            p1: None,
            p2: None,
            p3: None,
            buf: encode_body(pixels, w, h),
        };

        let out = SixelHandler::default().decode(&dcs).expect("decode");
        // Width round-trips exactly. Height is rounded UP to the sixel
        // cell height (6 rows per sixel band), so a 2-row image decodes
        // back at height 6 — assert it covers the original rows.
        assert_eq!(out.width, w as u32);
        assert!(out.height >= h as u32);
        assert_eq!(out.height % 6, 0, "height should be a multiple of the sixel band");
        assert_eq!(out.pixels.len(), out.width as usize * out.height as usize * 4);

        // Sixel is lossy (palette quantization); assert the top-left
        // corner is "close" to red rather than exactly equal.
        let tl = &out.pixels[0..4];
        assert!(tl[0] > 180, "top-left should be reddish, got {tl:?}");
        assert!(tl[1] < 80, "top-left green channel should be low, got {tl:?}");
        assert!(tl[2] < 80, "top-left blue channel should be low, got {tl:?}");
    }

    #[test]
    fn empty_or_garbage_body_errs() {
        // A malformed repeat count (`!` followed by a value past
        // icy_sixel's 0xffff repeat limit) is rejected as invalid sixel
        // data rather than silently yielding a tiny canvas.
        let dcs = SixelDcs {
            p1: None,
            p2: None,
            p3: None,
            buf: b"!70000~".to_vec(),
        };
        assert!(SixelHandler::default().decode(&dcs).is_err());
    }

    #[test]
    fn too_large_cap() {
        // A valid image larger than the 4px cap (4x2 = 8px).
        let w = 4;
        let h = 2;
        let pixels = vec![255u8; w * h * 4];
        let dcs = SixelDcs {
            p1: None,
            p2: None,
            p3: None,
            buf: encode_body(pixels, w, h),
        };

        let err = SixelHandler::new(4).decode(&dcs).unwrap_err();
        match err {
            // The decoded image is at least w*h pixels (height rounds up
            // to a sixel band), comfortably over the 4px cap.
            SixelError::TooLarge { pixels, cap } => {
                assert!(pixels >= (w * h) as u64);
                assert_eq!(cap, 4);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }
}
