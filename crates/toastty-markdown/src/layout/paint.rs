//! Draw ops → tiny-skia pixmap → straight-alpha RGBA.

use cosmic_text::{CacheKey, FontSystem, SwashCache, SwashContent};
use tiny_skia::{
    FillRule, FilterQuality, LineCap, LineJoin, Paint, PathBuilder, Pixmap, PixmapPaint, Rect,
    Stroke, Transform,
};

use crate::{Raster, Rgba};

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
        idx: usize,
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

pub(crate) fn execute(
    pm: &mut Pixmap,
    ops: &[Op],
    rasters: &[Raster],
    fs: &mut FontSystem,
    swash: &mut SwashCache,
) {
    let (pw, ph) = (pm.width() as i32, pm.height() as i32);
    let mut converted: Vec<Option<Option<Pixmap>>> = vec![None; rasters.len()];
    for op in ops {
        match *op {
            Op::Rect {
                x,
                y,
                w,
                h,
                r,
                color,
            } => {
                if y > ph as f32 || y + h < 0.0 {
                    continue;
                }
                if let Some(path) = rounded(x, y, w, h, r) {
                    pm.fill_path(
                        &path,
                        &paint(color),
                        FillRule::Winding,
                        Transform::identity(),
                        None,
                    );
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
                if y > ph as f32 || y + h < 0.0 {
                    continue;
                }
                if let Some(path) = rounded(x, y, w, h, r) {
                    let st = Stroke {
                        width,
                        ..Stroke::default()
                    };
                    pm.stroke_path(&path, &paint(color), &st, Transform::identity(), None);
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
            Op::Image { idx, x, y, w, h } => {
                let Some(r) = rasters.get(idx) else { continue };
                if y > ph as f32 || y + h < 0.0 || r.width == 0 || r.height == 0 {
                    continue;
                }
                let slot = &mut converted[idx];
                if slot.is_none() {
                    *slot = Some(to_pixmap(r));
                }
                let Some(Some(src)) = slot.as_ref() else {
                    continue;
                };
                let (sx, sy) = (w / r.width as f32, h / r.height as f32);
                let exact = (sx - 1.0).abs() < 1e-3 && (sy - 1.0).abs() < 1e-3;
                let pp = PixmapPaint {
                    quality: if exact {
                        FilterQuality::Nearest
                    } else {
                        FilterQuality::Bicubic
                    },
                    ..PixmapPaint::default()
                };
                if exact {
                    pm.draw_pixmap(
                        x.round() as i32,
                        y.round() as i32,
                        src.as_ref(),
                        &pp,
                        Transform::identity(),
                        None,
                    );
                } else {
                    pm.draw_pixmap(
                        0,
                        0,
                        src.as_ref(),
                        &pp,
                        Transform::from_row(sx, 0.0, 0.0, sy, x, y),
                        None,
                    );
                }
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
                        pm.stroke_path(&path, &paint(color), &st, Transform::identity(), None);
                    }
                    None => pm.fill_path(
                        &path,
                        &paint(color),
                        FillRule::Winding,
                        Transform::identity(),
                        None,
                    ),
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
                    pm.stroke_path(&path, &paint(color), &st, Transform::identity(), None);
                }
            }
            Op::PictureIcon { x, y, s, color } => {
                // A framed landscape: border, sun, mountain.
                if let Some(frame) = rounded(x + 0.5, y + 0.5, s - 1.0, s * 0.8 - 1.0, s * 0.12) {
                    let st = Stroke {
                        width: (s * 0.08).max(1.0),
                        ..Stroke::default()
                    };
                    pm.stroke_path(&frame, &paint(color), &st, Transform::identity(), None);
                }
                if let Some(sun) = PathBuilder::from_circle(x + s * 0.68, y + s * 0.28, s * 0.09) {
                    pm.fill_path(
                        &sun,
                        &paint(color),
                        FillRule::Winding,
                        Transform::identity(),
                        None,
                    );
                }
                let mut pb = PathBuilder::new();
                pb.move_to(x + s * 0.12, y + s * 0.68);
                pb.line_to(x + s * 0.38, y + s * 0.36);
                pb.line_to(x + s * 0.58, y + s * 0.58);
                pb.line_to(x + s * 0.7, y + s * 0.46);
                pb.line_to(x + s * 0.88, y + s * 0.68);
                pb.close();
                if let Some(m) = pb.finish() {
                    pm.fill_path(
                        &m,
                        &paint(color),
                        FillRule::Winding,
                        Transform::identity(),
                        None,
                    );
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
        execute(&mut pm, &ops, &[], &mut fs, &mut sc);
        let r = into_raster(pm);
        assert_eq!(&r.pixels[(2 * 10) * 4..(2 * 10) * 4 + 4], &[255, 0, 0, 255]);
        assert_eq!(r.pixels[(2 * 10 + 5) * 4 + 3], 0);
    }
}
