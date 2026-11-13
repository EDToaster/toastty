//! Draw ops → tiny-skia pixmap → straight-alpha RGBA.

use cosmic_text::{CacheKey, FontSystem, SwashCache, SwashContent};
use tiny_skia::{
    FillRule, FilterQuality, LineCap, LineJoin, Paint, PathBuilder, Pixmap, PixmapPaint, Rect,
    Stroke, Transform,
};

use std::sync::{Arc, Weak};

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
            Src::Diagram(d, _) => Arc::as_ptr(d) as usize,
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

/// Images resampled to their placed size and fixed-height diagram bands.
/// Every paint blits identical pixels regardless of its vertical offset.
/// Images retain their source; diagram entries retain only a weak
/// identity, so cached pixels cannot keep old prepared scenes alive.
#[derive(Default)]
pub(crate) struct Scaled {
    map: std::collections::HashMap<ScaledKey, ScaledEntry>,
    bytes: u64,
    clock: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ScaledKey {
    Image(usize, u32, u32),
    /// Source identity, canonical fit, band top and height. The requested
    /// width is deliberately absent: multiple widths can fit identically.
    Diagram(usize, (u8, u32, u32, u32), u32, u32),
}

struct ScaledEntry {
    source: SourceOwner,
    pixels: Arc<Pixmap>,
    used: u64,
}

enum SourceOwner {
    Image { _image: Arc<Img> },
    // A weak handle reserves the allocation identity without retaining
    // the SVG scene. Expired owners must never produce a cache hit.
    Diagram(Weak<PreparedDiagram>),
}

impl SourceOwner {
    fn new(source: &Src) -> Self {
        match source {
            Src::Img(image) => Self::Image {
                _image: Arc::clone(image),
            },
            Src::Diagram(diagram, _) => Self::Diagram(Arc::downgrade(diagram)),
        }
    }

    fn is_live(&self) -> bool {
        match self {
            Self::Image { .. } => true,
            Self::Diagram(diagram) => diagram.upgrade().is_some(),
        }
    }
}

const MAX_SCALED_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SCALED_ENTRIES: usize = 512;
const DIAGRAM_BAND_HEIGHT: u32 = 256;

impl Scaled {
    fn cached(&mut self, key: &ScaledKey) -> Option<Arc<Pixmap>> {
        let entry = self.map.get_mut(key)?;
        if !entry.source.is_live() {
            self.discard(key);
            return None;
        }
        self.clock = self.clock.saturating_add(1);
        entry.used = self.clock;
        Some(Arc::clone(&entry.pixels))
    }

    fn discard(&mut self, key: &ScaledKey) {
        if let Some(old) = self.map.remove(key) {
            self.bytes -= old.pixels.data().len() as u64;
        }
    }

    fn purge_stale(&mut self) {
        let bytes = &mut self.bytes;
        self.map.retain(|_, entry| {
            if entry.source.is_live() {
                true
            } else {
                *bytes -= entry.pixels.data().len() as u64;
                false
            }
        });
    }

    fn remember(&mut self, key: ScaledKey, src: &Src, pixels: &Arc<Pixmap>) {
        let bytes = pixels.data().len() as u64;
        // An oversized image can still be painted, but must not flush
        // useful entries or make the retained-byte budget a soft limit.
        if bytes > MAX_SCALED_BYTES {
            return;
        }
        self.purge_stale();
        self.discard(&key);
        while self.bytes + bytes > MAX_SCALED_BYTES || self.map.len() >= MAX_SCALED_ENTRIES {
            let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| *key)
            else {
                break;
            };
            self.discard(&oldest);
        }
        self.clock = self.clock.saturating_add(1);
        self.bytes += bytes;
        self.map.insert(
            key,
            ScaledEntry {
                source: SourceOwner::new(src),
                pixels: Arc::clone(pixels),
                used: self.clock,
            },
        );
    }

    fn get_image(&mut self, src: &Src, w: u32, h: u32) -> Option<Arc<Pixmap>> {
        let Src::Img(img) = src else { return None };
        let key = ScaledKey::Image(src.id(), w, h);
        if let Some(pm) = self.cached(&key) {
            return Some(pm);
        }
        let from = &img.pm;
        let pm = if from.width() == w && from.height() == h {
            Arc::new(from.clone())
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
        self.remember(key, src, &pm);
        Some(pm)
    }

    /// Rasterize only normalized bands intersecting the destination. The
    /// diagram is already fitted by layout, so no second resampling pass
    /// (and no full-height intermediate raster) is needed.
    fn draw_diagram(
        &mut self,
        dst: &mut Pixmap,
        src: &Src,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    ) -> Option<()> {
        let Src::Diagram(diagram, max_w) = src else {
            return None;
        };
        let fit = diagram.raster_key(*max_w).ok()?;
        if (fit.2, fit.3) != (w, h) {
            return None;
        }
        if x >= dst.width() as i32 || x + w as i32 <= 0 {
            return Some(());
        }
        let top = (-y).max(0) as u32;
        let bottom = (dst.height() as i32 - y).max(0) as u32;
        let bottom = bottom.min(h);
        if top >= bottom {
            return Some(());
        }
        let first = top / DIAGRAM_BAND_HEIGHT * DIAGRAM_BAND_HEIGHT;
        for band_y in (first..bottom).step_by(DIAGRAM_BAND_HEIGHT as usize) {
            let band_h = DIAGRAM_BAND_HEIGHT.min(h - band_y);
            let key = ScaledKey::Diagram(Arc::as_ptr(diagram) as usize, fit, band_y, band_h);
            let pixels = if let Some(hit) = self.cached(&key) {
                hit
            } else {
                let pixels = diagram.pixmap_band(*max_w, band_y, band_h).ok()?;
                self.remember(key, src, &pixels);
                pixels
            };
            blit(dst, &pixels, x, y + band_y as i32);
        }
        Some(())
    }
}

/// Paint `ops` into `pm`, whose top edge is at block y = `oy` (a whole
/// pixel). Ops entirely outside the band are skipped. Painting is
/// translation-invariant: any band gives the same pixels as the
/// corresponding rows of a full paint. Returns false when a raster
/// cannot be painted, so callers can retain their text fallback.
pub(crate) fn execute(
    pm: &mut Pixmap,
    ops: &[Op],
    oy: u32,
    fs: &mut FontSystem,
    swash: &mut SwashCache,
    scaled: &mut Scaled,
) -> bool {
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
                let (x, y) = (x.round() as i32, y.round() as i32);
                if matches!(src, Src::Diagram(..)) {
                    if scaled.draw_diagram(pm, &src, x, y, iw, ih).is_none() {
                        return false;
                    }
                    continue;
                }
                let Some(img) = scaled.get_image(&src, iw, ih) else {
                    return false;
                };
                blit(pm, &img, x, y);
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
    true
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

    fn sequence() -> Arc<PreparedDiagram> {
        let mut source = String::from("sequenceDiagram\nparticipant A\nparticipant B\n");
        for i in 0..45 {
            use std::fmt::Write as _;
            writeln!(source, "A->>B: Message {i}").unwrap();
        }
        crate::mermaid::prepare_mermaid(&source, 16.0, &crate::Theme::default()).unwrap()
    }

    #[test]
    fn diagram_paints_only_visible_bands_and_reuses_equivalent_fits() {
        let diagram = sequence();
        let (w, h) = diagram.size_for(1200).unwrap();
        assert!(h > 3 * DIAGRAM_BAND_HEIGHT);
        assert_eq!(
            diagram.raster_key(1200).unwrap(),
            diagram.raster_key(1600).unwrap()
        );
        let mut cache = Scaled::default();
        let mut viewport = Pixmap::new(w, 40).unwrap();
        let source = Src::Diagram(Arc::clone(&diagram), 1200);
        cache
            .draw_diagram(&mut viewport, &source, 0, -300, w, h)
            .unwrap();
        assert_eq!(cache.map.len(), 1);
        assert_eq!(cache.bytes, u64::from(w) * DIAGRAM_BAND_HEIGHT as u64 * 4);
        let key = *cache.map.keys().next().unwrap();
        let pixels = Arc::clone(&cache.map[&key].pixels);

        viewport.fill(tiny_skia::Color::TRANSPARENT);
        cache
            .draw_diagram(
                &mut viewport,
                &Src::Diagram(Arc::clone(&diagram), 1600),
                0,
                -300,
                w,
                h,
            )
            .unwrap();
        assert_eq!(cache.map.len(), 1);
        assert!(Arc::ptr_eq(&pixels, &cache.map[&key].pixels));

        let full = diagram.pixmap_band(1200, 0, h).unwrap();
        for (y0, band_h) in [(0, 37), (249, 37), (501, 300), (h - 17, 17)] {
            let mut band = Pixmap::new(w, band_h).unwrap();
            cache
                .draw_diagram(&mut band, &source, 0, -(y0 as i32), w, h)
                .unwrap();
            let start = y0 as usize * w as usize * 4;
            let expected = &full.data()[start..start + band.data().len()];
            assert!(
                band.data()
                    .iter()
                    .zip(expected)
                    .all(|(a, b)| a.abs_diff(*b) <= 4),
                "diagram band differs at y={y0}"
            );
        }
        let before = cache.map.len();
        cache
            .draw_diagram(&mut viewport, &source, 0, 100, w, h)
            .unwrap();
        assert_eq!(cache.map.len(), before, "offscreen diagram was rasterized");
    }

    #[test]
    fn raster_cache_evicts_cold_entries_without_flushing_hot_ones() {
        let source = Src::Diagram(sequence(), 1200);
        let mut cache = Scaled::default();
        let key = |i| ScaledKey::Diagram(source.id(), (0, 1.0_f32.to_bits(), 512, 20_000), i, 256);
        let band_bytes = 512 * 256 * 4;
        let count = MAX_SCALED_BYTES / band_bytes;
        for i in 0..count {
            let pixels = Arc::new(Pixmap::new(512, 256).unwrap());
            cache.remember(key(i as u32), &source, &pixels);
        }
        let hot = cache.cached(&key(0)).unwrap();
        cache.remember(
            key(count as u32),
            &source,
            &Arc::new(Pixmap::new(512, 256).unwrap()),
        );
        assert!(cache.cached(&key(1)).is_none(), "oldest cold band survived");
        assert!(Arc::ptr_eq(&hot, &cache.cached(&key(0)).unwrap()));
        assert_eq!(cache.map.len(), count as usize);
        assert_eq!(cache.bytes, MAX_SCALED_BYTES);
        assert_eq!(
            cache.bytes,
            cache
                .map
                .values()
                .map(|e| e.pixels.data().len() as u64)
                .sum::<u64>()
        );

        let before = cache.bytes;
        cache.remember(
            key(1000),
            &source,
            &Arc::new(Pixmap::new(4096, (MAX_SCALED_BYTES / (4096 * 4) + 1) as u32).unwrap()),
        );
        assert_eq!(cache.bytes, before, "oversized raster displaced the cache");
        assert!(cache.cached(&key(1000)).is_none());
    }

    #[test]
    fn raster_cache_bounds_tiny_entries_without_retaining_diagram_scenes() {
        let diagram = sequence();
        let weak = Arc::downgrade(&diagram);
        let source = Src::Diagram(diagram, 1200);
        let mut cache = Scaled::default();
        for i in 0..=MAX_SCALED_ENTRIES {
            let key = ScaledKey::Diagram(source.id(), (0, 1.0_f32.to_bits(), 1, 1000), i as u32, 1);
            cache.remember(key, &source, &Arc::new(Pixmap::new(1, 1).unwrap()));
        }
        assert_eq!(cache.map.len(), MAX_SCALED_ENTRIES);
        assert_eq!(cache.bytes, MAX_SCALED_ENTRIES as u64 * 4);
        let key = *cache.map.keys().next().unwrap();
        drop(source);
        assert!(
            weak.upgrade().is_none(),
            "band cache retained its prepared diagram"
        );
        assert!(
            cache.cached(&key).is_none(),
            "stale diagram pixels were returned"
        );
        assert_eq!(cache.map.len(), MAX_SCALED_ENTRIES - 1);
        assert_eq!(cache.bytes, (MAX_SCALED_ENTRIES - 1) as u64 * 4);

        // A new admission sweeps the remaining expired owners without
        // disturbing live image entries or keeping their byte charges.
        let image = Img::new(&Raster {
            width: 1,
            height: 1,
            pixels: vec![255; 4],
            baseline: None,
        })
        .unwrap();
        let image_weak = Arc::downgrade(&image);
        let image_source = Src::Img(image);
        let image_key = ScaledKey::Image(image_source.id(), 1, 1);
        cache.remember(
            image_key,
            &image_source,
            &Arc::new(Pixmap::new(1, 1).unwrap()),
        );
        assert_eq!(cache.map.len(), 1);
        assert_eq!(cache.bytes, 4);
        drop(image_source);
        assert!(
            image_weak.upgrade().is_some(),
            "image cache no longer retains its source identity"
        );
        assert!(cache.cached(&image_key).is_some());
        drop(cache);
        assert!(image_weak.upgrade().is_none());
    }

    #[test]
    fn failed_diagram_paint_is_reported_instead_of_succeeding_blank() {
        let mut pm = Pixmap::new(10, 10).unwrap();
        let mut fonts = FontSystem::new_with_locale_and_db(
            "en-US".into(),
            cosmic_text::fontdb::Database::new(),
        );
        let op = Op::Image {
            // A zero-width fit is rejected by the real Mermaid backend.
            src: Src::Diagram(sequence(), 0),
            x: 0.0,
            y: 0.0,
            w: 10.0,
            h: 10.0,
        };
        assert!(!execute(
            &mut pm,
            &[op],
            0,
            &mut fonts,
            &mut SwashCache::new(),
            &mut Scaled::default(),
        ));
    }

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
