//! LaTeX maths → [`Raster`], using `RaTeX` (a Rust port of `KaTeX`).
//!
//! Pipeline: `ratex-parser` (TeX → AST) → `ratex-layout` (AST → boxes →
//! display list, `KaTeX` metrics) → `ratex-render` (display list → tiny-skia
//! pixmap, glyph outlines from the `KaTeX` fonts embedded by its
//! `embed-fonts` feature, so no font files are needed at runtime). The
//! PNG that `ratex-render` returns is decoded back to straight-alpha RGBA,
//! cropped to the ink box and tagged with its baseline.
//!
//! Before handing the source to `RaTeX` we apply our own limits
//! ([`MAX_SOURCE_BYTES`], [`MAX_NESTING`]) and catch any panic, so a
//! hostile formula can only ever produce an `Err`. Rendering runs on a
//! short-lived scoped thread with an 8 MiB stack, so `RaTeX`'s recursion
//! can't overflow a small caller stack (about 20 µs of overhead). A caught
//! panic is still reported by the process's panic hook.
//!
//! Characters the `KaTeX` fonts lack (CJK, emoji in `\text{…}`) make
//! `RaTeX` look up a system fallback font: the first such lookup scans
//! the system fonts (a few hundred ms, logged to stderr by
//! `ratex-unicode-font`); later ones hit its cache.
//!
//! The `KaTeX` fonts are licensed under the SIL Open Font License 1.1 (see
//! the `ratex-katex-fonts` crate's `fonts/FONT_NOTICE.txt` and `OFL.txt`).

use std::panic::{AssertUnwindSafe, catch_unwind};

use ratex_layout::{LayoutOptions, layout, to_display_list, validate_layout};
use ratex_render::{RenderOptions, render_to_png};
use ratex_types::color::Color;
use ratex_types::display_item::DisplayList;
use ratex_types::math_style::MathStyle;

use crate::{Raster, Rgba};

/// Longest accepted source, in bytes.
pub const MAX_SOURCE_BYTES: usize = 4096;
/// Deepest accepted nesting of `{…}`, `\left…\right` and
/// `\begin…\end` (combined). `RaTeX` applies its own, tighter structural
/// limit on top of this.
pub const MAX_NESTING: usize = 256;
/// Largest raster we return; bigger formulas are rendered at a smaller
/// size so they fit.
pub const MAX_WIDTH: u32 = 4096;
pub const MAX_HEIGHT: u32 = 2048;

/// `KaTeX`'s em relative to the surrounding text (`KaTeX`'s CSS uses
/// `font-size: 1.21em`): Computer Modern has a small x-height, so at
/// the same nominal size it looks smaller than a typical body font.
pub const INLINE_SCALE: f32 = 1.21;
/// Display maths relative to inline maths.
pub const DISPLAY_SCALE: f32 = 1.2;

/// Stack for the render thread (see [`render_math`]).
const RENDER_STACK_BYTES: usize = 8 * 1024 * 1024;

/// Transparent margin left around the ink box.
const CROP_PAD: u32 = 1;
/// Smallest em we are willing to render at after scaling to fit.
const MIN_EM_PX: f32 = 2.0;
#[allow(clippy::cast_precision_loss)] // exact: both caps are small
const MAX_WIDTH_F: f32 = MAX_WIDTH as f32;
#[allow(clippy::cast_precision_loss)]
const MAX_HEIGHT_F: f32 = MAX_HEIGHT as f32;

/// Render a LaTeX maths expression.
///
/// - `display`: display style (`$$…$$`, centered block) vs inline (`$…$`).
/// - `font_px`: the surrounding body font size in pixels. Inline maths is
///   drawn at `font_px * INLINE_SCALE` per em so it matches body text;
///   display maths is `DISPLAY_SCALE` times bigger again.
/// - `color`: glyph colour (straight alpha; its alpha scales the result).
///   `\color{…}` inside the formula still wins for its part.
///
/// The result is straight-alpha RGBA8 on a transparent background,
/// cropped to the ink with a 1 px margin, at most
/// [`MAX_WIDTH`] × [`MAX_HEIGHT`].
///
/// [`Raster::baseline`] is set for both inline and display results: the
/// distance in pixels from the top of the raster to the formula's
/// baseline. The crop always keeps the baseline at or below the top row,
/// but the baseline may lie below the last row (e.g. `^2`, or a lone `-`
/// that sits on the maths axis). Display callers may simply ignore it.
///
/// # Errors
/// Returns a message describing the parse/render failure (or a limit
/// that was hit); callers fall back to showing the source as inline
/// code. Never panics.
pub fn render_math(tex: &str, display: bool, font_px: f32, color: Rgba) -> Result<Raster, String> {
    if !font_px.is_finite() || font_px <= 0.0 || font_px > 1024.0 {
        return Err(format!("invalid font size {font_px}"));
    }
    if tex.len() > MAX_SOURCE_BYTES {
        return Err(format!(
            "maths source too long ({} bytes, limit {MAX_SOURCE_BYTES})",
            tex.len()
        ));
    }
    check_nesting(tex)?;
    // RaTeX recurses over the input; at its own depth limit a debug build
    // needs about 1 MiB of stack (release about 256 KiB). Render on a
    // scoped thread with a known, generous stack so the result doesn't
    // depend on the caller's stack size (a stack overflow aborts the
    // process and cannot be caught).
    let work = || {
        catch_unwind(AssertUnwindSafe(|| {
            render_checked(tex, display, font_px, color)
        }))
    };
    let outcome = std::thread::scope(|scope| {
        match std::thread::Builder::new()
            .name("markdown-math".into())
            .stack_size(RENDER_STACK_BYTES)
            .spawn_scoped(scope, work)
        {
            // `work` catches panics itself; `join` only fails if dropping
            // a panic payload panicked.
            Ok(handle) => handle
                .join()
                .unwrap_or_else(|_| Err(Box::new("maths thread failed"))),
            // Couldn't start a thread (resource limits): render here.
            Err(_) => work(),
        }
    });
    match outcome {
        Ok(result) => result,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_owned());
            Err(format!("maths renderer panicked: {msg}"))
        }
    }
}

/// Reject sources nested deeper than [`MAX_NESTING`] without recursing.
fn check_nesting(tex: &str) -> Result<(), String> {
    let bytes = tex.as_bytes();
    let mut depth = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                let start = i + 1;
                let mut end = start;
                while end < bytes.len() && bytes[end].is_ascii_alphabetic() {
                    end += 1;
                }
                if end == start {
                    // Control symbol (`\{`, `\\`, `\%` …): skip one byte.
                    i = start + 1;
                    continue;
                }
                match &tex[start..end] {
                    "left" | "begin" => depth += 1,
                    "right" | "end" => depth = depth.saturating_sub(1),
                    _ => {}
                }
                i = end;
                if depth > MAX_NESTING {
                    break;
                }
                continue;
            }
            b'{' => depth += 1,
            b'}' => depth = depth.saturating_sub(1),
            b'%' => {
                // Comment: braces in it don't count.
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            _ => {}
        }
        if depth > MAX_NESTING {
            break;
        }
        i += 1;
    }
    if depth > MAX_NESTING {
        return Err(format!("maths nested too deeply (limit {MAX_NESTING})"));
    }
    Ok(())
}

fn render_checked(tex: &str, display: bool, font_px: f32, color: Rgba) -> Result<Raster, String> {
    #[cfg(test)]
    assert!(tex != tests::PANIC_TEX, "injected test panic");
    let ast = ratex_parser::parser::parse(tex).map_err(|e| e.to_string())?;
    let style = if display {
        MathStyle::Display
    } else {
        MathStyle::Text
    };
    // RaTeX quantizes RGB by truncating `c * 255`; aim for the middle of
    // the byte so the exact colour survives. Alpha is applied afterwards
    // so antialiasing stays independent of it.
    let channel = |c: u8| ((f32::from(c) + 0.5) / 255.0).min(1.0);
    let options = LayoutOptions::default()
        .with_style(style)
        .with_color(Color {
            r: channel(color[0]),
            g: channel(color[1]),
            b: channel(color[2]),
            a: 1.0,
        });
    let lbox = layout(&ast, &options);
    validate_layout(&lbox)?;
    let list = to_display_list(&lbox);

    let mut em = font_px * INLINE_SCALE * if display { DISPLAY_SCALE } else { 1.0 };
    em = em.min(fit_em(&list, em));
    // Ink can overhang the layout box (italic corrections, big operators),
    // so the box-based fit may still be too big: shrink and retry.
    for _ in 0..3 {
        if em < MIN_EM_PX {
            return Err("formula too large to render".into());
        }
        let raster = rasterize(&list, em, color[3])?;
        if raster.width <= MAX_WIDTH && raster.height <= MAX_HEIGHT {
            return Ok(raster);
        }
        let scale = (MAX_WIDTH_F / px_f32(raster.width)).min(MAX_HEIGHT_F / px_f32(raster.height));
        em *= scale * 0.98;
    }
    Err("formula too large to render".into())
}

/// A pixel dimension as `f32`. Exact for anything a pixmap can hold.
#[allow(clippy::cast_precision_loss)]
fn px_f32(v: u32) -> f32 {
    v as f32
}

/// Padding rendered around the layout box so overhanging ink survives
/// until we crop.
fn render_pad(em: f32) -> f32 {
    (em * 0.25).ceil().max(2.0)
}

/// Largest em at which the layout box (plus margins) fits the size cap.
fn fit_em(list: &DisplayList, em: f32) -> f32 {
    let w = list.width.max(0.0) as f32;
    let h = (list.height + list.depth).max(0.0) as f32;
    let pad = 2.0 * render_pad(em) + 2.0;
    let mut fit = em;
    if w * em + pad > MAX_WIDTH_F {
        fit = fit.min((MAX_WIDTH_F - pad) / w);
    }
    if h * em + pad > MAX_HEIGHT_F {
        fit = fit.min((MAX_HEIGHT_F - pad) / h);
    }
    fit
}

fn rasterize(list: &DisplayList, em: f32, alpha: u8) -> Result<Raster, String> {
    let pad = render_pad(em);
    let png = render_to_png(
        list,
        &RenderOptions {
            font_size: em,
            padding: pad,
            background_color: Color {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            },
            font_dir: String::new(),
            device_pixel_ratio: 1.0,
        },
    )?;
    let (width, height, mut pixels) = decode_png(&png)?;
    if alpha != 255 {
        for px in pixels.as_chunks_mut::<4>().0 {
            px[3] = ((u16::from(px[3]) * u16::from(alpha) + 127) / 255) as u8;
        }
    }
    let baseline = (pad + list.height as f32 * em).round().max(0.0) as u32;
    crop(width, height, &pixels, baseline)
}

fn decode_png(png: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let mut decoder = png::Decoder::new(png);
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return Err(format!(
            "unexpected PNG format {:?}/{:?}",
            info.color_type, info.bit_depth
        ));
    }
    buf.truncate(info.buffer_size());
    Ok((info.width, info.height, buf))
}

/// Crop to the ink box plus [`CROP_PAD`], keeping the baseline at or
/// below the top edge.
fn crop(width: u32, height: u32, pixels: &[u8], baseline: u32) -> Result<Raster, String> {
    let (w, h) = (width as usize, height as usize);
    let mut ink: Option<(usize, usize, usize, usize)> = None;
    for y in 0..h {
        let row = &pixels[y * w * 4..(y + 1) * w * 4];
        for (x, px) in row.as_chunks::<4>().0.iter().enumerate() {
            if px[3] != 0 {
                let b = ink.get_or_insert((x, y, x, y));
                b.0 = b.0.min(x);
                b.1 = b.1.min(y);
                b.2 = b.2.max(x);
                b.3 = b.3.max(y);
            }
        }
    }
    let (x0, y0, x1, y1) = ink.ok_or("formula has no visible ink")?;
    let pad = CROP_PAD as usize;
    let left = x0.saturating_sub(pad);
    let top = y0.saturating_sub(pad).min(baseline as usize);
    let right = (x1 + pad + 1).min(w);
    let bottom = (y1 + pad + 1).min(h);
    let (cw, ch) = (right - left, bottom - top);
    let mut out = Vec::with_capacity(cw * ch * 4);
    for y in top..bottom {
        out.extend_from_slice(&pixels[(y * w + left) * 4..(y * w + right) * 4]);
    }
    Ok(Raster {
        width: cw as u32,
        height: ch as u32,
        pixels: out,
        baseline: Some(baseline - top as u32),
    })
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss, clippy::many_single_char_names)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use super::*;

    /// Source that makes `render_checked` panic, to test containment.
    pub(super) const PANIC_TEX: &str = r"\toasttytestpanic";
    const FG: Rgba = [0xd8, 0xd8, 0xd8, 0xff];
    const PX: f32 = 16.0;

    fn ok(tex: &str, display: bool) -> Raster {
        let r = render_math(tex, display, PX, FG).unwrap_or_else(|e| panic!("{tex:?}: {e}"));
        assert_eq!(r.pixels.len(), (r.width * r.height * 4) as usize, "{tex:?}");
        dump(tex, display, &r);
        r
    }

    fn inline(tex: &str) -> Raster {
        ok(tex, false)
    }

    fn display(tex: &str) -> Raster {
        ok(tex, true)
    }

    fn alpha(r: &Raster, x: u32, y: u32) -> u8 {
        r.pixels[((y * r.width + x) * 4 + 3) as usize]
    }

    /// Ink bounding box `(x0, y0, x1, y1)`, inclusive.
    fn ink(r: &Raster) -> (u32, u32, u32, u32) {
        let mut b = (u32::MAX, u32::MAX, 0, 0);
        for y in 0..r.height {
            for x in 0..r.width {
                if alpha(r, x, y) != 0 {
                    b = (b.0.min(x), b.1.min(y), b.2.max(x), b.3.max(y));
                }
            }
        }
        assert!(b.0 != u32::MAX, "no ink");
        b
    }

    fn ink_rows(r: &Raster) -> u32 {
        let (_, y0, _, y1) = ink(r);
        y1 - y0 + 1
    }

    fn baseline(r: &Raster) -> u32 {
        r.baseline.expect("baseline set")
    }

    fn encode_png(r: &Raster) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, r.width, r.height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().unwrap();
        w.write_image_data(&r.pixels).unwrap();
        w.finish().unwrap();
        out
    }

    /// With `MD_MATH_DUMP=1`, write each render (and a copy composited
    /// on a dark background with the baseline marked) to `/tmp/md-math`.
    fn dump(tex: &str, display: bool, r: &Raster) {
        if std::env::var_os("MD_MATH_DUMP").is_none() {
            return;
        }
        let dir = Path::new("/tmp/md-math");
        std::fs::create_dir_all(dir).unwrap();
        let name: String = tex
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .take(40)
            .collect();
        let name = format!("{}_{name}", if display { "d" } else { "i" });
        std::fs::write(dir.join(format!("{name}.png")), encode_png(r)).unwrap();
        let mut bg = r.clone();
        for (i, px) in bg.pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let a = u32::from(px[3]);
            let y = i as u32 / r.width;
            let base = if Some(y) == r.baseline {
                [0x60, 0x20, 0x20]
            } else {
                [0x14, 0x14, 0x18]
            };
            for c in 0..3 {
                px[c] = ((u32::from(px[c]) * a + base[c] * (255 - a)) / 255) as u8;
            }
            px[3] = 255;
        }
        std::fs::write(dir.join(format!("{name}_bg.png")), encode_png(&bg)).unwrap();
    }

    // ---- basic constructs ----

    #[test]
    fn plain_x_sits_on_baseline() {
        let r = inline("x");
        let (_, y0, _, y1) = ink(&r);
        // `x` has no descender: the ink ends right at the baseline.
        assert!(
            baseline(&r).abs_diff(y1 + 1) <= 1,
            "baseline {:?} ink {y0}..{y1}",
            r.baseline
        );
        // x-height of KaTeX Main is 0.43 em; em = 16 * 1.21.
        let xh = 0.431 * PX * INLINE_SCALE;
        assert!(
            (ink_rows(&r) as f32 - xh).abs() <= 2.0,
            "x ink height {}",
            ink_rows(&r)
        );
        // Tight crop: exactly one transparent margin pixel on every side.
        assert_eq!((ink(&r).0, y0), (CROP_PAD, CROP_PAD));
        assert_eq!(
            (r.width - 1 - ink(&r).2, r.height - 1 - y1),
            (CROP_PAD, CROP_PAD)
        );
    }

    #[test]
    fn inline_text_matches_body_size() {
        // A capital plus a descender spans roughly a body line: about
        // 0.68 + 0.19 em of KaTeX at 1.21 × body size.
        let r = inline("Hy");
        let rows = ink_rows(&r) as f32;
        assert!(rows > 0.8 * PX && rows < 1.25 * PX, "Hy ink rows {rows}");
        let (_, y0, _, y1) = ink(&r);
        let b = baseline(&r);
        assert!(b > y0 && b < y1, "descender goes below the baseline");
    }

    #[test]
    fn fraction_is_taller_with_baseline_in_the_middle() {
        let x = inline("x");
        let f = inline(r"\frac{a+b}{c}");
        assert!(f.height > 2 * x.height, "{} vs {}", f.height, x.height);
        let b = baseline(&f) as f32 / f.height as f32;
        assert!((0.35..0.8).contains(&b), "baseline at {b} of height");
        // The fraction bar is a solid horizontal run near the maths axis
        // (0.25 em above the baseline).
        let bar = (0..f.height)
            .filter(|&y| {
                (0..f.width).filter(|&x| alpha(&f, x, y) > 100).count() as u32 > f.width * 3 / 5
            })
            .collect::<Vec<_>>();
        assert!(!bar.is_empty(), "no fraction bar");
        let axis = baseline(&f) as f32 - 0.25 * PX * INLINE_SCALE;
        assert!(
            (bar[0] as f32 - axis).abs() <= 2.5,
            "bar {bar:?} axis {axis}"
        );
    }

    #[test]
    fn display_fraction_is_bigger_than_inline() {
        let i = inline(r"\frac{1}{2}");
        let d = display(r"\frac{1}{2}");
        assert!(
            d.height as f32 > 1.3 * i.height as f32,
            "{} vs {}",
            d.height,
            i.height
        );
    }

    #[test]
    fn sqrt_has_radical_over_content() {
        let x = inline("x");
        let s = inline(r"\sqrt{x}");
        assert!(s.width > x.width + 4 && s.height > x.height + 2);
        let n = inline(r"\sqrt[3]{x^2+1}");
        assert!(n.width > s.width);
    }

    #[test]
    fn display_sum_puts_limits_above_and_below() {
        let tex = r"\sum_{i=1}^{n} i^2";
        let i = inline(tex);
        let d = display(tex);
        // Inline limits go beside the sigma, display limits stack.
        assert!(
            d.height as f32 > 1.6 * i.height as f32,
            "{} vs {}",
            d.height,
            i.height
        );
        assert!(d.width < 2 * i.width);
        let lim = display(r"\lim_{x \to 0} \frac{\sin x}{x}");
        assert!(lim.height > i.height);
    }

    #[test]
    fn integrals_with_limits() {
        let d = display(r"\int_0^\infty e^{-x^2}\,dx = \frac{\sqrt{\pi}}{2}");
        let i = inline(r"\int_0^1 f(x)\,dx");
        assert!(d.height > i.height);
        let b = baseline(&d);
        assert!(b > d.height / 4 && b < d.height * 3 / 4);
        display(
            r"\oint_C \mathbf{F}\cdot d\mathbf{r} = \iint_S (\nabla\times\mathbf{F})\cdot d\mathbf{S}",
        );
    }

    #[test]
    fn matrices() {
        let m2 = display(r"\begin{pmatrix} a & b \\ c & d \end{pmatrix}");
        let m3 = display(r"\begin{bmatrix} 1 & 0 & 0 \\ 0 & 1 & 0 \\ 0 & 0 & 1 \end{bmatrix}");
        assert!(m3.height > m2.height && m3.width > m2.width);
        display(r"f(x) = \begin{cases} x & x \ge 0 \\ -x & \text{otherwise} \end{cases}");
        display(r"\begin{aligned} a &= b + c \\ d &= e \end{aligned}");
        inline(r"\begin{smallmatrix} a & b \\ c & d \end{smallmatrix}");
    }

    #[test]
    fn text_greek_and_scripts() {
        let t = inline(r"\text{if } x > 0");
        assert!(t.width > inline("x>0").width);
        let g = inline(r"\alpha + \beta = \Gamma(\omega)");
        assert!(g.width > 40);
        let x = inline("x");
        let sup = inline("x^2");
        let sub = inline("x_i");
        let both = inline("x_i^2");
        assert!(sup.width > x.width && sub.width > x.width);
        // Superscript raises the top, subscript lowers the bottom.
        assert!(baseline(&sup) > baseline(&x));
        assert!(sub.height - baseline(&sub) > x.height - baseline(&x));
        assert!(both.height >= sup.height.max(sub.height));
        inline(r"e^{i\pi} + 1 = 0");
        inline(r"a_{n+1} = a_n^{2} - \mathbb{R}");
    }

    #[test]
    fn left_right_delimiters_grow() {
        let small = inline(r"(a)");
        let big = display(r"\left( \frac{a}{b} \right)");
        assert!(big.height as f32 > 1.8 * small.height as f32);
        let (_, y0, _, y1) = ink(&big);
        // The parentheses span the whole height of the fraction.
        let col = (0..big.width)
            .find(|&x| (0..big.height).any(|y| alpha(&big, x, y) > 0))
            .unwrap();
        let rows: Vec<u32> = (0..big.height)
            .filter(|&y| (col..col + 4).any(|x| alpha(&big, x, y) > 0))
            .collect();
        assert!(
            rows.len() as u32 >= (y1 - y0) * 3 / 4,
            "{} of {}",
            rows.len(),
            y1 - y0
        );
        display(
            r"\left[ x \right] \left\{ y \right\} \left| z \right| \left\langle w \right\rangle",
        );
        display(r"\left. \frac{df}{dx} \right|_{x=0}");
    }

    #[test]
    fn colour_is_applied_with_straight_alpha() {
        let colour = [0x30, 0xa0, 0xe0, 0xff];
        let r = render_math(r"\sum_{k} \frac{x^k}{k!}", true, PX, colour).unwrap();
        let mut solid = 0;
        let mut partial = 0;
        for px in r.pixels.as_chunks::<4>().0 {
            match px[3] {
                0 => {}
                255 => {
                    solid += 1;
                    assert_eq!(&px[..3], &colour[..3]);
                }
                a => {
                    partial += 1;
                    // Straight alpha: edge pixels keep the colour (up to
                    // demultiply rounding), they are not darkened.
                    if a >= 32 {
                        for c in 0..3 {
                            assert!(px[c].abs_diff(colour[c]) <= 8, "{px:?}");
                        }
                    }
                }
            }
        }
        assert!(solid > 20, "solid {solid}");
        assert!(partial > 20, "no antialiasing ({partial} partial pixels)");
    }

    #[test]
    fn colour_alpha_scales_output() {
        let r = render_math("x+y", false, PX, [255, 0, 0, 128]).unwrap();
        let max = r
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| p[3])
            .max()
            .unwrap();
        assert!((126..=129).contains(&max), "max alpha {max}");
        assert!(
            r.pixels
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|p| p[3] > 0)
                .all(|p| p[0] > 200 && p[1] < 40)
        );
    }

    #[test]
    fn tex_colour_commands_override() {
        let r = render_math(r"x + \color{red}{y}", false, PX, [255, 255, 255, 255]).unwrap();
        let px = r.pixels.as_chunks::<4>().0;
        assert!(
            px.iter()
                .any(|p| p[3] == 255 && p[0] > 200 && p[1] < 50 && p[2] < 50),
            "no red"
        );
        assert!(
            px.iter()
                .any(|p| p[3] == 255 && p[0] > 200 && p[1] > 200 && p[2] > 200),
            "no white"
        );
        let r = render_math(r"\textcolor{#00ff00}{z}", false, PX, FG).unwrap();
        assert!(
            r.pixels
                .as_chunks::<4>()
                .0
                .iter()
                .any(|p| p[3] == 255 && p[1] > 200 && p[0] < 50)
        );
    }

    #[test]
    fn display_is_bigger_than_inline() {
        let i = inline("x + y = z");
        let d = display("x + y = z");
        let ratio = (d.width - 2) as f32 / (i.width - 2) as f32;
        assert!((ratio - DISPLAY_SCALE).abs() < 0.08, "ratio {ratio}");
    }

    #[test]
    fn scales_with_font_size() {
        let a = render_math(r"a^2+b^2=c^2", false, 16.0, FG).unwrap();
        let b = render_math(r"a^2+b^2=c^2", false, 32.0, FG).unwrap();
        let ratio = (b.width - 2) as f32 / (a.width - 2) as f32;
        assert!((ratio - 2.0).abs() < 0.1, "ratio {ratio}");
        let tiny = render_math("x", false, 4.0, FG).unwrap();
        assert!(tiny.width >= 3 && tiny.height >= 3);
    }

    #[test]
    fn baseline_stays_inside_or_below() {
        // Ink entirely above the baseline: the baseline lies below the
        // raster (never above its top).
        let sup = inline("{}^{2}");
        assert!(baseline(&sup) >= ink(&sup).3);
        // Ink entirely below the baseline: the crop extends up to it.
        let sub = inline(r"\raisebox{-2em}{x}");
        assert_eq!(baseline(&sub), 0);
        assert!(ink(&sub).1 > PX as u32);
        let minus = inline("-");
        assert!(baseline(&minus) > ink(&minus).3);
        assert!(display(r"\frac{a}{b}").baseline.is_some());
    }

    // ---- errors and limits ----

    #[test]
    fn invalid_input_is_an_error() {
        for tex in [
            r"\frac{a}",
            r"\frac{",
            "}",
            "x^^2",
            r"\notacommand{x}",
            r"\left( x",
            r"x \right)",
            r"\begin{pmatrix} a",
            r"\begin{nosuchenv} a \end{nosuchenv}",
            "a_b_c",
            "",
            "   ",
        ] {
            assert!(
                render_math(tex, false, PX, FG).is_err(),
                "{tex:?} should fail"
            );
        }
    }

    #[test]
    fn panics_are_caught() {
        let err = render_math(PANIC_TEX, false, PX, FG).unwrap_err();
        assert!(
            err.contains("panicked") && err.contains("injected"),
            "{err}"
        );
        // And rendering keeps working afterwards.
        inline("x");
    }

    #[test]
    fn invalid_font_size_is_an_error() {
        for px in [0.0, -1.0, f32::NAN, f32::INFINITY, 5000.0] {
            assert!(render_math("x", false, px, FG).is_err(), "{px}");
        }
    }

    #[test]
    fn oversize_source_is_an_error() {
        let tex = "x".repeat(MAX_SOURCE_BYTES + 1);
        let err = render_math(&tex, false, PX, FG).unwrap_err();
        assert!(err.contains("too long"), "{err}");
        // At the limit it is handed to the renderer (and either renders,
        // scaled to fit, or fails cleanly).
        let tex = "x+".repeat(MAX_SOURCE_BYTES / 2 - 1) + "x";
        assert_eq!(tex.len(), MAX_SOURCE_BYTES - 1);
        if let Ok(r) = render_math(&tex, false, PX, FG) {
            assert!(r.width <= MAX_WIDTH && r.height <= MAX_HEIGHT);
        }
    }

    fn assert_fast_err(tex: &str) -> String {
        let start = Instant::now();
        let err = render_math(tex, false, PX, FG).unwrap_err();
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "{:?} slow",
            start.elapsed()
        );
        err
    }

    #[test]
    fn deep_nesting_is_rejected_fast() {
        let n = 2000;
        let braces = format!("{}x{}", "{".repeat(n), "}".repeat(n));
        assert!(assert_fast_err(&braces).contains("nested"));
        let left = format!("{}x{}", r"\left(".repeat(300), r"\right)".repeat(300));
        assert!(assert_fast_err(&left).contains("nested"));
        let env = r"\begin{matrix}".repeat(260);
        assert!(assert_fast_err(&env).contains("nested"));
        // Unbalanced: only opening braces, up to the source limit.
        assert!(assert_fast_err(&"{".repeat(MAX_SOURCE_BYTES)).contains("nested"));
        // Nesting without braces is bounded by RaTeX's own limit.
        assert_fast_err(&(r"\sqrt".repeat(1000) + "x"));
        assert_fast_err(&(r"\frac1".repeat(600) + "x"));
        assert_fast_err(&"x^".repeat(1000));
    }

    #[test]
    fn nesting_count_rules() {
        let at = |n: usize| format!("{}x{}", "{".repeat(n), "}".repeat(n));
        assert!(check_nesting(&at(MAX_NESTING)).is_ok());
        assert!(check_nesting(&at(MAX_NESTING + 1)).is_err());
        // Escaped braces and comments don't nest; siblings don't add up.
        assert!(check_nesting(&r"\{".repeat(1000)).is_ok());
        assert!(check_nesting(&format!("%{}\nx", "{".repeat(1000))).is_ok());
        assert!(check_nesting(&"{x}".repeat(1000)).is_ok());
        assert!(check_nesting(&r"\left(x\right)".repeat(300)).is_ok());
        // `\leftarrow` is not `\left`.
        assert!(check_nesting(&r"\leftarrow".repeat(300)).is_ok());
    }

    #[test]
    fn moderate_nesting_renders_on_a_small_stack() {
        // RaTeX's structural limit is lower than ours; anything it
        // accepts must render without overflowing a small thread stack.
        let tex = format!("{}x{}", r"\sqrt{".repeat(24), "}".repeat(24));
        let frac = format!("{}x{}", r"\frac{1}{".repeat(24), "}".repeat(24));
        let h = std::thread::Builder::new()
            .stack_size(512 * 1024)
            .spawn(move || {
                let a = render_math(&tex, true, PX, FG).map(|r| r.width);
                let b = render_math(&frac, true, PX, FG).map(|r| r.width);
                (a, b)
            })
            .unwrap();
        let (a, b) = h.join().expect("no stack overflow");
        assert!(a.is_ok() && b.is_ok(), "{a:?} {b:?}");
        // At RaTeX's limit (32) too, on a 256 KiB caller stack.
        let deepest = format!("{}x{}", r"\frac{1}{".repeat(32), "}".repeat(32));
        let h = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || render_math(&deepest, true, PX, FG).map(|r| r.height))
            .unwrap();
        assert!(h.join().expect("no stack overflow").is_ok());
    }

    #[test]
    fn macro_bombs_terminate() {
        let bomb = r"\def\a{xxxxxxxxxx}\def\b{\a\a\a\a\a\a\a\a\a\a}\def\c{\b\b\b\b\b\b\b\b\b\b}\def\d{\c\c\c\c\c\c\c\c\c\c}\def\e{\d\d\d\d\d\d\d\d\d\d}\e";
        let start = Instant::now();
        let _ = render_math(bomb, false, PX, FG);
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
        let start = Instant::now();
        assert!(render_math(r"\def\x{\x}\x", false, PX, FG).is_err());
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn huge_output_is_scaled_to_the_cap() {
        let wide = "x".repeat(800);
        let r = render_math(&wide, false, 64.0, FG).unwrap();
        assert!(
            r.width <= MAX_WIDTH && r.width > MAX_WIDTH / 2,
            "{}",
            r.width
        );
        let rows: Vec<String> = (0..120).map(|i| i.to_string()).collect();
        let tall = format!(r"\begin{{matrix}}{}\end{{matrix}}", rows.join(r"\\"));
        let r = render_math(&tall, true, 64.0, FG).unwrap();
        assert!(
            r.height <= MAX_HEIGHT && r.height > MAX_HEIGHT / 2,
            "{}",
            r.height
        );
        let r = render_math(r"\rule{50em}{50em}", true, 200.0, FG);
        if let Ok(r) = r {
            assert!(r.width <= MAX_WIDTH && r.height <= MAX_HEIGHT);
        }
        // Too large to shrink to a legible size.
        assert!(render_math(r"\rule{100000em}{1em}", true, PX, FG).is_err());
    }

    #[test]
    fn odd_inputs_never_panic() {
        for tex in [
            r"\\",
            r"\\\\\\",
            "&",
            "#",
            "~",
            "^",
            "_",
            r"\left",
            r"\right",
            r"\frac",
            r"\sqrt[",
            r"\begin{",
            r"\end{matrix}",
            r"\hspace{-1000em}x",
            r"\kern-50em x",
            r"\raisebox{100em}{x}",
            r"\color{nope}{x}",
            r"\text{\(\)}",
            "\u{0}",
            "x\u{301}",
            r"\mathrm{ÿ}",
            r"\operatorname{}",
            r"\overbrace{\underbrace{x}_{a}}^{b}",
            r"\xrightarrow[under]{over}",
            r"\boxed{x}\cancel{y}",
        ] {
            let _ = render_math(tex, false, PX, FG);
            let _ = render_math(tex, true, PX, FG);
        }
    }

    // ---- golden snapshots ----

    fn golden_path(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/golden")
            .join(format!("math_{name}.png"))
    }

    fn decode(path: &Path) -> Raster {
        let bytes = std::fs::read(path).unwrap_or_else(|e| {
            panic!(
                "{}: {e} (run with MD_UPDATE_GOLDEN=1 to create)",
                path.display()
            )
        });
        let (width, height, pixels) = decode_png(&bytes).unwrap();
        Raster {
            width,
            height,
            pixels,
            baseline: None,
        }
    }

    /// Compare against a stored PNG. Sizes may differ by a pixel (edge
    /// antialiasing can move the ink box); at most 1% of the overlapping
    /// pixels may differ by more than 24 in any channel.
    fn golden(name: &str, tex: &str, display: bool) {
        let r = render_math(tex, display, 20.0, [0xea, 0xc0, 0x60, 0xff]).unwrap();
        dump(tex, display, &r);
        let path = golden_path(name);
        if std::env::var_os("MD_UPDATE_GOLDEN").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, encode_png(&r)).unwrap();
            return;
        }
        let g = decode(&path);
        assert!(
            r.width.abs_diff(g.width) <= 1 && r.height.abs_diff(g.height) <= 1,
            "{name}: size {}x{} vs golden {}x{}",
            r.width,
            r.height,
            g.width,
            g.height
        );
        let (w, h) = (r.width.min(g.width), r.height.min(g.height));
        let mut bad = 0;
        for y in 0..h {
            for x in 0..w {
                let a = &r.pixels[((y * r.width + x) * 4) as usize..][..4];
                let b = &g.pixels[((y * g.width + x) * 4) as usize..][..4];
                // Compare premultiplied so invisible colour noise is ignored.
                let pm = |p: &[u8], c: usize| u32::from(p[c]) * u32::from(p[3]) / 255;
                let diff = (0..3)
                    .map(|c| pm(a, c).abs_diff(pm(b, c)))
                    .max()
                    .unwrap()
                    .max(u32::from(a[3].abs_diff(b[3])));
                if diff > 24 {
                    bad += 1;
                }
            }
        }
        assert!(
            bad * 100 <= w * h,
            "{name}: {bad} of {} pixels differ",
            w * h
        );
    }

    #[test]
    fn golden_inline_quadratic() {
        golden(
            "inline_quadratic",
            r"x = \frac{-b \pm \sqrt{b^2 - 4ac}}{2a}",
            false,
        );
    }

    #[test]
    fn golden_display_series() {
        golden(
            "display_series",
            r"\sum_{n=1}^{\infty} \frac{1}{n^2} = \int_0^1 \frac{\ln x}{x - 1}\,dx = \frac{\pi^2}{6}",
            true,
        );
    }

    #[test]
    fn golden_display_matrix() {
        golden(
            "display_matrix",
            r"A = \left( \begin{matrix} \alpha & \beta \\ \gamma_1 & \delta^2 \end{matrix} \right), \quad \text{if } \det A \neq 0",
            true,
        );
    }

    #[test]
    fn golden_compare_detects_changes() {
        let path = golden_path("inline_quadratic");
        if !path.exists() {
            return;
        }
        let g = decode(&path);
        let other = render_math(
            r"x = \frac{-b \mp \sqrt{b^2 + 4ac}}{2a}",
            false,
            20.0,
            [0xea, 0xc0, 0x60, 0xff],
        )
        .unwrap();
        assert!(
            other != g,
            "a different formula must not match the golden image"
        );
    }
}
