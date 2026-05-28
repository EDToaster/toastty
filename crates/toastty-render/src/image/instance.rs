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
    /// Padding for std430 alignment. Not consumed by callers; the
    /// underscore prefix tells Rust it's intentional.
    #[allow(clippy::pub_underscore_fields)]
    pub _pad: [u32; 2],
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

/// Build per-instance draw data from `grid` + `registry` + `cache`.
///
/// The output is sorted by `(z, insertion-order)` already — same-z
/// placements draw in insertion order. The caller splits below/above
/// text via the sign of `z`.
///
/// `cell_size` is `(w, h)` in pixels.
#[allow(clippy::cast_precision_loss)] // image dims fit comfortably in 23-bit mantissa.
pub fn build_image_instances(
    out: &mut Vec<ImageInstance>,
    grid: &ImageGrid,
    registry: &ImageRegistry,
    cache: &ImageTextureCache,
    cell_size: (f32, f32),
) {
    // Two-pass: collect with stable insertion order, then sort by z
    // ascending. Same-z placements keep insertion order via a stable
    // sort.
    let len_before = out.len();
    for placement in grid.iter() {
        let Some(image) = registry.get(placement.image_id) else {
            continue;
        };
        let Some(entry) = cache.get(placement.image_id) else {
            continue;
        };
        let cols = placement.col_range.end.saturating_sub(placement.col_range.start);
        let rows = placement.row_range.end.saturating_sub(placement.row_range.start);
        if cols == 0 || rows == 0 {
            continue;
        }
        // M3: `X=`/`Y=` are intra-cell pixel offsets within the first
        // cell. Add them on top of the cell-aligned position.
        let pos_x =
            f32::from(placement.col_range.start) * cell_size.0 + placement.pix_offset.0 as f32;
        let pos_y =
            f32::from(placement.row_range.start) * cell_size.1 + placement.pix_offset.1 as f32;
        let size_x = f32::from(cols) * cell_size.0;
        let size_y = f32::from(rows) * cell_size.1;
        let (uv_min, uv_max) = if placement.src_rect.is_full() {
            ([0.0, 0.0], [1.0, 1.0])
        } else {
            let iw = image.width.max(1) as f32;
            let ih = image.height.max(1) as f32;
            let x0 = placement.src_rect.x as f32 / iw;
            let y0 = placement.src_rect.y as f32 / ih;
            let x1 = (placement.src_rect.x + placement.src_rect.w) as f32 / iw;
            let y1 = (placement.src_rect.y + placement.src_rect.h) as f32 / ih;
            ([x0, y0], [x1, y1])
        };
        out.push(ImageInstance {
            pos: [pos_x, pos_y],
            size: [size_x, size_y],
            uv_min,
            uv_max,
            texture_index: entry.texture_index as u32,
            z: placement.z,
            _pad: [0; 2],
        });
    }
    // Stable sort by z so below-text and above-text both come out in
    // the order they were inserted.
    out[len_before..].sort_by_key(|i| i.z);
}

/// Split a `Vec<ImageInstance>` into `(below_text, above_text)` slices
/// based on the sign of `z`. `z < 0` is below; `z >= 0` is above.
#[must_use]
pub fn split_below_above(instances: &[ImageInstance]) -> (&[ImageInstance], &[ImageInstance]) {
    // Already z-sorted; find first index where z >= 0.
    let split = instances
        .iter()
        .position(|i| i.z >= 0)
        .unwrap_or(instances.len());
    (&instances[..split], &instances[split..])
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

    fn place(id: u32, rows: std::ops::Range<u16>, cols: std::ops::Range<u16>, z: i32) -> Placement {
        Placement {
            image_id: id,
            placement_id: 0,
            row_range: rows,
            col_range: cols,
            src_rect: SrcRect::FULL,
            z,
            pix_offset: (0, 0),
        }
    }

    #[test]
    fn empty_grid_emits_no_instances() {
        let grid = ImageGrid::new();
        let reg = ImageRegistry::new(1024);
        let cache = ImageTextureCache::new(4);
        let mut out = Vec::new();
        build_image_instances(&mut out, &grid, &reg, &cache, (10.0, 20.0));
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
        build_image_instances(&mut out, &grid, &reg, &cache, (10.0, 20.0));
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
        build_image_instances(&mut out, &grid, &reg, &cache, (10.0, 20.0));
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
        build_image_instances(&mut out, &grid, &reg, &cache, (10.0, 10.0));
        let zs: Vec<i32> = out.iter().map(|i| i.z).collect();
        assert_eq!(zs, vec![-5, 0, 10]);
    }

    #[test]
    fn split_below_above_partitions_by_z_sign() {
        let mut grid = ImageGrid::new();
        grid.add(place(1, 0..1, 0..1, -1));
        grid.add(place(1, 0..1, 1..2, -2));
        grid.add(place(1, 0..1, 2..3, 0));
        grid.add(place(1, 0..1, 3..4, 5));
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
        build_image_instances(&mut out, &grid, &reg, &cache, (10.0, 10.0));
        let (below, above) = split_below_above(&out);
        assert_eq!(below.len(), 2);
        assert_eq!(above.len(), 2);
        assert!(below.iter().all(|i| i.z < 0));
        assert!(above.iter().all(|i| i.z >= 0));
    }

    #[test]
    fn sub_rect_normalizes_to_image_dims() {
        let mut grid = ImageGrid::new();
        grid.add(Placement {
            image_id: 1,
            placement_id: 0,
            row_range: 0..2,
            col_range: 0..4,
            src_rect: SrcRect {
                x: 1,
                y: 1,
                w: 2,
                h: 2,
            },
            z: 0,
            pix_offset: (0, 0),
        });
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
        build_image_instances(&mut out, &grid, &reg, &cache, (10.0, 10.0));
        assert_eq!(out[0].uv_min, [0.25, 0.25]);
        assert_eq!(out[0].uv_max, [0.75, 0.75]);
    }
}
