//! Rich blocks (OSC 7700, see `docs/markdown.md`): GPU mirror of the
//! term's [`toastty_term::RichBlockStore`] images and the per-row slice
//! instances.
//!
//! Block images are painted lazily ([`RichImage::paint`]) and uploaded
//! in fixed tiles of [`TILE_ROWS`] cell rows, and only the tiles holding
//! a visible row are painted. A re-layout (every step of a window resize
//! re-lays out each block) therefore costs one screenful of pixels, not
//! the whole document. Each visible tagged row draws the `cell_h`-tall
//! slice `index * cell_h .. (index + 1) * cell_h` of its block across the
//! row; a row never straddles two tiles.

use std::collections::HashMap;

use toastty_term::{RichBlockId, RichImage, RichRow, Term};

use super::instance::ImageInstance;
use super::pipeline::ImagePipeline;

/// Cell rows per tile (fewer when the GPU's max texture size demands).
pub const TILE_ROWS: u32 = 16;

/// Never keep more than this many tiles resident while not visible
/// (visible ones are always kept).
pub const MAX_IDLE_TILES: usize = 64;

/// One texture holding rows `y0 .. y0 + height` of a block image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RichTile {
    pub texture_index: usize,
    pub y0: u32,
    pub height: u32,
    last_frame: u64,
}

/// GPU state of one block: the tiles painted so far, by tile number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RichTexEntry {
    pub revision: u32,
    pub width: u32,
    pub height: u32,
    pub cell_h: u32,
    /// Cell rows per tile.
    pub tile_rows: u32,
    pub tiles: HashMap<u32, RichTile>,
}

/// Cell rows per tile for this cell height and texture limit (0 when a
/// single row doesn't fit).
#[must_use]
pub fn tile_rows(cell_h: u32, max_dim: u32) -> u32 {
    if cell_h == 0 || cell_h > max_dim {
        return 0;
    }
    (max_dim / cell_h).min(TILE_ROWS)
}

impl RichTexEntry {
    /// Pixel band `(y0, height)` of tile `t`, or `None` past the image.
    #[must_use]
    pub fn tile_span(&self, t: u32) -> Option<(u32, u32)> {
        let tile_h = self.tile_rows * self.cell_h;
        let y0 = t.checked_mul(tile_h)?;
        (tile_h > 0 && y0 < self.height).then(|| (y0, tile_h.min(self.height - y0)))
    }
}

/// Texture index and UV rect of row `index` of a block, when its tile
/// is resident.
#[must_use]
pub fn row_slice(entry: &RichTexEntry, index: u16) -> Option<(usize, [f32; 2], [f32; 2])> {
    if entry.tile_rows == 0 {
        return None;
    }
    let y = u32::from(index) * entry.cell_h;
    if y + entry.cell_h > entry.height {
        return None;
    }
    let tile = entry.tiles.get(&(u32::from(index) / entry.tile_rows))?;
    #[allow(clippy::cast_precision_loss)]
    let (y0, y1, th) = (
        (y - tile.y0) as f32,
        (y - tile.y0 + entry.cell_h) as f32,
        tile.height as f32,
    );
    Some((tile.texture_index, [0.0, y0 / th], [1.0, y1 / th]))
}

/// Block id → GPU tiles.
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

    /// Resident tiles across all blocks.
    #[must_use]
    pub fn tile_count(&self) -> usize {
        self.entries.values().map(|e| e.tiles.len()).sum()
    }

    fn free(pipe: &mut ImagePipeline, e: &RichTexEntry) {
        for t in e.tiles.values() {
            pipe.free_texture(t.texture_index);
        }
    }

    /// Bring the tiles behind the rows visible this frame (`visible`
    /// lists their block tags) up to date, painting and uploading any
    /// that are missing, and drop tiles for blocks that are gone,
    /// re-imaged, or idle past [`MAX_IDLE_TILES`]. `max_dim` is the GPU's
    /// max 2D texture dimension. Returns `true` when anything was
    /// uploaded or freed.
    #[allow(clippy::too_many_arguments)]
    pub fn sync(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pipe: &mut ImagePipeline,
        term: &Term,
        visible: &[RichRow],
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
        let rows_per_tile = tile_rows(cell_h, max_dim);
        let mut warmed: Vec<RichBlockId> = Vec::new();
        for tag in visible {
            let id = tag.block;
            if !warmed.contains(&id) {
                // Keep the block warm in the store's LRU while on screen.
                let _ = store.image(id);
                warmed.push(id);
            }
            if let std::collections::hash_map::Entry::Vacant(slot) = self.entries.entry(id) {
                let (Some(img), Some(rev)) = (store.image(id), store.block_revision(id)) else {
                    continue;
                };
                if img.width == 0 || img.width > max_dim || rows_per_tile == 0 {
                    continue;
                }
                slot.insert(RichTexEntry {
                    revision: rev,
                    width: img.width,
                    height: img.height,
                    cell_h,
                    tile_rows: rows_per_tile,
                    tiles: HashMap::new(),
                });
            }
            let Some(e) = self.entries.get_mut(&id) else {
                continue;
            };
            let t = u32::from(tag.index) / e.tile_rows;
            if let Some(tile) = e.tiles.get_mut(&t) {
                tile.last_frame = frame;
                continue;
            }
            let Some((y0, h)) = e.tile_span(t) else {
                continue;
            };
            let Some(img) = store.image(id) else { continue };
            let Some(bytes) = paint_tile(img, y0, h) else {
                continue;
            };
            let idx = pipe.upload_rgba(device, queue, e.width, h, &bytes);
            e.tiles.insert(
                t,
                RichTile {
                    texture_index: idx,
                    y0,
                    height: h,
                    last_frame: frame,
                },
            );
            changed = true;
        }
        changed |= self.evict_idle(pipe, frame);
        changed
    }

    /// Free least-recently-shown tiles past [`MAX_IDLE_TILES`].
    fn evict_idle(&mut self, pipe: &mut ImagePipeline, frame: u64) -> bool {
        let mut idle: Vec<(u64, RichBlockId, u32)> = self
            .entries
            .iter()
            .flat_map(|(id, e)| {
                e.tiles
                    .iter()
                    .filter(|(_, t)| t.last_frame != frame)
                    .map(|(n, t)| (t.last_frame, *id, *n))
            })
            .collect();
        let mut changed = false;
        if idle.len() > MAX_IDLE_TILES {
            idle.sort_unstable();
            for &(_, id, n) in &idle[..idle.len() - MAX_IDLE_TILES] {
                if let Some(e) = self.entries.get_mut(&id)
                    && let Some(t) = e.tiles.remove(&n)
                {
                    pipe.free_texture(t.texture_index);
                    changed = true;
                }
            }
        }
        self.entries.retain(|_, e| !e.tiles.is_empty());
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

/// Paint one tile's pixels (exactly `width * h * 4` bytes) or `None`.
fn paint_tile(img: &RichImage, y0: u32, h: u32) -> Option<Vec<u8>> {
    let bytes = img.paint(y0, h)?;
    (bytes.len() == img.width as usize * h as usize * 4).then_some(bytes)
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
            key: 0,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(height: u32, cell_h: u32, tile_rows: u32, tiles: &[u32]) -> RichTexEntry {
        let mut e = RichTexEntry {
            revision: 0,
            width: 80,
            height,
            cell_h,
            tile_rows,
            tiles: HashMap::new(),
        };
        for &t in tiles {
            let (y0, h) = e.tile_span(t).unwrap();
            e.tiles.insert(
                t,
                RichTile {
                    texture_index: t as usize + 10,
                    y0,
                    height: h,
                    last_frame: 0,
                },
            );
        }
        e
    }

    #[test]
    fn tile_rows_respect_the_texture_limit() {
        assert_eq!(tile_rows(16, 8192), TILE_ROWS);
        assert_eq!(tile_rows(16, 64), 4);
        assert_eq!(tile_rows(0, 64), 0);
        assert_eq!(tile_rows(32, 16), 0);
    }

    #[test]
    fn tile_spans_cover_the_image_on_row_boundaries() {
        let e = entry(20 * 37, 20, 16, &[]);
        assert_eq!(e.tile_span(0), Some((0, 320)));
        assert_eq!(e.tile_span(1), Some((320, 320)));
        assert_eq!(e.tile_span(2), Some((640, 100)));
        assert_eq!(e.tile_span(3), None);
    }

    #[test]
    fn row_slice_maps_rows_to_tiles_and_uvs() {
        let e = entry(64, 16, 2, &[0, 1]);
        assert_eq!(row_slice(&e, 0), Some((10, [0.0, 0.0], [1.0, 0.5])));
        assert_eq!(row_slice(&e, 1), Some((10, [0.0, 0.5], [1.0, 1.0])));
        assert_eq!(row_slice(&e, 2), Some((11, [0.0, 0.0], [1.0, 0.5])));
        assert_eq!(row_slice(&e, 3), Some((11, [0.0, 0.5], [1.0, 1.0])));
        assert_eq!(row_slice(&e, 4), None);
        // Tile not painted yet → no slice (the row's text draws).
        let e = entry(64, 16, 2, &[1]);
        assert_eq!(row_slice(&e, 0), None);
        assert!(row_slice(&e, 2).is_some());
    }

    #[test]
    fn instances_only_for_resident_blocks() {
        let mut cache = RichTextureCache::default();
        cache.entries.insert(7, entry(32, 16, 2, &[0]));
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
                image: RichImage::from_pixels(w, h, vec![self.0; (w * h * 4) as usize]),
                text: Vec::new(),
                links: Vec::new(),
            })
        }
    }

    fn term_with_block(lines: usize) -> Term {
        term_with_block_in(Term::new(40, 4, 0), lines)
    }

    fn term_with_block_in(mut t: Term, lines: usize) -> Term {
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

    fn tags(id: RichBlockId, rows: std::ops::Range<u16>) -> Vec<RichRow> {
        rows.map(|index| RichRow { block: id, index }).collect()
    }

    #[test]
    fn gpu_sync_paints_only_visible_tiles_and_reuploads_on_revision() {
        let Some((device, queue)) = crate::test_gpu::try_device_and_queue() else {
            return; // no GPU in this sandbox
        };
        let mut pipe = ImagePipeline::new(&device, wgpu::TextureFormat::Rgba8UnormSrgb);
        let mut t = term_with_block(10);
        let id = t.row(0).rich.unwrap().block;
        let mut cache = RichTextureCache::default();
        // Cell 16 px, max_dim 64 → 4 rows per tile. Rows 0..3 visible →
        // only tile 0 is painted.
        assert!(cache.sync(&device, &queue, &mut pipe, &t, &tags(id, 0..3), 16, 64));
        let e = cache.get(id).unwrap().clone();
        assert_eq!(e.tile_rows, 4);
        assert_eq!(e.tiles.len(), 1);
        assert!(row_slice(&e, 2).is_some());
        assert!(row_slice(&e, 9).is_none());
        // Nothing changed → no upload.
        assert!(!cache.sync(&device, &queue, &mut pipe, &t, &tags(id, 0..3), 16, 64));
        // Scrolled to the end: the last (short) tile gets painted.
        assert!(cache.sync(&device, &queue, &mut pipe, &t, &tags(id, 8..10), 16, 64));
        let e = cache.get(id).unwrap();
        assert_eq!(e.tiles[&2].height, 32);
        assert_eq!(
            row_slice(e, 9).map(|s| s.0),
            Some(e.tiles[&2].texture_index)
        );
        // Palette change → new image revision → re-upload.
        let rev = e.revision;
        let mut env = t.rich_env();
        env.palette.fg = [9, 9, 9];
        t.set_rich_env(env);
        t.sync_rich_blocks();
        assert!(cache.sync(&device, &queue, &mut pipe, &t, &tags(id, 0..3), 16, 64));
        assert_ne!(cache.get(id).unwrap().revision, rev);
        assert_eq!(cache.tile_count(), 1, "old tiles freed");
        // Block gone → textures freed.
        let mut p = toastty_parser::Parser::new();
        p.advance(&mut t, b"\x1b[2J");
        t.sync_rich_blocks();
        assert!(cache.sync(&device, &queue, &mut pipe, &t, &[], 16, 64));
        assert!(cache.is_empty());
    }

    #[test]
    fn gpu_idle_tiles_are_bounded() {
        let Some((device, queue)) = crate::test_gpu::try_device_and_queue() else {
            return;
        };
        let mut pipe = ImagePipeline::new(&device, wgpu::TextureFormat::Rgba8UnormSrgb);
        // 300 rows at 2 rows per tile (max_dim 32) → 150 possible tiles.
        let t = term_with_block_in(Term::new(40, 4, 1000), 300);
        let id = t.rich_blocks().ids().next().expect("block stored");
        assert_eq!(t.rich_blocks().get(id).unwrap().rows, 300);
        let mut cache = RichTextureCache::default();
        for r in 0..200u16 {
            cache.sync(&device, &queue, &mut pipe, &t, &tags(id, r..r + 1), 16, 32);
        }
        assert!(cache.tile_count() <= MAX_IDLE_TILES + 1);
        // The most recently shown rows are the ones kept.
        let e = cache.get(id).unwrap();
        assert_eq!(e.tile_rows, 2);
        assert!(e.tiles.contains_key(&99) && e.tiles.contains_key(&75));
        assert!(!e.tiles.contains_key(&0));
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
        cache.sync(&device, &queue, &mut pipe, &t, &tags(id, 0..1), 16, 16);
        assert!(cache.get(id).is_none());
    }
}
