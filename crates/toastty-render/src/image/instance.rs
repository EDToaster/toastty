//! GPU vertex layout for an image-instance draw, plus the CPU-side
//! [`build_image_instances`] that turns `ImageGrid` + `ImageRegistry`
//! + [`super::atlas::ImageTextureCache`] into per-image instance vecs.
//!
//! We emit one instance per `(image, placement)`. The texture is
//! selected per draw call (rebind between draws); see
//! [`super::pipeline::ImagePipeline::render`].

use bytemuck::{Pod, Zeroable};
use toastty_term::{ImageGrid, ImageRegistry};

use super::atlas::ImageTextureCache;

/// One instance pushed to the image pipeline. Mirrors `ImageInstance`
/// in `shaders/image.wgsl`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct ImageInstance {
    /// Top-left in screen pixels.
    pub pos: [f32; 2],
    /// Width / height in screen pixels.
    pub size: [f32; 2],
    /// UV min/max in normalized (0..1) image-space.
    pub uv_min: [f32; 2],
    pub uv_max: [f32; 2],
    /// Which texture slot this instance binds to. Drives the host's
    /// per-draw rebind loop.
    pub texture_index: u32,
    /// Z order. The host splits below/above text using sign.
    pub z: i32,
    /// Source image id. Not consumed by the shader; carried here so the
    /// CPU-side sort can tie-break equal-z placements by ascending id
    /// (kitty spec). Occupies the first std430 pad slot.
    #[allow(clippy::pub_underscore_fields)]
    pub image_id: u32,
    /// Padding for std430 alignment. Not consumed by callers; the
    /// underscore prefix tells Rust it's intentional.
    #[allow(clippy::pub_underscore_fields)]
    pub _pad: [u32; 1],
}

/// Which compositing layer a placement belongs to, per the kitty spec's
/// z-index banding under "Controlling displayed image layout".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    /// `z < INT32_MIN/2`: draw beneath everything, including the cell
    /// background colors.
    BelowCellBg,
    /// `INT32_MIN/2 <= z < 0`: draw below text glyphs but above the cell
    /// background.
    BelowText,
    /// `z >= 0`: draw above text.
    AboveText,
}

/// Classify a z-index into its compositing [`Layer`] per the kitty spec.
///
/// - `z < INT32_MIN/2` → [`Layer::BelowCellBg`]
/// - `INT32_MIN/2 <= z < 0` → [`Layer::BelowText`]
/// - `z >= 0` → [`Layer::AboveText`]
#[must_use]
pub fn classify_layer(z: i32) -> Layer {
    const HALF_MIN: i32 = i32::MIN / 2;
    if z < HALF_MIN {
        Layer::BelowCellBg
    } else if z < 0 {
        Layer::BelowText
    } else {
        Layer::AboveText
    }
}

impl ImageInstance {
    /// Per-instance attribute layout for the wgpu pipeline. Matches the
    /// layout declared in `shaders/image.wgsl`.
    pub const ATTRS: [wgpu::VertexAttribute; 4] = [
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32x2,
            offset: 0,
            shader_location: 0,
        },
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32x2,
            offset: 8,
            shader_location: 1,
        },
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32x2,
            offset: 16,
            shader_location: 2,
        },
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32x2,
            offset: 24,
            shader_location: 3,
        },
    ];

    /// Vertex buffer layout for the instance stream.
    #[must_use]
    pub fn layout() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &Self::ATTRS,
        }
    }
}

/// Where the active screen sits in the window this frame, for placing
/// line-anchored image placements (see
/// `docs/decisions/image-placement-model.md`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImageView {
    /// Line id of the live screen's row 0 (`Term::image_top_line`). A
    /// placement's screen row is `placement.line - top_line`.
    pub top_line: i64,
    /// Extra downward shift in pixels from looking at scrollback:
    /// `view_offset_lines * cell_h + view_offset_pixel`, exactly the
    /// translation text and rich blocks get. `0.0` at the live bottom.
    pub scroll_px: f32,
    /// Visible area `[x0, y0, x1, y1]` in pixels, relative to the content
    /// origin. Images are cropped to it.
    pub clip: [f32; 4],
    /// The content origin in window pixels (the shader adds it). Image
    /// origins are snapped so `origin + pos` is a whole pixel, which a
    /// centered grid's half-pixel lead would otherwise break.
    pub origin: [f32; 2],
}

impl ImageView {
    /// A view at the live bottom (no scrollback offset) with screen row 0
    /// on line `top_line`, clipped to `size` (`(w, h)` pixels).
    #[must_use]
    pub fn live(top_line: i64, size: (f32, f32)) -> Self {
        Self {
            top_line,
            scroll_px: 0.0,
            clip: [0.0, 0.0, size.0, size.1],
            origin: [0.0, 0.0],
        }
    }
}

/// Build per-instance draw data from `grid` + `registry` + `cache`.
///
/// The output is sorted by `(z, insertion-order)` already — same-z
/// placements draw in insertion order. The caller splits below/above
/// text via the sign of `z`.
///
/// `cell_size` is `(w, h)` in pixels. Each placement's full destination
/// rect is computed from its (never trimmed) cell box, shifted by the
/// view's scroll offset, then intersected with the view's clip rect and
/// the placement's margin clip; the UVs are cropped by the same
/// fractions, so a partly hidden image is cropped, never squashed. This
/// is O(placements): no per-cell work.
#[allow(clippy::cast_precision_loss)] // image dims fit comfortably in 23-bit mantissa.
pub fn build_image_instances(
    out: &mut Vec<ImageInstance>,
    grid: &ImageGrid,
    registry: &ImageRegistry,
    cache: &ImageTextureCache,
    cell_size: (f32, f32),
    view: &ImageView,
) {
    // Two-pass: collect with stable insertion order, then sort by z
    // ascending. Same-z placements keep insertion order via a stable
    // sort.
    let len_before = out.len();
    let (cw, ch) = cell_size;
    for placement in grid.iter() {
        if placement.cols == 0 || placement.rows == 0 || placement.clip_rows.is_empty() {
            continue;
        }
        let Some(image) = registry.get(placement.image_id) else {
            continue;
        };
        // What part of the image is shown (spec defaults + intersection
        // with the image, SIZE-5/6); nothing when it lies outside.
        let src = placement.src_rect.resolve(image.width, image.height);
        if src.is_empty() {
            continue;
        }
        // Top-left of the reserved cell box. Rows are relative to the
        // live screen (negative = in scrollback).
        let row = (placement.line - view.top_line) as f32;
        let box_x = f32::from(placement.col) * cw;
        let box_y = row * ch + view.scroll_px;
        // Destination rect, at the current cell size (SIZE-1/2/3/10/11):
        // native pixels unless `c=`/`r=` were given. Its origin is snapped
        // to whole pixels (the scroll offset stays exact, as for text), so
        // a native-size image maps texels 1:1 with no resampling blur.
        let ((ox, oy), (dst_w, dst_h)) = placement.draw_rect((src.w, src.h), cell_size);
        if dst_w <= 0.0 || dst_h <= 0.0 {
            continue;
        }
        let snap = |v: f32, o: f32| (o + v).round() - o;
        let dst_x = snap(box_x + ox, view.origin[0]);
        let dst_y = snap(row * ch + oy, view.origin[1]) + view.scroll_px;
        // Crop: the view, narrowed by any margin clip. An unclipped edge
        // doesn't constrain (the `Y=` shift may push the image past the
        // box's last row, as before).
        let mut clip = view.clip;
        if placement.clip_rows.start > 0 {
            clip[1] = clip[1].max(box_y + f32::from(placement.clip_rows.start) * ch);
        }
        if placement.clip_rows.end < placement.rows {
            clip[3] = clip[3].min(box_y + f32::from(placement.clip_rows.end) * ch);
        }
        let x0 = dst_x.max(clip[0]);
        let y0 = dst_y.max(clip[1]);
        let x1 = (dst_x + dst_w).min(clip[2]);
        let y1 = (dst_y + dst_h).min(clip[3]);
        // Cull before touching the registry / texture cache: only what
        // is visible this frame is emitted.
        if x1 <= x0 || y1 <= y0 {
            continue;
        }
        let Some(entry) = cache.get(placement.image_id) else {
            continue;
        };
        let iw = image.width.max(1) as f32;
        let ih = image.height.max(1) as f32;
        let uv_min = [src.x as f32 / iw, src.y as f32 / ih];
        let uv_max = [(src.x + src.w) as f32 / iw, (src.y + src.h) as f32 / ih];
        // Map the cropped rect back through the dst -> uv linear map.
        let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
        let uv0 = [
            lerp(uv_min[0], uv_max[0], (x0 - dst_x) / dst_w),
            lerp(uv_min[1], uv_max[1], (y0 - dst_y) / dst_h),
        ];
        let uv1 = [
            lerp(uv_min[0], uv_max[0], (x1 - dst_x) / dst_w),
            lerp(uv_min[1], uv_max[1], (y1 - dst_y) / dst_h),
        ];
        out.push(ImageInstance {
            pos: [x0, y0],
            size: [x1 - x0, y1 - y0],
            uv_min: uv0,
            uv_max: uv1,
            texture_index: entry.texture_index as u32,
            z: placement.z,
            image_id: placement.image_id,
            _pad: [0; 1],
        });
    }
    // m3: sort by (z, image_id) so equal-z placements tie-break by
    // ascending image id (kitty: lower id has lower z). The sort is
    // stable, so any further ties keep insertion order.
    out[len_before..].sort_by_key(|i| (i.z, i.image_id));
}

/// Split a z-sorted `Vec<ImageInstance>` into three layers per the kitty
/// z-index banding: `(below_cell_bg, below_text, above_text)`.
///
/// Instances must already be sorted ascending by z (as
/// [`build_image_instances`] leaves them).
#[must_use]
pub fn split_layers(
    instances: &[ImageInstance],
) -> (&[ImageInstance], &[ImageInstance], &[ImageInstance]) {
    // Boundaries: first index that leaves BelowCellBg, then first index
    // that leaves BelowText. Both monotonic because z is sorted.
    let below_bg_end = instances
        .iter()
        .position(|i| classify_layer(i.z) != Layer::BelowCellBg)
        .unwrap_or(instances.len());
    let below_text_end = instances
        .iter()
        .position(|i| classify_layer(i.z) == Layer::AboveText)
        .unwrap_or(instances.len());
    (
        &instances[..below_bg_end],
        &instances[below_bg_end..below_text_end],
        &instances[below_text_end..],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use toastty_term::{ImageGrid, ImageRegistry, Placement, SrcRect};

    fn red_pixel(width: u32, height: u32) -> toastty_term::ImageData {
        let n = (width * height) as usize;
        let mut pixels = Vec::with_capacity(n * 4);
        for _ in 0..n {
            pixels.extend_from_slice(&[255, 0, 0, 255]);
        }
        toastty_term::ImageData {
            width,
            height,
            pixels,
        }
    }

    fn place(id: u32, rows: std::ops::Range<i64>, cols: std::ops::Range<u16>, z: i32) -> Placement {
        let mut p = Placement::new(
            id,
            u16::try_from(rows.end - rows.start).unwrap(),
            cols.end - cols.start,
        );
        p.line = rows.start;
        p.col = cols.start;
        p.z = z;
        p
    }

    /// A live view with screen row 0 on line 0 and a large clip.
    fn view() -> ImageView {
        ImageView::live(0, (10_000.0, 10_000.0))
    }

    /// Registry + cache holding one `w`x`h` image under id 1.
    fn one_image(w: u32, h: u32) -> (ImageRegistry, ImageTextureCache) {
        let mut reg = ImageRegistry::new(1 << 20);
        reg.insert(1, red_pixel(w, h)).unwrap();
        let mut cache = ImageTextureCache::new(4);
        cache.insert(
            1,
            crate::image::atlas::ImageTexEntry {
                texture_index: 0,
                width: w,
                height: h,
                content_hash: 0,
            },
        );
        (reg, cache)
    }

    fn build(
        grid: &ImageGrid,
        w: u32,
        h: u32,
        cell: (f32, f32),
        v: &ImageView,
    ) -> Vec<ImageInstance> {
        let (reg, cache) = one_image(w, h);
        let mut out = Vec::new();
        build_image_instances(&mut out, grid, &reg, &cache, cell, v);
        out
    }

    /// Float-tolerant `assert_eq!` for a 2-vector.
    #[track_caller]
    fn assert_near(got: [f32; 2], want: [f32; 2], msg: &str) {
        let close = (got[0] - want[0]).abs() < 1e-5 && (got[1] - want[1]).abs() < 1e-5;
        assert!(close, "{msg}: got {got:?}, want {want:?}");
    }

    #[test]
    fn partly_scrolled_off_top_is_cropped_not_squashed() {
        // SCR-1: a 10-row box whose top 3 rows are above the screen.
        let mut grid = ImageGrid::new();
        grid.add(place(1, 0..10, 0..20, 0));
        let v = ImageView::live(3, (400.0, 400.0));
        let out = build(&grid, 200, 100, (10.0, 20.0), &v);
        assert_eq!(out.len(), 1);
        let i = out[0];
        assert_near(i.pos, [0.0, 0.0], "");
        assert_near(i.size, [200.0, 140.0], "7 rows on screen at full scale");
        assert_near(i.uv_min, [0.0, 0.3], "");
        assert_near(i.uv_max, [1.0, 1.0], "");
    }

    #[test]
    fn hanging_off_bottom_and_right_is_cropped() {
        let mut grid = ImageGrid::new();
        grid.add(place(1, 8..12, 6..10, 0));
        let v = ImageView::live(0, (80.0, 200.0)); // 8 cols, 10 rows
        let out = build(&grid, 4, 4, (10.0, 20.0), &v);
        let i = out[0];
        assert_near(i.pos, [60.0, 160.0], "");
        assert_near(i.size, [20.0, 40.0], "");
        assert_near(i.uv_min, [0.0, 0.0], "");
        assert_near(i.uv_max, [0.5, 0.5], "");
    }

    #[test]
    fn scrollback_offset_moves_image_with_text() {
        // SCR-2 / SCR-4: viewing 3 lines + 7 px back shifts the image down
        // by exactly what text gets.
        let mut grid = ImageGrid::new();
        grid.add(place(1, -2..0, 0..4, 0)); // in scrollback
        let mut v = ImageView::live(0, (400.0, 400.0));
        assert!(
            build(&grid, 4, 4, (10.0, 20.0), &v).is_empty(),
            "off-screen"
        );
        v.scroll_px = 3.0 * 20.0 + 7.0;
        let out = build(&grid, 4, 4, (10.0, 20.0), &v);
        let i = out[0];
        assert_near(i.pos, [0.0, 27.0], "row -2 at (-2 + 3) * 20 + 7");
        assert_near(i.size, [40.0, 40.0], "whole");
        assert_near(i.uv_min, [0.0, 0.0], "");
        assert_near(i.uv_max, [1.0, 1.0], "");
    }

    #[test]
    fn sub_row_offset_crops_by_pixels() {
        // Mid smooth-scroll: a box whose top is 5 px above the window.
        let mut grid = ImageGrid::new();
        grid.add(place(1, -1..1, 0..2, 0));
        let mut v = ImageView::live(0, (400.0, 400.0));
        v.scroll_px = 15.0;
        let out = build(&grid, 20, 40, (10.0, 20.0), &v);
        let i = out[0];
        assert_near(i.pos, [0.0, 0.0], "");
        assert_near(i.size, [20.0, 35.0], "");
        assert_near(i.uv_min, [0.0, 5.0 / 40.0], "");
        assert_near(i.uv_max, [1.0, 1.0], "");
    }

    #[test]
    fn margin_clip_crops_rows() {
        // DECSTBM cut the first row of a 4-row box.
        let mut grid = ImageGrid::new();
        let mut p = place(1, 2..6, 0..4, 0);
        p.clip_rows = 1..4;
        grid.add(p);
        let out = build(&grid, 4, 8, (10.0, 20.0), &view());
        let i = out[0];
        assert_near(i.pos, [0.0, 60.0], "");
        assert_near(i.size, [40.0, 60.0], "");
        assert_near(i.uv_min, [0.0, 0.25], "");
        assert_near(i.uv_max, [1.0, 1.0], "");
    }

    #[test]
    fn crop_composes_with_src_rect() {
        // A sub-rect x=0..4, y=4..8 of an 8x8 image, half scrolled off.
        let mut grid = ImageGrid::new();
        let mut p = place(1, 0..2, 0..2, 0);
        p.src_rect = SrcRect {
            x: 0,
            y: 4,
            w: 4,
            h: 4,
        };
        grid.add(p);
        let v = ImageView::live(1, (400.0, 400.0));
        let out = build(&grid, 8, 8, (10.0, 20.0), &v);
        let i = out[0];
        assert_near(i.size, [20.0, 20.0], "");
        assert_near(i.uv_min, [0.0, 0.75], "");
        assert_near(i.uv_max, [0.5, 1.0], "");
    }

    /// A placement drawn at native size (no `c=`/`r=`).
    fn natural(rows: std::ops::Range<i64>, cols: std::ops::Range<u16>) -> Placement {
        let mut p = place(1, rows, cols, 0);
        (p.fit_cols, p.fit_rows) = (0, 0);
        p
    }

    #[test]
    fn natural_size_draws_texels_one_to_one() {
        // SIZE-1: a 205x105 image in a 21x6-cell box is drawn 205x105 at
        // a whole-pixel origin, UVs covering the whole texture.
        let mut grid = ImageGrid::new();
        grid.add(natural(2..8, 3..24));
        let out = build(&grid, 205, 105, (10.0, 19.0), &view());
        let i = out[0];
        assert_near(i.pos, [30.0, 38.0], "");
        assert_near(i.size, [205.0, 105.0], "");
        assert_near(i.uv_min, [0.0, 0.0], "");
        assert_near(i.uv_max, [1.0, 1.0], "");
        // A fractional cell size still lands on whole pixels.
        let out = build(&grid, 205, 105, (9.6, 19.4), &view());
        assert_near(out[0].pos, [29.0, 39.0], "28.8, 38.8 rounded");
        assert_near(out[0].size, [205.0, 105.0], "");
        // A centered grid's half-pixel origin: snapped in window space.
        let mut v = view();
        v.origin = [0.5, 0.25];
        let out = build(&grid, 205, 105, (10.0, 19.0), &v);
        assert_near(out[0].pos, [30.5, 37.75], "origin + pos = 31, 38");
    }

    #[test]
    fn natural_size_wider_than_the_screen_is_truncated_not_squashed() {
        // SIZE-7: cropped at the clip edge at native scale.
        let mut grid = ImageGrid::new();
        grid.add(natural(0..1, 40..190));
        let v = ImageView::live(0, (800.0, 600.0));
        let out = build(&grid, 1500, 10, (10.0, 19.0), &v);
        let i = out[0];
        assert_near(i.pos, [400.0, 0.0], "");
        assert_near(i.size, [400.0, 10.0], "");
        assert_near(i.uv_max, [400.0 / 1500.0, 1.0], "");
    }

    #[test]
    fn partial_src_rect_and_offsets_follow_kitty() {
        // SIZE-5: `x=150` alone shows the right 50 px; SIZE-10: X=25 on
        // 10 px cells clamps to 9.
        let mut grid = ImageGrid::new();
        let mut p = natural(0..6, 0..6);
        p.src_rect = SrcRect {
            x: 150,
            y: 0,
            w: 0,
            h: 0,
        };
        p.pix_offset = (25, 0);
        grid.add(p);
        let out = build(&grid, 200, 100, (10.0, 19.0), &view());
        let i = out[0];
        assert_near(i.pos, [9.0, 0.0], "");
        assert_near(i.size, [50.0, 100.0], "");
        assert_near(i.uv_min, [0.75, 0.0], "");
        assert_near(i.uv_max, [1.0, 1.0], "");
    }

    #[test]
    fn src_rect_outside_the_image_draws_nothing() {
        // SIZE-6: kitty shows nothing (no edge-pixel smear).
        let mut grid = ImageGrid::new();
        let mut p = natural(0..6, 0..6);
        p.src_rect = SrcRect {
            x: 250,
            y: 0,
            w: 50,
            h: 0,
        };
        grid.add(p);
        assert!(build(&grid, 200, 100, (10.0, 19.0), &view()).is_empty());
    }

    #[test]
    fn cols_only_follows_aspect_at_the_current_cell_size() {
        // SIZE-2 / SIZE-11: c=10 on 400x200 is 100x50 at 10 px cells and
        // 120x60 at 12 px cells.
        let mut grid = ImageGrid::new();
        let mut p = place(1, 0..3, 0..10, 0);
        p.fit_rows = 0;
        grid.add(p);
        let out = build(&grid, 400, 200, (10.0, 19.0), &view());
        assert_near(out[0].size, [100.0, 50.0], "");
        let out = build(&grid, 400, 200, (12.0, 29.0), &view());
        assert_near(out[0].size, [120.0, 60.0], "");
    }

    #[test]
    fn empty_grid_emits_no_instances() {
        let grid = ImageGrid::new();
        let reg = ImageRegistry::new(1024);
        let cache = ImageTextureCache::new(4);
        let mut out = Vec::new();
        build_image_instances(&mut out, &grid, &reg, &cache, (10.0, 20.0), &view());
        assert!(out.is_empty());
    }

    #[test]
    fn placement_without_cache_entry_is_skipped() {
        let mut grid = ImageGrid::new();
        grid.add(place(1, 0..2, 0..4, 0));
        let mut reg = ImageRegistry::new(4096);
        reg.insert(1, red_pixel(2, 2)).unwrap();
        let cache = ImageTextureCache::new(4); // no insert.
        let mut out = Vec::new();
        build_image_instances(&mut out, &grid, &reg, &cache, (10.0, 20.0), &view());
        assert!(out.is_empty());
    }

    #[test]
    fn single_placement_emits_one_instance() {
        let mut grid = ImageGrid::new();
        grid.add(place(1, 0..2, 0..4, 0));
        let mut reg = ImageRegistry::new(4096);
        reg.insert(1, red_pixel(2, 2)).unwrap();
        let mut cache = ImageTextureCache::new(4);
        cache.insert(
            1,
            crate::image::atlas::ImageTexEntry {
                texture_index: 7,
                width: 2,
                height: 2,
                content_hash: 0,
            },
        );
        let mut out = Vec::new();
        build_image_instances(&mut out, &grid, &reg, &cache, (10.0, 20.0), &view());
        assert_eq!(out.len(), 1);
        let i = out[0];
        assert_eq!(i.pos, [0.0, 0.0]);
        assert_eq!(i.size, [40.0, 40.0]);
        assert_eq!(i.uv_min, [0.0, 0.0]);
        assert_eq!(i.uv_max, [1.0, 1.0]);
        assert_eq!(i.texture_index, 7);
        assert_eq!(i.z, 0);
    }

    #[test]
    fn placements_sorted_by_z_ascending() {
        let mut grid = ImageGrid::new();
        grid.add(place(1, 0..1, 0..1, 10));
        grid.add(place(1, 0..1, 1..2, -5));
        grid.add(place(1, 0..1, 2..3, 0));
        let mut reg = ImageRegistry::new(4096);
        reg.insert(1, red_pixel(1, 1)).unwrap();
        let mut cache = ImageTextureCache::new(4);
        cache.insert(
            1,
            crate::image::atlas::ImageTexEntry {
                texture_index: 0,
                width: 1,
                height: 1,
                content_hash: 0,
            },
        );
        let mut out = Vec::new();
        build_image_instances(&mut out, &grid, &reg, &cache, (10.0, 10.0), &view());
        let zs: Vec<i32> = out.iter().map(|i| i.z).collect();
        assert_eq!(zs, vec![-5, 0, 10]);
    }

    #[test]
    fn split_layers_partitions_three_bands() {
        let mut grid = ImageGrid::new();
        grid.add(place(1, 0..1, 0..1, -1));
        grid.add(place(1, 0..1, 1..2, -2));
        grid.add(place(1, 0..1, 2..3, 0));
        grid.add(place(1, 0..1, 3..4, 5));
        grid.add(place(1, 0..1, 4..5, i32::MIN));
        let mut reg = ImageRegistry::new(4096);
        reg.insert(1, red_pixel(1, 1)).unwrap();
        let mut cache = ImageTextureCache::new(4);
        cache.insert(
            1,
            crate::image::atlas::ImageTexEntry {
                texture_index: 0,
                width: 1,
                height: 1,
                content_hash: 0,
            },
        );
        let mut out = Vec::new();
        build_image_instances(&mut out, &grid, &reg, &cache, (10.0, 10.0), &view());
        let (below_bg, below_text, above) = split_layers(&out);
        assert_eq!(below_bg.len(), 1);
        assert_eq!(below_text.len(), 2);
        assert_eq!(above.len(), 2);
        assert!(
            below_bg
                .iter()
                .all(|i| classify_layer(i.z) == Layer::BelowCellBg)
        );
        assert!(
            below_text
                .iter()
                .all(|i| classify_layer(i.z) == Layer::BelowText)
        );
        assert!(above.iter().all(|i| i.z >= 0));
    }

    #[test]
    fn sub_rect_normalizes_to_image_dims() {
        let mut grid = ImageGrid::new();
        let mut p = place(1, 0..2, 0..4, 0);
        p.src_rect = SrcRect {
            x: 1,
            y: 1,
            w: 2,
            h: 2,
        };
        grid.add(p);
        let mut reg = ImageRegistry::new(4096);
        reg.insert(1, red_pixel(4, 4)).unwrap();
        let mut cache = ImageTextureCache::new(4);
        cache.insert(
            1,
            crate::image::atlas::ImageTexEntry {
                texture_index: 0,
                width: 4,
                height: 4,
                content_hash: 0,
            },
        );
        let mut out = Vec::new();
        build_image_instances(&mut out, &grid, &reg, &cache, (10.0, 10.0), &view());
        assert_eq!(out[0].uv_min, [0.25, 0.25]);
        assert_eq!(out[0].uv_max, [0.75, 0.75]);
    }
}

#[cfg(test)]
mod minor_render_tests {
    use super::*;
    use toastty_term::{ImageGrid, ImageRegistry, Placement};

    fn view() -> ImageView {
        ImageView::live(0, (10_000.0, 10_000.0))
    }

    fn red_pixel(width: u32, height: u32) -> toastty_term::ImageData {
        let n = (width * height) as usize;
        let mut pixels = Vec::with_capacity(n * 4);
        for _ in 0..n {
            pixels.extend_from_slice(&[255, 0, 0, 255]);
        }
        toastty_term::ImageData {
            width,
            height,
            pixels,
        }
    }

    fn place(id: u32, z: i32) -> Placement {
        let mut p = Placement::new(id, 1, 1);
        p.z = z;
        p
    }

    // m3: equal z, differing image ids must sort by ascending image id,
    // regardless of insertion order.
    #[test]
    fn equal_z_ties_break_by_ascending_image_id() {
        let mut grid = ImageGrid::new();
        // Insert in descending id order to prove the sort, not insertion
        // order, decides.
        grid.add(place(30, 7));
        grid.add(place(10, 7));
        grid.add(place(20, 7));
        let mut reg = ImageRegistry::new(8192);
        let mut cache = ImageTextureCache::new(8);
        for id in [10u32, 20, 30] {
            reg.insert(id, red_pixel(1, 1)).unwrap();
            cache.insert(
                id,
                crate::image::atlas::ImageTexEntry {
                    texture_index: id as usize,
                    width: 1,
                    height: 1,
                    content_hash: 0,
                },
            );
        }
        let mut out = Vec::new();
        build_image_instances(&mut out, &grid, &reg, &cache, (10.0, 10.0), &view());
        let ids: Vec<u32> = out.iter().map(|i| i.image_id).collect();
        assert_eq!(ids, vec![10, 20, 30]);
    }

    // m2: layer classification by z-index band.
    #[test]
    fn classify_layer_bands() {
        assert_eq!(classify_layer(i32::MIN), Layer::BelowCellBg);
        assert_eq!(classify_layer(i32::MIN / 2 - 1), Layer::BelowCellBg);
        // Boundary: z == INT32_MIN/2 is in the below-text band (>= MIN/2).
        assert_eq!(classify_layer(i32::MIN / 2), Layer::BelowText);
        assert_eq!(classify_layer(-1), Layer::BelowText);
        assert_eq!(classify_layer(0), Layer::AboveText);
        assert_eq!(classify_layer(5), Layer::AboveText);
    }
}
