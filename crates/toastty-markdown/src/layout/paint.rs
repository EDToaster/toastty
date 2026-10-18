//! Draw ops → tiny-skia pixmap → straight-alpha RGBA.

use cosmic_text::{CacheKey, FontSystem, SwashCache, SwashContent};
use tiny_skia::{
    FillRule, FilterQuality, LineCap, LineJoin, Paint, PathBuilder, Pixmap, PixmapPaint, Rect,
    Stroke, Transform,
};

use std::sync::Arc;

use crate::mermaid::PreparedDiagram;
use crate::{Raster, Rgba};

/// A decoded raster (maths, image) kept premultiplied for painting.
#[derive(Debug)]
pub(crate) struct Img {
    pub width: u32,
    pub height: u32,
    pub baseline: Option<u32>,
    pub pm: Pixmap,
}

impl Img {
    pub(crate) fn new(r: &Raster) -> Option<Arc<Self>> {
        if r.width == 0 || r.height == 0 {
            return None;
        }
        Some(Arc::new(Self {
            width: r.width,
            height: r.height,
            baseline: r.baseline,
            pm: to_pixmap(r)?,
        }))
    }

    pub(crate) fn bytes(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height) * 4
    }
}

/// What an image op draws.
#[derive(Debug, Clone)]
pub(crate) enum Src {
    Img(Arc<Img>),
    /// Rasterized at paint time to the op's width (fitted by the layout).
    Diagram(Arc<PreparedDiagram>, u32),
}

impl Src {
    /// Pointer identity, for cache keys.
    pub(crate) fn id(&self) -> usize {
        match self {
            Src::Img(i) => Arc::as_ptr(i) as usize,
            Src::Diagram(d, w) => (Arc::as_ptr(d) as usize) ^ (*w as usize).rotate_left(48),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum Op {
    Rect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        r: f32,
        color: Rgba,
    },
    Stroke {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        r: f32,
        width: f32,
        color: Rgba,
    },
    Glyph {
        key: CacheKey,
        x: i32,
        y: i32,
        color: Rgba,
    },
    /// Raster `idx`, scaled into the given box.
    Image {
        src: Src,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    },
    Circle {
        cx: f32,
        cy: f32,
        r: f32,
        color: Rgba,
        stroke: Option<f32>,
    },
    Check {
        x: f32,
        y: f32,
        s: f32,
        color: Rgba,
    },
    PictureIcon {
        x: f32,
        y: f32,
        s: f32,
        color: Rgba,
    },
}

fn paint(color: Rgba) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8(color[0], color[1], color[2], color[3]);
    p.anti_alias = true;
    p
}

fn rounded(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    if r < 0.5 {
        return Some(PathBuilder::from_rect(Rect::from_xywh(x, y, w, h)?));
    }
    // Quarter circles approximated by cubic Béziers.
    let k = 0.552_284_8 * r;
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish()
}

/// Blend a coverage mask glyph into premultiplied pixels.
#[allow(clippy::too_many_arguments)]
fn blend_mask(
    px: &mut [u8],
    pw: i32,
    ph: i32,
    gx: i32,
    gy: i32,
    gw: i32,
    gh: i32,
    data: &[u8],
    color: Rgba,
) {
    for row in 0..gh {
        let y = gy + row;
        if y < 0 || y >= ph {
            continue;
        }
        for col in 0..gw {
            let x = gx + col;
            if x < 0 || x >= pw {
                continue;
            }
            let cov = u32::from(data[(row * gw + col) as usize]);
            if cov == 0 {
                continue;
            }
            let a = cov * u32::from(color[3]) / 255;
            let i = ((y * pw + x) * 4) as usize;
            let inv = 255 - a;
            for c in 0..3 {
                let src = u32::from(color[c]) * a / 255;
                px[i + c] = (src + u32::from(px[i + c]) * inv / 255) as u8;
            }
            px[i + 3] = (a + u32::from(px[i + 3]) * inv / 255) as u8;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn blend_color(px: &mut [u8], pw: i32, ph: i32, gx: i32, gy: i32, gw: i32, gh: i32, data: &[u8]) {
    for row in 0..gh {
        let y = gy + row;
        if y < 0 || y >= ph {
            continue;
        }
        for col in 0..gw {
            let x = gx + col;
            if x < 0 || x >= pw {
                continue;
            }
            let s = ((row * gw + col) * 4) as usize;
            let a = u32::from(data[s + 3]);
            if a == 0 {
                continue;
            }
            let i = ((y * pw + x) * 4) as usize;
            let inv = 255 - a;
            for c in 0..3 {
                let src = u32::from(data[s + c]) * a / 255;
                px[i + c] = (src + u32::from(px[i + c]) * inv / 255) as u8;
            }
            px[i + 3] = (a + u32::from(px[i + 3]) * inv / 255) as u8;
        }
    }
}

/// Straight-alpha raster → premultiplied pixmap.
fn to_pixmap(r: &Raster) -> Option<Pixmap> {
    let mut pm = Pixmap::new(r.width, r.height)?;
    if r.pixels.len() < (r.width as usize) * (r.height as usize) * 4 {
        return None;
    }
    for (d, s) in pm
        .data_mut()
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(r.pixels.as_chunks::<4>().0)
    {
        let a = u32::from(s[3]);
        d[0] = (u32::from(s[0]) * a / 255) as u8;
        d[1] = (u32::from(s[1]) * a / 255) as u8;
        d[2] = (u32::from(s[2]) * a / 255) as u8;
        d[3] = s[3];
    }
    Some(pm)
}

/// Vertical extent of an op (for culling), generous for glyphs.
fn extent(op: &Op) -> (f32, f32) {
    match *op {
        Op::Rect { y, h, .. } | Op::Image { y, h, .. } => (y, y + h),
        Op::Stroke { y, h, width, .. } => (y - width, y + h + width),
        Op::Glyph { y, .. } => (y as f32 - 400.0, y as f32 + 200.0),
        Op::Circle { cy, r, stroke, .. } => {
            let e = r + stroke.unwrap_or(0.0);
            (cy - e, cy + e)
        }
        Op::Check { y, s, .. } | Op::PictureIcon { y, s, .. } => (y - 2.0, y + s + 2.0),
    }
}

/// A glyph or image op moved up by `oy` (whole) pixels. Both are drawn
/// at integer positions, so this is exact.
fn shifted(op: &Op, oy: f32) -> Op {
    let mut op = op.clone();
    match &mut op {
        Op::Image { y, .. } => *y -= oy,
        Op::Glyph { y, .. } => *y -= oy as i32,
        _ => {}
    }
    op
}

/// Source-over blit of premultiplied `src` at integer `(x, y)`, clipped.
fn blit(dst: &mut Pixmap, src: &Pixmap, x: i32, y: i32) {
    let (dw, dh) = (dst.width() as i32, dst.height() as i32);
    let (sw, sh) = (src.width() as i32, src.height() as i32);
    let (x0, x1) = (x.max(0), (x + sw).min(dw));
    let (y0, y1) = (y.max(0), (y + sh).min(dh));
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let (sd, dd) = (src.data(), dst.data_mut());
    let n = (x1 - x0) as usize * 4;
    for row in y0..y1 {
        let s0 = (((row - y) * sw + (x0 - x)) * 4) as usize;
        let d0 = ((row * dw + x0) * 4) as usize;
        let (s, d) = (&sd[s0..s0 + n], &mut dd[d0..d0 + n]);
        for (sp, dp) in s.as_chunks::<4>().0.iter().zip(d.as_chunks_mut::<4>().0) {
            let a = u32::from(sp[3]);
            if a == 255 {
                dp.copy_from_slice(sp);
            } else if a != 0 {
                let k = 255 - a;
                for c in 0..4 {
                    // Premultiplied source-over with rounding.
                    let v = u32::from(sp[c]) * 255 + u32::from(dp[c]) * k;
                    dp[c] = ((v + 127) / 255).min(255) as u8;
                }
            }
        }
    }
}

/// Images resampled to their placed size, so every band blits the same
/// pixels (resampling per band would depend on the band's offset) and a
/// tall image is resampled once, not once per tile. The key's `Src` is
/// kept alive with the entry, so its pointer can't be reused.
#[derive(Default)]
pub(crate) struct Scaled {
    map: std::collections::HashMap<(usize, u32, u32), (Src, Arc<Pixmap>)>,
    bytes: u64,
}

const MAX_SCALED_BYTES: u64 = 64 * 1024 * 1024;

impl Scaled {
    fn get(&mut self, src: &Src, w: u32, h: u32) -> Option<Arc<Pixmap>> {
        let key = (src.id(), w, h);
        if let Some((_, pm)) = self.map.get(&key) {
            return Some(Arc::clone(pm));
        }
        let diagram: Option<Arc<Pixmap>> = match src {
            Src::Img(_) => None,
            Src::Diagram(d, max_w) => Some(d.pixmap(*max_w).ok()?),
        };
        let from: &Pixmap = match (src, &diagram) {
            (_, Some(p)) => p,
            (Src::Img(img), None) => &img.pm,
            (Src::Diagram(..), None) => return None,
        };
        let pm = if from.width() == w && from.height() == h {
            match diagram {
                // Already at size and shared: no copy.
                Some(p) => return Some(p),
                None => Arc::new(from.clone()),
            }
        } else {
            let mut out = Pixmap::new(w, h)?;
            let pp = PixmapPaint {
                quality: FilterQuality::Bicubic,
                ..PixmapPaint::default()
            };
            out.draw_pixmap(
                0,
                0,
                from.as_ref(),
                &pp,
                Transform::from_scale(
                    w as f32 / from.width() as f32,
                    h as f32 / from.height() as f32,
                ),
                None,
            );
            Arc::new(out)
        };
        let bytes = u64::from(w) * u64::from(h) * 4;
        if self.bytes + bytes > MAX_SCALED_BYTES {
            self.map.clear();
            self.bytes = 0;
        }
        self.bytes += bytes;
        self.map.insert(key, (src.clone(), Arc::clone(&pm)));
        Some(pm)
    }
}

/// Paint `ops` into `pm`, whose top edge is at block y = `oy` (a whole
/// pixel). Ops entirely outside the band are skipped. Painting is
/// translation-invariant: any band gives the same pixels as the
/// corresponding rows of a full paint.
pub(crate) fn execute(
    pm: &mut Pixmap,
    ops: &[Op],
    oy: u32,
    fs: &mut FontSystem,
    swash: &mut SwashCache,
    scaled: &mut Scaled,
) {
    let (pw, ph) = (pm.width() as i32, pm.height() as i32);
    let (top, bottom) = (oy as f32, oy as f32 + ph as f32);
    let tf = Transform::from_translate(0.0, -top);
    for op in ops {
        let (y0, y1) = extent(op);
        if y1 < top - 1.0 || y0 > bottom + 1.0 {
            continue;
        }
        // Integer-positioned ops (glyphs, images) move by whole pixels;
        // vector ops keep block coordinates and draw through `tf`, so
        // their geometry (stroking, curve flattening) is computed exactly
        // as in a full paint and only translated, exactly, at the end.
        let op = match op {
            Op::Glyph { .. } | Op::Image { .. } if oy != 0 => shifted(op, oy as f32),
            _ => op.clone(),
        };
        match op {
            Op::Rect {
                x,
                y,
                w,
                h,
                r,
                color,
            } => {
                if y - top > ph as f32 || y + h - top < 0.0 {
                    continue;
                }
                if let Some(path) = rounded(x, y, w, h, r) {
                    pm.fill_path(&path, &paint(color), FillRule::Winding, tf, None);
                }
            }
            Op::Stroke {
                x,
                y,
                w,
                h,
                r,
                width,
                color,
            } => {
                if y - top > ph as f32 || y + h - top < 0.0 {
                    continue;
                }
                if let Some(path) = rounded(x, y, w, h, r) {
                    let st = Stroke {
                        width,
                        ..Stroke::default()
                    };
                    pm.stroke_path(&path, &paint(color), &st, tf, None);
                }
            }
            Op::Glyph { key, x, y, color } => {
                if y < -200 || y > ph + 200 {
                    continue;
                }
                let Some(img) = swash.get_image(fs, key) else {
                    continue;
                };
                let (gx, gy) = (x + img.placement.left, y - img.placement.top);
                let (gw, gh) = (img.placement.width as i32, img.placement.height as i32);
                let px = pm.data_mut();
                match img.content {
                    SwashContent::Mask => blend_mask(px, pw, ph, gx, gy, gw, gh, &img.data, color),
                    SwashContent::Color => blend_color(px, pw, ph, gx, gy, gw, gh, &img.data),
                    SwashContent::SubpixelMask => {
                        // Average the subpixel coverage into a mask.
                        let mask: Vec<u8> = img
                            .data
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .map(|c| {
                                ((u32::from(c[0]) + u32::from(c[1]) + u32::from(c[2])) / 3) as u8
                            })
                            .collect();
                        blend_mask(px, pw, ph, gx, gy, gw, gh, &mask, color);
                    }
                }
            }
            Op::Image { src, x, y, w, h } => {
                let (iw, ih) = (w.round() as u32, h.round() as u32);
                if iw == 0 || ih == 0 {
                    continue;
                }
                let Some(img) = scaled.get(&src, iw, ih) else {
                    continue;
                };
                blit(pm, &img, x.round() as i32, y.round() as i32);
            }
            Op::Circle {
                cx,
                cy,
                r,
                color,
                stroke,
            } => {
                let Some(path) = PathBuilder::from_circle(cx, cy, r) else {
                    continue;
                };
                match stroke {
                    Some(width) => {
                        let st = Stroke {
                            width,
                            ..Stroke::default()
                        };
                        pm.stroke_path(&path, &paint(color), &st, tf, None);
                    }
                    None => pm.fill_path(&path, &paint(color), FillRule::Winding, tf, None),
                }
            }
            Op::Check { x, y, s, color } => {
                let mut pb = PathBuilder::new();
                pb.move_to(x + s * 0.22, y + s * 0.52);
                pb.line_to(x + s * 0.42, y + s * 0.72);
                pb.line_to(x + s * 0.78, y + s * 0.3);
                if let Some(path) = pb.finish() {
                    let st = Stroke {
                        width: (s * 0.14).max(1.5),
                        line_cap: LineCap::Round,
                        line_join: LineJoin::Round,
                        ..Stroke::default()
                    };
                    pm.stroke_path(&path, &paint(color), &st, tf, None);
                }
            }
            Op::PictureIcon { x, y, s, color } => {
                // A framed landscape: border, sun, mountain.
                if let Some(frame) = rounded(x + 0.5, y + 0.5, s - 1.0, s * 0.8 - 1.0, s * 0.12) {
                    let st = Stroke {
                        width: (s * 0.08).max(1.0),
                        ..Stroke::default()
                    };
                    pm.stroke_path(&frame, &paint(color), &st, tf, None);
                }
                if let Some(sun) = PathBuilder::from_circle(x + s * 0.68, y + s * 0.28, s * 0.09) {
                    pm.fill_path(&sun, &paint(color), FillRule::Winding, tf, None);
                }
                let mut pb = PathBuilder::new();
                pb.move_to(x + s * 0.12, y + s * 0.68);
                pb.line_to(x + s * 0.38, y + s * 0.36);
                pb.line_to(x + s * 0.58, y + s * 0.58);
                pb.line_to(x + s * 0.7, y + s * 0.46);
                pb.line_to(x + s * 0.88, y + s * 0.68);
                pb.close();
                if let Some(m) = pb.finish() {
                    pm.fill_path(&m, &paint(color), FillRule::Winding, tf, None);
                }
            }
        }
    }
}

/// Premultiplied pixmap → straight-alpha raster.
pub(crate) fn into_raster(pm: Pixmap) -> Raster {
    let (width, height) = (pm.width(), pm.height());
    let mut pixels = pm.take();
    for p in pixels.as_chunks_mut::<4>().0 {
        let a = u32::from(p[3]);
        if a != 0 && a != 255 {
            for c in &mut p[..3] {
                *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    Raster {
        width,
        height,
        pixels,
        baseline: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn premultiplied_round_trip() {
        let r = Raster {
            width: 1,
            height: 1,
            pixels: vec![200, 100, 50, 128],
            baseline: None,
        };
        let pm = to_pixmap(&r).unwrap();
        let back = into_raster(pm);
        for (a, b) in back.pixels.iter().zip(&r.pixels) {
            assert!((i32::from(*a) - i32::from(*b)).abs() <= 2, "{a} vs {b}");
        }
    }

    #[test]
    fn rects_fill_and_clip() {
        let mut pm = Pixmap::new(10, 10).unwrap();
        let mut fs = FontSystem::new_with_locale_and_db(
            "en-US".into(),
            cosmic_text::fontdb::Database::new(),
        );
        let mut sc = SwashCache::new();
        let ops = [Op::Rect {
            x: -5.0,
            y: 2.0,
            w: 8.0,
            h: 100.0,
            r: 0.0,
            color: [255, 0, 0, 255],
        }];
        execute(&mut pm, &ops, 0, &mut fs, &mut sc, &mut Scaled::default());
        let r = into_raster(pm);
        assert_eq!(&r.pixels[(2 * 10) * 4..(2 * 10) * 4 + 4], &[255, 0, 0, 255]);
        assert_eq!(r.pixels[(2 * 10 + 5) * 4 + 3], 0);
    }
}
