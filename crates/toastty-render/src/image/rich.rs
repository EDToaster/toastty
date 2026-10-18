//! Rich blocks (OSC 7700, see `docs/markdown.md`): GPU mirror of the
//! term's [`toastty_term::RichBlockStore`] images and the per-row slice
//! instances.
//!
//! Each block image is uploaded once into the shared [`ImagePipeline`]
//! texture pool and re-uploaded when its revision changes. Images taller
//! than the GPU's max texture dimension are split into tiles whose height
//! is a whole number of cell rows, so a row's slice never straddles two
//! textures. Each visible tagged row draws the `cell_h`-tall slice
//! `index * cell_h .. (index + 1) * cell_h` of its block across the row.

use std::collections::HashMap;

use toastty_term::{RichBlockId, RichImage, Term};

use super::instance::ImageInstance;
use super::pipeline::ImagePipeline;

/// Never keep more than this many block textures resident while not
/// visible (visible ones are always kept).
pub const MAX_IDLE_BLOCKS: usize = 8;

/// One texture holding rows `y0 .. y0 + height` of a block image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RichTile {
    pub texture_index: usize,
    pub y0: u32,
    pub height: u32,
}

/// GPU state of one block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RichTexEntry {
    pub revision: u32,
    pub width: u32,
    pub height: u32,
    pub cell_h: u32,
    pub tiles: Vec<RichTile>,
    last_frame: u64,
}

/// Split an image of `height` px into tiles of at most `max_dim` px,
/// each a whole number of `cell_h` rows (so no row straddles tiles).
/// Returns `(y0, height)` pairs; empty when a single row doesn't fit.
#[must_use]
pub fn plan_tiles(height: u32, cell_h: u32, max_dim: u32) -> Vec<(u32, u32)> {
    if cell_h == 0 || cell_h > max_dim || height == 0 {
        return Vec::new();
    }
    let tile_h = (max_dim / cell_h) * cell_h;
    let mut out = Vec::new();
    let mut y = 0;
    while y < height {
        let h = tile_h.min(height - y);
        out.push((y, h));
        y += h;
    }
    out
}

/// Texture index and UV rect of row `index` of a block.
#[must_use]
pub fn row_slice(entry: &RichTexEntry, index: u16) -> Option<(usize, [f32; 2], [f32; 2])> {
    let y = u32::from(index) * entry.cell_h;
    if y + entry.cell_h > entry.height {
        return None;
    }
    let tile = entry
        .tiles
        .iter()
        .find(|t| y >= t.y0 && y + entry.cell_h <= t.y0 + t.height)?;
    #[allow(clippy::cast_precision_loss)]
    let (y0, y1, th) = (
        (y - tile.y0) as f32,
        (y - tile.y0 + entry.cell_h) as f32,
        tile.height as f32,
    );
    Some((tile.texture_index, [0.0, y0 / th], [1.0, y1 / th]))
}

/// Block id → GPU textures.
#[derive(Debug, Default)]
pub struct RichTextureCache {
    entries: HashMap<RichBlockId, RichTexEntry>,
    frame: u64,
    /// 1×1 selection tint texture and the colour it holds.
    tint: Option<(usize, [u8; 4])>,
}

impl RichTextureCache {
    #[must_use]
    pub fn get(&self, id: RichBlockId) -> Option<&RichTexEntry> {
        self.entries.get(&id)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn free(pipe: &mut ImagePipeline, e: &RichTexEntry) {
        for t in &e.tiles {
            pipe.free_texture(t.texture_index);
        }
    }

    /// Bring the textures for the blocks visible this frame up to date
    /// (`visible` lists their ids) and drop textures for blocks that are
    /// gone, evicted, or idle past [`MAX_IDLE_BLOCKS`]. `max_dim` is the
    /// GPU's max 2D texture dimension. Returns `true` when anything was
    /// uploaded or freed.
    #[allow(clippy::too_many_arguments)]
    pub fn sync(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pipe: &mut ImagePipeline,
        term: &Term,
        visible: &[RichBlockId],
        cell_h: u32,
        max_dim: u32,
    ) -> bool {
        self.frame += 1;
        let frame = self.frame;
        let store = term.rich_blocks();
        let mut changed = false;
        // Drop entries whose block is gone, re-imaged, or text-only now.
        let stale: Vec<RichBlockId> = self
            .entries
            .iter()
            .filter(|(id, e)| {
                store.block_revision(**id) != Some(e.revision)
                    || store.get(**id).and_then(|b| b.image()).is_none()
                    || e.cell_h != cell_h
            })
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            if let Some(e) = self.entries.remove(&id) {
                Self::free(pipe, &e);
                changed = true;
            }
        }
        for &id in visible {
            if let Some(e) = self.entries.get_mut(&id) {
                e.last_frame = frame;
                continue;
            }
            let (Some(img), Some(rev)) = (store.image(id), store.block_revision(id)) else {
                continue;
            };
            if let Some(e) = upload(device, queue, pipe, img, rev, cell_h, max_dim, frame) {
                self.entries.insert(id, e);
                changed = true;
            }
        }
        // Bound idle residency.
        while self.entries.len() > visible.len() + MAX_IDLE_BLOCKS {
            let Some(victim) = self
                .entries
                .iter()
                .filter(|(_, e)| e.last_frame != frame)
                .min_by_key(|(_, e)| e.last_frame)
                .map(|(id, _)| *id)
            else {
                break;
            };
            if let Some(e) = self.entries.remove(&victim) {
                Self::free(pipe, &e);
                changed = true;
            }
        }
        changed
    }

    /// Texture index of a 1×1 texture holding `rgba` (sRGB, straight
    /// alpha), uploading it on first use / colour change.
    pub fn tint_texture(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pipe: &mut ImagePipeline,
        rgba: [u8; 4],
    ) -> usize {
        if let Some((idx, c)) = self.tint
            && c == rgba
        {
            return idx;
        }
        if let Some((idx, _)) = self.tint.take() {
            pipe.free_texture(idx);
        }
        let idx = pipe.upload_rgba(device, queue, 1, 1, &rgba);
        self.tint = Some((idx, rgba));
        idx
    }
}

#[allow(clippy::too_many_arguments)]
fn upload(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipe: &mut ImagePipeline,
    img: &RichImage,
    revision: u32,
    cell_h: u32,
    max_dim: u32,
    frame: u64,
) -> Option<RichTexEntry> {
    if img.width == 0 || img.width > max_dim {
        return None;
    }
    let plan = plan_tiles(img.height, cell_h, max_dim);
    if plan.is_empty() {
        return None;
    }
    let stride = img.width as usize * 4;
    let mut tiles = Vec::with_capacity(plan.len());
    for (y0, h) in plan {
        let start = y0 as usize * stride;
        let end = start + h as usize * stride;
        let bytes = img.pixels.get(start..end)?;
        let idx = pipe.upload_rgba(device, queue, img.width, h, bytes);
        tiles.push(RichTile {
            texture_index: idx,
            y0,
            height: h,
        });
    }
    Some(RichTexEntry {
        revision,
        width: img.width,
        height: img.height,
        cell_h,
        tiles,
        last_frame: frame,
    })
}

/// One rendered row that shows a block slice.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RichRowDraw {
    /// Top of the row in content-relative pixels.
    pub y: f32,
    pub block: RichBlockId,
    pub index: u16,
}

/// Push the slice instance of every row in `rows` whose block has
/// resident textures. Rows without one are skipped (their text draws).
pub fn build_rich_instances(
    out: &mut Vec<ImageInstance>,
    cache: &RichTextureCache,
    rows: &[RichRowDraw],
    cell_h: f32,
) {
    for r in rows {
        let Some(entry) = cache.get(r.block) else {
            continue;
        };
        let Some((tex, uv_min, uv_max)) = row_slice(entry, r.index) else {
            continue;
        };
        #[allow(clippy::cast_precision_loss)]
        let w = entry.width as f32;
        #[allow(clippy::cast_possible_truncation)]
        out.push(ImageInstance {
            pos: [0.0, r.y],
            size: [w, cell_h],
            uv_min,
            uv_max,
            texture_index: tex as u32,
            z: -1,
            image_id: 0,
            _pad: [0; 1],
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(height: u32, cell_h: u32, tiles: &[(u32, u32)]) -> RichTexEntry {
        RichTexEntry {
            revision: 0,
            width: 80,
            height,
            cell_h,
            tiles: tiles
                .iter()
                .enumerate()
                .map(|(i, (y0, h))| RichTile {
                    texture_index: i + 10,
                    y0: *y0,
                    height: *h,
                })
                .collect(),
            last_frame: 0,
        }
    }

    #[test]
    fn plan_single_tile_when_it_fits() {
        assert_eq!(plan_tiles(160, 16, 8192), vec![(0, 160)]);
    }

    #[test]
    fn plan_splits_on_row_boundaries() {
        // 8192 / 20 = 409 rows → 8180 px tiles.
        let p = plan_tiles(20 * 1000, 20, 8192);
        assert_eq!(p, vec![(0, 8180), (8180, 8180), (16360, 3640)]);
        assert!(p.iter().all(|(y, h)| y % 20 == 0 && h % 20 == 0));
        assert_eq!(p.iter().map(|(_, h)| h).sum::<u32>(), 20_000);
    }

    #[test]
    fn plan_rejects_degenerate() {
        assert_eq!(plan_tiles(0, 16, 8192), vec![]);
        assert_eq!(plan_tiles(32, 0, 8192), vec![]);
        assert_eq!(plan_tiles(32, 16, 8), vec![]);
    }

    #[test]
    fn row_slice_maps_rows_to_tiles_and_uvs() {
        let e = entry(64, 16, &[(0, 32), (32, 32)]);
        assert_eq!(row_slice(&e, 0), Some((10, [0.0, 0.0], [1.0, 0.5])));
        assert_eq!(row_slice(&e, 1), Some((10, [0.0, 0.5], [1.0, 1.0])));
        assert_eq!(row_slice(&e, 2), Some((11, [0.0, 0.0], [1.0, 0.5])));
        assert_eq!(row_slice(&e, 3), Some((11, [0.0, 0.5], [1.0, 1.0])));
        assert_eq!(row_slice(&e, 4), None);
    }

    #[test]
    fn instances_only_for_resident_blocks() {
        let mut cache = RichTextureCache::default();
        cache.entries.insert(7, entry(32, 16, &[(0, 32)]));
        let rows = [
            RichRowDraw {
                y: 0.0,
                block: 7,
                index: 1,
            },
            RichRowDraw {
                y: 16.0,
                block: 8,
                index: 0,
            },
        ];
        let mut out = Vec::new();
        build_rich_instances(&mut out, &cache, &rows, 16.0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].pos, [0.0, 0.0]);
        assert_eq!(out[0].size, [80.0, 16.0]);
        assert_eq!(out[0].uv_min, [0.0, 0.5]);
        assert_eq!(out[0].texture_index, 10);
    }

    /// Solid-colour fake layout: one row per source line.
    struct Solid(u8);

    impl toastty_term::RichBlockLayout for Solid {
        fn layout(
            &mut self,
            req: &toastty_term::RichLayoutRequest<'_>,
        ) -> Option<toastty_term::RichLayout> {
            let rows = u16::try_from(req.source.lines().count().max(1)).unwrap();
            let w = u32::from(req.cols) * req.cell_w;
            let h = u32::from(rows) * req.cell_h;
            Some(toastty_term::RichLayout {
                rows,
                image: Some(RichImage {
                    width: w,
                    height: h,
                    pixels: vec![self.0; (w * h * 4) as usize].into(),
                }),
                text: Vec::new(),
                links: Vec::new(),
            })
        }
    }

    fn term_with_block(lines: usize) -> Term {
        let mut t = Term::new(40, 4, 0);
        t.set_rich_layout(Box::new(Solid(200)));
        let src = vec!["x"; lines].join("\n");
        let b64 = {
            // Minimal base64 encoder (no dev-dependency on the crate).
            const T: &[u8; 64] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let b = src.as_bytes();
            let mut out = String::new();
            for chunk in b.chunks(3) {
                let n = match chunk.len() {
                    3 => {
                        (u32::from(chunk[0]) << 16)
                            | (u32::from(chunk[1]) << 8)
                            | u32::from(chunk[2])
                    }
                    2 => (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8),
                    _ => u32::from(chunk[0]) << 16,
                };
                for i in 0..=chunk.len() {
                    out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
                }
            }
            out
        };
        let seq = format!("\x1b]7700;md;;{b64}\x1b\\");
        let mut p = toastty_parser::Parser::new();
        p.advance(&mut t, seq.as_bytes());
        t
    }

    #[test]
    fn gpu_sync_uploads_tiles_and_reuploads_on_revision() {
        let Some((device, queue)) = crate::test_gpu::try_device_and_queue() else {
            return; // no GPU in this sandbox
        };
        let mut pipe = ImagePipeline::new(&device, wgpu::TextureFormat::Rgba8UnormSrgb);
        let mut t = term_with_block(10);
        let id = t.row(0).rich.unwrap().block;
        let mut cache = RichTextureCache::default();
        // Default cell 8x16; max_dim 64 → 4 rows per tile → 3 tiles.
        assert!(cache.sync(&device, &queue, &mut pipe, &t, &[id], 16, 64));
        let e = cache.get(id).unwrap().clone();
        assert_eq!(e.tiles.len(), 3);
        assert_eq!(
            e.tiles.iter().map(|t| t.height).collect::<Vec<_>>(),
            [64, 64, 32]
        );
        assert_eq!(
            row_slice(&e, 9).map(|s| s.0),
            Some(e.tiles[2].texture_index)
        );
        // Nothing changed → no upload.
        assert!(!cache.sync(&device, &queue, &mut pipe, &t, &[id], 16, 64));
        // Palette change → new image revision → re-upload.
        let mut env = t.rich_env();
        env.palette.fg = [9, 9, 9];
        t.set_rich_env(env);
        t.sync_rich_blocks();
        assert!(cache.sync(&device, &queue, &mut pipe, &t, &[id], 16, 64));
        assert_ne!(cache.get(id).unwrap().revision, e.revision);
        // Block gone → textures freed.
        let mut p = toastty_parser::Parser::new();
        p.advance(&mut t, b"\x1b[2J");
        t.sync_rich_blocks();
        assert!(cache.sync(&device, &queue, &mut pipe, &t, &[], 16, 64));
        assert!(cache.is_empty());
    }

    #[test]
    fn gpu_sync_skips_images_wider_than_the_limit() {
        let Some((device, queue)) = crate::test_gpu::try_device_and_queue() else {
            return;
        };
        let mut pipe = ImagePipeline::new(&device, wgpu::TextureFormat::Rgba8UnormSrgb);
        let t = term_with_block(1);
        let id = t.row(0).rich.unwrap().block;
        let mut cache = RichTextureCache::default();
        // 4 cols * 8 px = 32 px wide > 16 → text fallback.
        cache.sync(&device, &queue, &mut pipe, &t, &[id], 16, 16);
        assert!(cache.get(id).is_none());
    }
}
