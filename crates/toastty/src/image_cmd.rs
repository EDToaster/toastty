//! `toastty image [--sixel] <FILE|->...`: print images inline.
//!
//! Runs inside a shell (no GUI is started). Each image is decoded with
//! the `image` crate (PNG, JPEG, GIF, WebP, BMP) and written to stdout
//! as a Kitty graphics transmit-and-display (`a=T`, PNG payload,
//! chunked) or, with `--sixel`, as a Sixel DCS. Either is followed by a
//! newline so the prompt lands below the image, and consecutive images
//! are separated by a blank line. Images are shown at their native
//! pixel size; no scaling to the window is done.

use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use image::{ImageFormat, RgbaImage};

/// Which graphics protocol to emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Protocol {
    #[default]
    Kitty,
    Sixel,
}

/// Base64 bytes per Kitty APC chunk. The spec caps chunks at 4096 and
/// requires every non-final chunk to be a multiple of 4.
pub const KITTY_CHUNK: usize = 4096;

/// Why an image couldn't be rendered.
#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("not a supported image: {0}")]
    Decode(#[from] image::ImageError),
    #[error("image has no pixels")]
    Empty,
    #[error("sixel encoding failed: {0}")]
    Sixel(String),
}

/// Decode `bytes` (any supported format) to RGBA8.
pub fn decode(bytes: &[u8]) -> Result<RgbaImage, ImageError> {
    let img = image::load_from_memory(bytes)?.to_rgba8();
    if img.width() == 0 || img.height() == 0 {
        return Err(ImageError::Empty);
    }
    Ok(img)
}

/// Frame `png` as Kitty graphics APCs: `a=T,f=100,q=2` on the first
/// chunk, `m=1` on every chunk but the last. `q=2` keeps the terminal's
/// reply out of the shell's input.
#[must_use]
pub fn kitty_apcs(png: &[u8]) -> Vec<u8> {
    let b64 = STANDARD.encode(png);
    let chunks: Vec<&[u8]> = b64.as_bytes().chunks(KITTY_CHUNK).collect();
    let mut out = Vec::with_capacity(b64.len() + chunks.len() * 32);
    for (i, chunk) in chunks.iter().enumerate() {
        let more = i + 1 < chunks.len();
        out.extend_from_slice(b"\x1b_G");
        if i == 0 {
            out.extend_from_slice(b"a=T,f=100,q=2");
            if more {
                out.extend_from_slice(b",m=1");
            }
        } else {
            out.extend_from_slice(if more { b"m=1" } else { b"m=0" });
        }
        out.push(b';');
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
    out
}

/// Kitty output for an image file's `bytes`. PNG input is sent as-is;
/// anything else is decoded and re-encoded as PNG.
pub fn encode_kitty(bytes: &[u8]) -> Result<Vec<u8>, ImageError> {
    let img = decode(bytes)?;
    if image::guess_format(bytes).ok() == Some(ImageFormat::Png) {
        return Ok(kitty_apcs(bytes));
    }
    let mut png = Vec::new();
    img.write_to(&mut Cursor::new(&mut png), ImageFormat::Png)?;
    Ok(kitty_apcs(&png))
}

/// Sixel DCS for an image file's `bytes`.
pub fn encode_sixel(bytes: &[u8]) -> Result<Vec<u8>, ImageError> {
    let img = decode(bytes)?;
    let (w, h) = (img.width() as usize, img.height() as usize);
    let sixel = icy_sixel::SixelImage::from_rgba(img.into_raw(), w, h)
        .encode()
        .map_err(|e| ImageError::Sixel(e.to_string()))?;
    Ok(sixel.into_bytes())
}

/// Everything to write to stdout for `bytes` in `protocol`.
pub fn render(bytes: &[u8], protocol: Protocol) -> Result<Vec<u8>, ImageError> {
    let mut out = match protocol {
        Protocol::Kitty => encode_kitty(bytes)?,
        Protocol::Sixel => encode_sixel(bytes)?,
    };
    out.push(b'\n');
    Ok(out)
}

/// Read one input (`-` = stdin), naming it for error messages.
fn read_input(file: &Path) -> anyhow::Result<(String, Vec<u8>)> {
    if file.as_os_str() == "-" {
        let mut buf = Vec::new();
        std::io::stdin().read_to_end(&mut buf)?;
        Ok(("<stdin>".to_string(), buf))
    } else {
        let bytes = std::fs::read(file)?;
        Ok((file.display().to_string(), bytes))
    }
}

/// Render every file in `files` to `out`, one after another with a
/// blank line between images. A file that can't be read or decoded is
/// reported on `err` and skipped; the rest still render.
///
/// # Errors
/// Writing `out` failed, or at least one file failed (the count is in
/// the message).
pub fn run_to<W: Write, E: Write>(
    files: &[PathBuf],
    protocol: Protocol,
    out: &mut W,
    err: &mut E,
) -> anyhow::Result<()> {
    let mut failed = 0usize;
    let mut printed = 0usize;
    for file in files {
        let rendered = read_input(file)
            .map_err(|e| format!("{}: {e}", file.display()))
            .and_then(|(name, bytes)| render(&bytes, protocol).map_err(|e| format!("{name}: {e}")));
        match rendered {
            Ok(bytes) => {
                if printed > 0 {
                    out.write_all(b"\n")?;
                }
                out.write_all(&bytes)?;
                out.flush()?;
                printed += 1;
            }
            Err(msg) => {
                failed += 1;
                let _ = writeln!(err, "toastty image: {msg}");
            }
        }
    }
    if failed > 0 {
        anyhow::bail!("toastty image: {failed} of {} image(s) failed", files.len());
    }
    Ok(())
}

/// Run `toastty image` on stdout / stderr.
///
/// # Errors
/// See [`run_to`].
pub fn run(files: &[PathBuf], protocol: Protocol) -> anyhow::Result<()> {
    run_to(
        files,
        protocol,
        &mut std::io::stdout().lock(),
        &mut std::io::stderr().lock(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, Rgba};

    /// A `w`×`h` image with a distinct colour per pixel.
    fn pattern(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_fn(w, h, |x, y| {
            Rgba([(x * 40) as u8, (y * 40) as u8, ((x + y) * 20) as u8, 255])
        })
    }

    fn encoded(img: &RgbaImage, format: ImageFormat) -> Vec<u8> {
        let mut buf = Vec::new();
        let dynimg = DynamicImage::ImageRgba8(img.clone());
        // JPEG has no alpha channel.
        let dynimg = if format == ImageFormat::Jpeg {
            DynamicImage::ImageRgb8(dynimg.to_rgb8())
        } else {
            dynimg
        };
        dynimg.write_to(&mut Cursor::new(&mut buf), format).unwrap();
        buf
    }

    fn png(img: &RgbaImage) -> Vec<u8> {
        encoded(img, ImageFormat::Png)
    }

    /// Split a stream of Kitty APCs into `(header, payload)` pairs,
    /// asserting the framing is well-formed.
    fn split_apcs(bytes: &[u8]) -> Vec<(String, String)> {
        let s = std::str::from_utf8(bytes).unwrap();
        let mut rest = s;
        let mut out = Vec::new();
        while !rest.is_empty() {
            rest = rest.strip_prefix("\x1b_G").expect("APC start");
            let end = rest.find("\x1b\\").expect("APC terminator");
            let (body, tail) = rest.split_at(end);
            let (header, payload) = body.split_once(';').expect("header;payload");
            out.push((header.to_string(), payload.to_string()));
            rest = &tail[2..];
        }
        out
    }

    /// Reassemble the base64 payload of a Kitty APC stream.
    fn reassemble(bytes: &[u8]) -> Vec<u8> {
        let b64: String = split_apcs(bytes).into_iter().map(|(_, p)| p).collect();
        STANDARD.decode(b64).unwrap()
    }

    #[test]
    fn protocol_defaults_to_kitty() {
        assert_eq!(Protocol::default(), Protocol::Kitty);
    }

    #[test]
    fn decode_png() {
        let img = pattern(3, 2);
        assert_eq!(decode(&png(&img)).unwrap(), img);
    }

    #[test]
    fn decode_other_formats() {
        let img = pattern(4, 4);
        for format in [
            ImageFormat::Bmp,
            ImageFormat::Gif,
            ImageFormat::Jpeg,
            ImageFormat::WebP,
        ] {
            let got = decode(&encoded(&img, format)).unwrap();
            assert_eq!(got.dimensions(), (4, 4), "{format:?}");
        }
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(matches!(
            decode(b"definitely not an image"),
            Err(ImageError::Decode(_))
        ));
    }

    #[test]
    fn decode_rejects_empty_input() {
        assert!(decode(b"").is_err());
    }

    #[test]
    fn decode_rejects_truncated_png() {
        let bytes = png(&pattern(8, 8));
        assert!(decode(&bytes[..bytes.len() / 2]).is_err());
    }

    #[test]
    fn kitty_single_chunk_has_no_m_key() {
        let apcs = split_apcs(&kitty_apcs(b"tiny"));
        assert_eq!(apcs.len(), 1);
        assert_eq!(apcs[0].0, "a=T,f=100,q=2");
        assert_eq!(apcs[0].1, STANDARD.encode(b"tiny"));
    }

    #[test]
    fn kitty_empty_payload_emits_nothing() {
        // `chunks` on an empty slice yields nothing. Unreachable in
        // practice: `decode` rejects empty input first.
        assert!(kitty_apcs(b"").is_empty());
    }

    #[test]
    fn kitty_exactly_one_full_chunk() {
        // 3072 raw bytes → exactly 4096 base64 bytes.
        let data = vec![7u8; KITTY_CHUNK / 4 * 3];
        let apcs = split_apcs(&kitty_apcs(&data));
        assert_eq!(apcs.len(), 1);
        assert_eq!(apcs[0].0, "a=T,f=100,q=2");
        assert_eq!(apcs[0].1.len(), KITTY_CHUNK);
    }

    #[test]
    fn kitty_one_byte_over_a_chunk_splits() {
        let data = vec![7u8; KITTY_CHUNK / 4 * 3 + 1];
        let apcs = split_apcs(&kitty_apcs(&data));
        assert_eq!(apcs.len(), 2);
        assert_eq!(apcs[0].0, "a=T,f=100,q=2,m=1");
        assert_eq!(apcs[1].0, "m=0");
        assert_eq!(reassemble(&kitty_apcs(&data)), data);
    }

    #[test]
    fn kitty_multi_chunk_framing() {
        let data: Vec<u8> = (0..20_000u32).map(|i| (i * 31) as u8).collect();
        let bytes = kitty_apcs(&data);
        let apcs = split_apcs(&bytes);
        assert!(apcs.len() > 2);
        assert_eq!(apcs[0].0, "a=T,f=100,q=2,m=1");
        for (header, payload) in &apcs[1..apcs.len() - 1] {
            assert_eq!(header, "m=1");
            assert_eq!(payload.len(), KITTY_CHUNK);
        }
        let (last_header, last_payload) = apcs.last().unwrap();
        assert_eq!(last_header, "m=0");
        assert!(!last_payload.is_empty() && last_payload.len() <= KITTY_CHUNK);
        // Every non-final chunk is a multiple of 4 (spec requirement).
        for (_, payload) in &apcs[..apcs.len() - 1] {
            assert_eq!(payload.len() % 4, 0);
        }
        assert_eq!(reassemble(&bytes), data);
    }

    #[test]
    fn kitty_headers_parse_with_toastty_parser() {
        use toastty_graphics::kitty::header::{self, Action, Format, Quiet};
        let data = vec![1u8; 10_000];
        let apcs = split_apcs(&kitty_apcs(&data));
        let first = header::parse(format!("G{}", apcs[0].0).as_bytes()).unwrap();
        assert_eq!(first.action, Action::TransmitAndPlace);
        assert_eq!(first.format, Format::Png);
        assert_eq!(first.quiet, Quiet::Silent);
        assert!(first.more);
        for (i, (h, _)) in apcs.iter().enumerate().skip(1) {
            let parsed = header::parse(format!("G{h}").as_bytes()).unwrap();
            assert_eq!(parsed.more, i + 1 < apcs.len());
        }
    }

    #[test]
    fn kitty_png_is_passed_through_unchanged() {
        let bytes = png(&pattern(5, 3));
        assert_eq!(reassemble(&encode_kitty(&bytes).unwrap()), bytes);
    }

    #[test]
    fn kitty_non_png_is_reencoded_as_png() {
        let img = pattern(6, 4);
        for format in [ImageFormat::Bmp, ImageFormat::Gif, ImageFormat::WebP] {
            let payload = reassemble(&encode_kitty(&encoded(&img, format)).unwrap());
            assert_eq!(image::guess_format(&payload).unwrap(), ImageFormat::Png);
            assert_eq!(decode(&payload).unwrap().dimensions(), (6, 4), "{format:?}");
        }
    }

    #[test]
    fn kitty_lossless_reencode_preserves_pixels() {
        let img = pattern(6, 4);
        let payload = reassemble(&encode_kitty(&encoded(&img, ImageFormat::Bmp)).unwrap());
        assert_eq!(decode(&payload).unwrap(), img);
    }

    #[test]
    fn kitty_payload_decodes_with_toastty_decoder() {
        use toastty_graphics::kitty::{decode::decode as kdecode, header::Format};
        let img = pattern(7, 5);
        let payload = reassemble(&encode_kitty(&png(&img)).unwrap());
        let data = kdecode(Format::Png, &payload, 0, 0).unwrap();
        assert_eq!((data.width, data.height), (7, 5));
        assert_eq!(data.pixels, img.into_raw());
    }

    #[test]
    fn kitty_rejects_garbage() {
        assert!(encode_kitty(b"nope").is_err());
    }

    #[test]
    fn sixel_is_a_dcs() {
        let out = encode_sixel(&png(&pattern(4, 6))).unwrap();
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.starts_with("\x1bP"), "{s:?}");
        assert!(s.contains('q'));
        assert!(s.ends_with("\x1b\\"), "{s:?}");
    }

    #[test]
    fn sixel_round_trips_dimensions_and_colours() {
        // Few solid colours so quantisation is exact.
        let img = RgbaImage::from_fn(12, 13, |x, _| {
            if x < 6 {
                Rgba([255, 0, 0, 255])
            } else {
                Rgba([0, 0, 255, 255])
            }
        });
        let out = encode_sixel(&png(&img)).unwrap();
        let back = icy_sixel::SixelImage::decode(&out).unwrap();
        assert_eq!(back.width, 12);
        // Sixel rows are 6 pixels tall; the decoder may round up.
        assert!(back.height >= 13);
        let px = |x: usize, y: usize| {
            let i = (y * back.width + x) * 4;
            &back.pixels[i..i + 3]
        };
        assert_eq!(px(0, 0), [255, 0, 0]);
        assert_eq!(px(11, 12), [0, 0, 255]);
    }

    #[test]
    fn sixel_accepts_non_png_input() {
        let out = encode_sixel(&encoded(&pattern(3, 3), ImageFormat::Jpeg)).unwrap();
        assert!(out.starts_with(b"\x1bP"));
    }

    #[test]
    fn sixel_rejects_garbage() {
        assert!(encode_sixel(b"nope").is_err());
    }

    #[test]
    fn render_appends_newline_for_both_protocols() {
        let bytes = png(&pattern(2, 2));
        for protocol in [Protocol::Kitty, Protocol::Sixel] {
            let out = render(&bytes, protocol).unwrap();
            assert!(out.ends_with(b"\x1b\\\n"), "{protocol:?}");
        }
    }

    #[test]
    fn render_dispatches_on_protocol() {
        let bytes = png(&pattern(2, 2));
        assert!(
            render(&bytes, Protocol::Kitty)
                .unwrap()
                .starts_with(b"\x1b_G")
        );
        assert!(
            render(&bytes, Protocol::Sixel)
                .unwrap()
                .starts_with(b"\x1bP")
        );
    }

    #[test]
    fn render_error_messages() {
        let err = render(b"xx", Protocol::Kitty).unwrap_err();
        assert!(err.to_string().starts_with("not a supported image"));
    }

    fn temp_file(tag: &str, bytes: &[u8]) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("toastty-image-test-{}-{tag}", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn run_capture(files: &[PathBuf], protocol: Protocol) -> (anyhow::Result<()>, Vec<u8>, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let res = run_to(files, protocol, &mut out, &mut err);
        (res, out, String::from_utf8(err).unwrap())
    }

    #[test]
    fn run_single_file_matches_render() {
        let bytes = png(&pattern(3, 3));
        let path = temp_file("single.png", &bytes);
        let (res, out, err) = run_capture(std::slice::from_ref(&path), Protocol::Kitty);
        std::fs::remove_file(&path).unwrap();
        res.unwrap();
        assert!(err.is_empty());
        assert_eq!(out, render(&bytes, Protocol::Kitty).unwrap());
    }

    #[test]
    fn run_multiple_files_separated_by_blank_line() {
        let a = png(&pattern(2, 2));
        let b = png(&pattern(4, 3));
        let pa = temp_file("multi-a.png", &a);
        let pb = temp_file("multi-b.png", &b);
        for protocol in [Protocol::Kitty, Protocol::Sixel] {
            let (res, out, err) = run_capture(&[pa.clone(), pb.clone()], protocol);
            res.unwrap();
            assert!(err.is_empty());
            let mut want = render(&a, protocol).unwrap();
            want.push(b'\n');
            want.extend(render(&b, protocol).unwrap());
            assert_eq!(out, want, "{protocol:?}");
        }
        std::fs::remove_file(&pa).unwrap();
        std::fs::remove_file(&pb).unwrap();
    }

    #[test]
    fn run_same_file_twice_renders_twice() {
        let bytes = png(&pattern(2, 2));
        let path = temp_file("twice.png", &bytes);
        let (res, out, _) = run_capture(&[path.clone(), path.clone()], Protocol::Kitty);
        std::fs::remove_file(&path).unwrap();
        res.unwrap();
        let one = render(&bytes, Protocol::Kitty).unwrap();
        assert_eq!(out.len(), one.len() * 2 + 1);
    }

    #[test]
    fn run_missing_file_is_reported_with_path() {
        let missing = PathBuf::from("/nonexistent/toastty-test.png");
        let (res, out, err) = run_capture(std::slice::from_ref(&missing), Protocol::Kitty);
        assert!(out.is_empty());
        assert!(
            err.starts_with("toastty image: /nonexistent/toastty-test.png: "),
            "{err}"
        );
        assert_eq!(
            res.unwrap_err().to_string(),
            "toastty image: 1 of 1 image(s) failed"
        );
    }

    #[test]
    fn run_non_image_file_is_reported_with_path() {
        let path = temp_file("not-image.txt", b"hello");
        let (res, out, err) = run_capture(std::slice::from_ref(&path), Protocol::Sixel);
        std::fs::remove_file(&path).unwrap();
        assert!(res.is_err());
        assert!(out.is_empty());
        assert!(err.contains(&path.display().to_string()), "{err}");
        assert!(err.contains("not a supported image"), "{err}");
    }

    #[test]
    fn run_bad_file_does_not_stop_the_rest() {
        let good = png(&pattern(2, 2));
        let pg1 = temp_file("mixed-1.png", &good);
        let pbad = temp_file("mixed-bad.txt", b"junk");
        let pg2 = temp_file("mixed-2.png", &good);
        let missing = PathBuf::from("/nonexistent/x.png");
        let (res, out, err) = run_capture(
            &[pg1.clone(), pbad.clone(), missing, pg2.clone()],
            Protocol::Kitty,
        );
        for p in [&pg1, &pbad, &pg2] {
            std::fs::remove_file(p).unwrap();
        }
        // Both good images printed, separated by exactly one blank line
        // (the failed ones leave no gap behind).
        let one = render(&good, Protocol::Kitty).unwrap();
        let mut want = one.clone();
        want.push(b'\n');
        want.extend(&one);
        assert_eq!(out, want);
        assert_eq!(err.lines().count(), 2, "{err}");
        assert_eq!(
            res.unwrap_err().to_string(),
            "toastty image: 2 of 4 image(s) failed"
        );
    }

    #[test]
    fn run_no_files_is_a_no_op() {
        let (res, out, err) = run_capture(&[], Protocol::Kitty);
        res.unwrap();
        assert!(out.is_empty() && err.is_empty());
    }
}
