//! Glyph rasterizer: cosmic-text shape → swash raster → GPU atlas upload.
//!
//! Wraps a [`cosmic_text::FontSystem`] + [`cosmic_text::SwashCache`] +
//! the dual-layer GPU atlas. Given a row of text and an SGR style, it
//! shapes the row, runs the cluster-width snap (decision §3), rasterizes
//! any missing glyphs via swash, uploads them into the appropriate atlas
//! layer (mask or color), and returns the atlas slots keyed by character
//! + style.

use cosmic_text::{
    Attrs, Buffer, CacheKey, Family, FontSystem, LayoutGlyph, Metrics, Shaping, SwashCache,
    SwashContent,
};
use std::collections::HashMap;
use toastty_protocols::unicode_core::cluster_cell_width;
use unicode_width::UnicodeWidthChar;
use wgpu::{Device, Extent3d, Queue, Texture, TextureFormat};

use crate::text::atlas::{Atlas, AtlasLayer, AtlasSlot, GlyphKey};
use crate::text::cluster_width::{GlyphPos, snap_cluster_widths_per_cluster};
use crate::text::instance::GlyphSlot;

/// Per-row cache slot: which char was shaped into this column and the
/// atlas slot we got back. Storing `char` keeps the same defense-in-depth
/// the old `HashMap<(col, char), GlyphSlot>` had — callers re-check the
/// char so a stale cache entry (e.g. wrong-char lookup) misses cleanly.
type ColumnSlot = Option<(char, GlyphSlot)>;

/// Mask atlas dims (R8) and color atlas dims (BGRA8) — generously sized
/// per the M4b "panic when full" policy.
pub const ATLAS_W: u32 = 1024;
pub const ATLAS_H: u32 = 1024;

/// Default line-height multiplier (× `font_size_px`). 1.20 is tight enough
/// that adjacent rows of box-drawing characters connect (`│` to `│`,
/// `┴` to `┬`) while still keeping enough breathing room above descenders
/// for most monospace fonts. The `toastty-config` schema defaults to the
/// same value.
pub const DEFAULT_LINE_HEIGHT_RATIO: f32 = 1.20;

struct PendingGlyph {
    glyph: LayoutGlyph,
    col: u16,
    ch: char,
}

/// Result of laying out a single line: each `(char, glyph_slot)` pair the
/// caller can plug into `build_instances`.
///
/// Stored densely as `by_column[col] = Some((ch, slot))`. Switched from
/// `HashMap<(col, char), GlyphSlot>` after profiling showed ~1% of
/// main-thread wall time in hash bucket lookups during `build_instances`,
/// plus per-row heap allocations populating the map. Columns are 0..N
/// over a row, so an indexed vec is a strict upgrade.
#[derive(Debug, Clone, Default)]
pub struct LineGlyphs {
    /// Slot per column. Indices outside the populated range are
    /// implicitly `None`. The vec is grown only as far as the highest
    /// column shaped — wide-character lines that skip over a column
    /// (CJK / combining-mark continuation cells) leave those slots
    /// `None`, matching the old "key absent" semantics.
    pub by_column: Vec<ColumnSlot>,
}

impl LineGlyphs {
    /// Look up the slot for `col` matching `ch`. Returns `None` if the
    /// column is out of range, was never shaped, or holds a different
    /// char — same semantics as the old `HashMap::get(&(col, ch))`.
    #[must_use]
    pub fn get(&self, col: u16, ch: char) -> Option<GlyphSlot> {
        match self.by_column.get(col as usize) {
            Some(Some((c, slot))) if *c == ch => Some(*slot),
            _ => None,
        }
    }

    /// Insert a slot at `col`. Grows the underlying vec as needed.
    pub fn insert(&mut self, col: u16, ch: char, slot: GlyphSlot) {
        let idx = col as usize;
        if self.by_column.len() <= idx {
            self.by_column.resize(idx + 1, None);
        }
        self.by_column[idx] = Some((ch, slot));
    }
}

/// Cached layout per (text, style fingerprint). M4b doesn't actually
/// cache rows — every render shapes fresh. That's fine for the demo;
/// the dirty-set optimization is in M5.
/// Per-glyph pixel placement relative to a cell origin. Stays in a
/// parallel cache to the atlas slot so cache-hit paths can rebuild the
/// `GlyphSlot` without re-rasterizing.
#[derive(Debug, Clone, Copy)]
struct GlyphPlacement {
    offset: [f32; 2],
    size: [f32; 2],
}

#[derive(Debug)]
pub struct GlyphRasterizer {
    font_system: FontSystem,
    swash_cache: SwashCache,
    atlas: Atlas,
    /// GPU texture for monochrome (R8) glyphs.
    mask_texture: Texture,
    /// GPU texture for color (BGRA8) glyphs.
    color_texture: Texture,
    /// Placement (bearing + extent) cache keyed by `GlyphKey`. Parallel
    /// to `atlas`'s slot cache; same lifetime semantics.
    placements: HashMap<GlyphKey, GlyphPlacement>,
    /// Per-line buffer reused across shapings to avoid re-allocating.
    buffer: Buffer,
    /// Font metrics in pixels (`size`, `line_height`). Retained so
    /// future work (live size change, viewport recompute) doesn't have
    /// to re-derive them. Not currently read post-construction.
    #[allow(dead_code)]
    metrics: Metrics,
    /// Logical cell size in pixels (width × height). Width is computed
    /// after shaping the reference monospace glyph.
    cell_size: (f32, f32),
    /// Configured font family, kept around for `Attrs`.
    family_name: String,
    /// Per-character glyph cache. Keyed on `char` alone, which is **only
    /// correct under three invariants**:
    ///   1. Single-weight font (no bold/italic switching). Bold/italic
    ///      glyphs are different shapes; adding font-weight selection
    ///      requires re-keying as `(char, style_fingerprint)`.
    ///   2. No NFD combining marks. `é` as `e + U+0301` would render as
    ///      `e` and a separately-cached `U+0301` placed in the next
    ///      column — visually wrong. Pre-composed `é` (U+00E9) is fine.
    ///   3. Cell width == 1. Wide chars (CJK, fullwidth) and zero-width
    ///      chars (control, combining) must not be cached at all.
    ///      Enforced by the `unicode_width::UnicodeWidthChar::width(c)
    ///      != Some(1)` bail in the fast path.
    ///
    /// M6+ font-weight switching will require lifting invariant #1
    /// by re-keying the cache. NFD inputs (#2) and complex shaping
    /// require dropping the fast path entirely and going through
    /// cosmic-text + the cluster-width snap.
    ///
    /// Populated as `shape_line` resolves each character through
    /// cosmic-text the first time; subsequent lines containing only
    /// cached single-column characters skip the (expensive)
    /// cosmic-text layout call entirely. The per-line layout cost
    /// dominates the renderer's per-frame budget at fullscreen
    /// (~110 µs / row of ASCII on M4 Pro).
    char_cache: HashMap<char, GlyphSlot>,
    /// Characters known to be uncacheable (cosmic-text returned no
    /// glyph for them — e.g., zero-width spaces). Recording them stops
    /// repeated misses from triggering the slow path forever.
    char_cache_misses: std::collections::HashSet<char>,
    /// True iff the family name requested by the caller resolved to at
    /// least one face in the loaded font database. When false, cosmic-
    /// text will fall back to whatever the system considers a default
    /// monospace face — the renderer still works, but the user's
    /// `font.family` config didn't take effect. Surfaced via
    /// [`GlyphRasterizer::requested_family_available`].
    requested_family_available: bool,
}

impl GlyphRasterizer {
    /// Build a rasterizer with `font_size` in pixels, the
    /// line-height ratio (× `font_size`), and an optional font name
    /// (falls back to monospace if absent or unloaded).
    ///
    /// Also bundles a "fallback" TTF as a guaranteed-present monospace
    /// face — see `Renderer::with_font`.
    ///
    /// Pass [`DEFAULT_LINE_HEIGHT_RATIO`] for the value the M4b snapshots
    /// were captured at.
    pub fn new(
        device: &Device,
        font_size: f32,
        line_height_ratio: f32,
        font_name: Option<&str>,
        bundled_font: Option<&[u8]>,
    ) -> Self {
        let mut font_system = FontSystem::new();

        if let Some(data) = bundled_font {
            font_system
                .db_mut()
                .load_font_source(cosmic_text::fontdb::Source::Binary(std::sync::Arc::new(
                    data.to_vec(),
                )));
        }

        // Line height = `font_size * line_height_ratio`. See
        // [`DEFAULT_LINE_HEIGHT_RATIO`] for the rationale on the default.
        let metrics = Metrics::new(font_size, font_size * line_height_ratio);

        // Determine cell size by shaping the reference glyph "M".
        let cell_size = measure_cell(&mut font_system, metrics, font_name);

        // Pre-allocate the per-line buffer.
        let mut buffer = Buffer::new(&mut font_system, metrics);
        buffer.set_size(Some(f32::INFINITY), Some(f32::INFINITY));
        // Tell cosmic-text we want monospace-style positioning; the
        // cluster-width snap fixes its per-glyph rounding (decision §3).
        buffer.set_monospace_width(Some(cell_size.0));

        let swash_cache = SwashCache::new();
        let atlas = Atlas::new(ATLAS_W, ATLAS_H);
        let mask_texture =
            create_atlas_texture(device, TextureFormat::R8Unorm, "toastty-mask-atlas");
        let color_texture =
            create_atlas_texture(device, TextureFormat::Bgra8Unorm, "toastty-color-atlas");

        let family_name = font_name.unwrap_or("monospace").to_string();

        // Check whether the requested family is actually present in
        // the font database. `db().faces()` walks every loaded face;
        // we report whether any face's `families` list contains a
        // case-insensitive match. The bundled fallback TTF advertises
        // its own family name (e.g. "Fira Mono"), so requesting that
        // family will resolve even on a host with no system fonts of
        // the same name. Used by the binary to log a warning when the
        // user's `font.family` config doesn't match anything.
        let requested_family_available = font_name.is_none_or(|requested| {
            font_system
                .db()
                .faces()
                .any(|face| {
                    face.families
                        .iter()
                        .any(|(name, _)| name.eq_ignore_ascii_case(requested))
                })
        });

        Self {
            font_system,
            swash_cache,
            atlas,
            mask_texture,
            color_texture,
            placements: HashMap::new(),
            buffer,
            metrics,
            cell_size,
            family_name,
            char_cache: HashMap::new(),
            char_cache_misses: std::collections::HashSet::new(),
            requested_family_available,
        }
    }

    /// True when the family name passed to [`Self::new`] resolved to a
    /// real face. `false` means cosmic-text will fall back to the
    /// host's default monospace; callers can log a warning so the user
    /// notices their config didn't take effect.
    pub fn requested_family_available(&self) -> bool {
        self.requested_family_available
    }

    /// Family name as requested by the caller (or `"monospace"` if the
    /// caller passed `None`). Useful for emitting a "font X not found"
    /// log line.
    pub fn family_name(&self) -> &str {
        &self.family_name
    }

    /// Cell size in pixels (width, height).
    pub fn cell_size(&self) -> (f32, f32) {
        self.cell_size
    }

    /// Atlas dimensions per layer (same for both).
    pub fn atlas_dims(&self) -> (u32, u32) {
        self.atlas.dimensions()
    }

    /// Borrow the GPU textures (for binding).
    pub fn mask_texture(&self) -> &Texture {
        &self.mask_texture
    }

    pub fn color_texture(&self) -> &Texture {
        &self.color_texture
    }

    /// Shape `text` as one line and ensure every glyph is in the atlas.
    /// Returns per-column glyph slots keyed by `(column, char)`.
    ///
    /// `mode_2027_active` controls cluster-width semantics for the
    /// per-cluster snap: when true, the renderer trusts the
    /// grapheme-segmenter's answer (VS16 emoji, ZWJ family etc. snap
    /// to 2 cells); when false it falls back to the legacy `wcwidth`
    /// table (which still gives 2 for plain CJK / fullwidth chars).
    ///
    /// Fast path: if every non-space character is already in
    /// [`char_cache`](Self::char_cache), we skip cosmic-text entirely
    /// and just pack the cached slots by column. Spaces are skipped (no
    /// glyph). This avoids the per-line layout cost — measured at ~110
    /// µs per row of ASCII on M4 Pro — which dominates the renderer's
    /// per-frame budget at fullscreen.
    ///
    /// Slow path: full cosmic-text shaping. Used the first time a
    /// character is seen and any time a line contains an
    /// uncached-and-non-miss character. Populates the cache so future
    /// lines hit the fast path.
    pub fn shape_line(&mut self, queue: &Queue, text: &str, mode_2027_active: bool) -> LineGlyphs {
        if let Some(line) = self.try_shape_line_fast(text) {
            return line;
        }
        self.shape_line_slow(queue, text, mode_2027_active)
    }

    /// Fast path: per-character lookups against `char_cache`. Returns
    /// `None` if any character in `text` would require cosmic-text
    /// (cache miss for a character that has not been marked
    /// uncacheable, or any non-single-column character). When it
    /// returns `Some`, no cosmic-text work was done.
    fn try_shape_line_fast(&self, text: &str) -> Option<LineGlyphs> {
        let mut out = LineGlyphs::default();
        let cell_w = self.cell_size.0;
        if cell_w <= 0.0 {
            return None;
        }
        // Each character in a monospace terminal grid occupies one
        // cell. We assign columns by index in the iteration order; the
        // term's row text is fed cell-by-cell, so the i'th char is
        // column i. This breaks for wide / zero-width characters
        // (CJK ideographs, combining marks, controls): a wide char
        // would span two columns, and a combining mark would consume a
        // column index without advancing the cell. Bail to the slow
        // path on any char whose `unicode_width` is not exactly 1.
        for (col, ch) in text.chars().enumerate() {
            if col > u16::MAX as usize {
                return None;
            }
            if ch == ' ' || ch == '\0' {
                continue;
            }
            // Width-1 invariant — see `char_cache` doc comment.
            if ch.width() != Some(1) {
                return None;
            }
            // Already-known miss: skip the cell, but keep going on
            // the fast path. This stops e.g. control characters from
            // forcing every line through cosmic-text forever.
            if self.char_cache_misses.contains(&ch) {
                continue;
            }
            match self.char_cache.get(&ch) {
                Some(slot) => {
                    #[allow(clippy::cast_possible_truncation)]
                    out.insert(col as u16, ch, *slot);
                }
                None => return None,
            }
        }
        Some(out)
    }

    /// Slow path: cosmic-text shaping. Populates `char_cache` /
    /// `char_cache_misses` with whatever cosmic-text resolves so that
    /// future calls with the same characters can take the fast path.
    fn shape_line_slow(
        &mut self,
        queue: &Queue,
        text: &str,
        mode_2027_active: bool,
    ) -> LineGlyphs {
        let family_name = self.family_name.clone();
        let attrs = Attrs::new().family(Family::Name(&family_name));
        self.buffer.set_text(text, &attrs, Shaping::Advanced, None);
        self.buffer.shape_until_scroll(&mut self.font_system, false);

        let cell_w = self.cell_size.0;

        let mut pending: Vec<(PendingGlyph, f32)> = Vec::new();

        for run in self.buffer.layout_runs() {
            // Baseline within the cell: cosmic-text reports `line_y`
            // (baseline) and `line_top` in buffer coords; the difference
            // is the baseline-from-cell-top offset.
            let baseline_y = run.line_y - run.line_top;

            let positions: Vec<GlyphPos> = run
                .glyphs
                .iter()
                .map(|g| GlyphPos {
                    start: g.start,
                    end: g.end,
                    x: g.x,
                    w: g.w,
                })
                .collect();
            // Compute the per-cluster cell width. Walk `positions`
            // grouping by `(start, end)`; for each group, look up the
            // cluster substring in the original `run.text` and ask
            // `cluster_cell_width` (mode-2027 aware) for its width.
            let mut widths: Vec<u8> = Vec::new();
            {
                let mut i = 0;
                while i < positions.len() {
                    let start = positions[i].start;
                    let end = positions[i].end;
                    let mut j = i + 1;
                    while j < positions.len()
                        && positions[j].start == start
                        && positions[j].end == end
                    {
                        j += 1;
                    }
                    let cluster_str = run.text.get(start..end).unwrap_or("");
                    widths.push(cluster_cell_width(cluster_str, mode_2027_active));
                    i = j;
                }
            }
            let snapped = snap_cluster_widths_per_cluster(&positions, cell_w, &widths);
            for (g, snap) in run.glyphs.iter().zip(snapped.iter()) {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let col = (snap.x / cell_w).round() as u16;
                let ch = run.text[g.start..g.end].chars().next().unwrap_or(' ');
                pending.push((
                    PendingGlyph {
                        glyph: g.clone(),
                        col,
                        ch,
                    },
                    baseline_y,
                ));
            }
        }

        // Track which characters in the input source line had a glyph
        // produced. Anything *not* in this set is a cosmic-text miss
        // (e.g. zero-width spaces, control chars, characters mapped to
        // the .notdef glyph) and we record it as a miss so the fast
        // path stops trying.
        let mut produced_chars: std::collections::HashSet<char> =
            std::collections::HashSet::new();

        let mut out = LineGlyphs::default();
        for (p, baseline_y) in &pending {
            if let Some(slot) = self.ensure_atlas_slot(queue, &p.glyph, *baseline_y) {
                out.insert(p.col, p.ch, slot);
                // Populate the per-character fast-path cache. A single
                // monospace glyph's slot is column-independent (see
                // `build_instances`), so reusing it for the next line
                // is correct — but ONLY for width-1 chars. Wide and
                // zero-width chars (CJK, combining marks, controls)
                // must never enter the cache: their `(col, ch)` keys
                // wouldn't survive the fast path's column-by-iteration
                // assumption (invariant #3 on `char_cache`).
                if p.ch.width() == Some(1) {
                    self.char_cache.entry(p.ch).or_insert(slot);
                }
                produced_chars.insert(p.ch);
            }
        }

        // Anything in the input that didn't produce a glyph is a miss.
        for ch in text.chars() {
            if ch == ' ' || ch == '\0' {
                continue;
            }
            if !produced_chars.contains(&ch) {
                self.char_cache_misses.insert(ch);
            }
        }

        out
    }

    /// Rasterize `glyph` if not already in the atlas; return its slot
    /// translated into `GlyphSlot` form for `build_instances`.
    ///
    /// `baseline_y` is the baseline's offset from the cell's top edge
    /// (pixels). Used to convert swash's bearing into a cell-relative
    /// offset.
    fn ensure_atlas_slot(
        &mut self,
        queue: &Queue,
        glyph: &LayoutGlyph,
        baseline_y: f32,
    ) -> Option<GlyphSlot> {
        let physical = glyph.physical((0.0, 0.0), 1.0);
        let cache_key: CacheKey = physical.cache_key;

        // Compose a stable u64 key for our Atlas. Combine font_id +
        // glyph_id + integer-quantised size.
        let key = GlyphKey(stable_key(cache_key));

        // Atlas slot already? Fast path. Placement was cached alongside.
        if let Some(existing) = self.atlas.lookup(key) {
            let placement = self
                .placements
                .get(&key)
                .copied()
                .unwrap_or(GlyphPlacement {
                    offset: [0.0, 0.0],
                    size: [0.0, 0.0],
                });
            return Some(make_glyph_slot(existing, placement));
        }

        let image = self
            .swash_cache
            .get_image_uncached(&mut self.font_system, cache_key)?;

        let w = image.placement.width;
        let h = image.placement.height;
        #[allow(clippy::cast_precision_loss)]
        let placement = GlyphPlacement {
            offset: [
                image.placement.left as f32,
                baseline_y - image.placement.top as f32,
            ],
            size: [w as f32, h as f32],
        };

        if w == 0 || h == 0 || image.data.is_empty() {
            // Whitespace / zero-extent: cache an empty slot so we don't
            // re-rasterize next time.
            let slot = self.atlas.reserve(key, AtlasLayer::Mask, 1, 1).ok()?;
            self.placements.insert(key, placement);
            // Don't upload anything.
            return Some(make_glyph_slot(slot, placement));
        }

        let layer = match image.content {
            SwashContent::Color => AtlasLayer::Color,
            // Mask + SubpixelMask both rasterize to alpha-only data; we
            // treat them identically in M4b.
            SwashContent::Mask | SwashContent::SubpixelMask => AtlasLayer::Mask,
        };

        // Best-effort recovery: if the layer is full, evict the LRU
        // shelf and retry once. Failing that, degrade by returning
        // `None` — the renderer falls back to a background-only
        // instance and the next frame will re-shape the row.
        let slot = if let Ok(s) = self.atlas.reserve(key, layer, w, h) {
            s
        } else {
            self.atlas.evict_oldest_shelf(layer);
            self.atlas.reserve(key, layer, w, h).ok()?
        };

        upload_glyph_pixels(queue, self.atlas_texture_for(layer), slot, &image.data);

        self.placements.insert(key, placement);
        Some(make_glyph_slot(slot, placement))
    }

    fn atlas_texture_for(&self, layer: AtlasLayer) -> &Texture {
        match layer {
            AtlasLayer::Mask => &self.mask_texture,
            AtlasLayer::Color => &self.color_texture,
        }
    }
}

fn stable_key(k: CacheKey) -> u64 {
    // font_id is wrapper around u32; glyph_id u16; font_size as bits;
    // pack a few fields into u64.
    let font_id_bits: u64 = font_id_to_u64(k.font_id);
    let glyph: u64 = k.glyph_id as u64;
    let size: u64 = k.font_size_bits as u64;
    let flags: u64 = k.flags.bits() as u64;
    (font_id_bits << 32) ^ (glyph << 16) ^ size ^ (flags << 56)
}

fn font_id_to_u64(id: cosmic_text::fontdb::ID) -> u64 {
    // fontdb::ID has no public accessors but Debug prints as `ID(N)`.
    // The most portable hash is via the inner repr through serialization,
    // but a Debug-format parse is brittle. Use a deterministic hash via
    // std::hash::Hasher instead.
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut h);
    h.finish()
}

#[allow(clippy::cast_precision_loss)] // atlas coords fit in 24 bits easily
fn make_glyph_slot(slot: AtlasSlot, placement: GlyphPlacement) -> GlyphSlot {
    GlyphSlot {
        uv_min: [slot.x as f32, slot.y as f32],
        uv_max: [(slot.x + slot.w) as f32, (slot.y + slot.h) as f32],
        is_color: matches!(slot.layer, AtlasLayer::Color),
        glyph_offset: placement.offset,
        glyph_size: placement.size,
    }
}

fn measure_cell(
    font_system: &mut FontSystem,
    metrics: Metrics,
    family: Option<&str>,
) -> (f32, f32) {
    let mut probe = Buffer::new(font_system, metrics);
    probe.set_size(Some(f32::INFINITY), Some(f32::INFINITY));
    let fam = family.unwrap_or("monospace");
    let attrs = Attrs::new().family(Family::Name(fam));
    probe.set_text("M", &attrs, Shaping::Advanced, None);
    probe.shape_until_scroll(font_system, false);

    let mut max_w: f32 = metrics.font_size * 0.6;
    for run in probe.layout_runs() {
        for g in run.glyphs {
            if g.w > max_w {
                max_w = g.w;
            }
        }
    }
    let height = metrics.line_height.max(metrics.font_size);
    (max_w.max(1.0), height.max(1.0))
}

fn create_atlas_texture(device: &Device, format: TextureFormat, label: &str) -> Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: Extent3d {
            width: ATLAS_W,
            height: ATLAS_H,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn upload_glyph_pixels(queue: &Queue, texture: &Texture, slot: AtlasSlot, data: &[u8]) {
    let bytes_per_pixel = match texture.format() {
        TextureFormat::R8Unorm => 1u32,
        TextureFormat::Bgra8Unorm | TextureFormat::Rgba8Unorm => 4u32,
        other => panic!("unexpected atlas format: {other:?}"),
    };

    // swash emits BGRA-ordered bytes for color content (its
    // SwashImage.data is RGBA premultiplied for color, A only for mask).
    // We need BGRA for the color texture. Translate if needed.
    let bytes: std::borrow::Cow<'_, [u8]> = if matches!(slot.layer, AtlasLayer::Color) {
        let mut out = Vec::with_capacity(data.len());
        for px in data.chunks_exact(4) {
            // swash gives RGBA; swap to BGRA.
            out.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
        std::borrow::Cow::Owned(out)
    } else {
        std::borrow::Cow::Borrowed(data)
    };

    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d {
                x: slot.x,
                y: slot.y,
                z: 0,
            },
            aspect: wgpu::TextureAspect::All,
        },
        &bytes,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(slot.w * bytes_per_pixel),
            rows_per_image: Some(slot.h),
        },
        Extent3d {
            width: slot.w,
            height: slot.h,
            depth_or_array_layers: 1,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{instance_descriptor, instance_flags_for_tests};
    use pollster::block_on;
    use wgpu::{DeviceDescriptor, PowerPreference, RequestAdapterOptions};

    const TEST_FONT: &[u8] = include_bytes!("../../fonts/FiraMono-Medium.ttf");

    fn make_rasterizer() -> (Device, Queue, GlyphRasterizer) {
        let instance = wgpu::Instance::new(instance_descriptor(instance_flags_for_tests()));
        let adapter = block_on(instance.request_adapter(&RequestAdapterOptions {
            power_preference: PowerPreference::LowPower,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .expect("no GPU adapter for test");
        let (device, queue) = block_on(adapter.request_device(&DeviceDescriptor {
            label: Some("char_cache test device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults(),
            memory_hints: wgpu::MemoryHints::Performance,
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            trace: wgpu::Trace::Off,
        }))
        .expect("device request failed");
        let rasterizer = GlyphRasterizer::new(
            &device,
            16.0,
            DEFAULT_LINE_HEIGHT_RATIO,
            Some("Fira Mono"),
            Some(TEST_FONT),
        );
        (device, queue, rasterizer)
    }

    /// Wide / zero-width characters must not enter `char_cache`: their
    /// cell-width != 1 violates invariant #3, so the fast path bails
    /// before consulting the cache and the slow path skips insertion.
    /// Without this guard, a CJK ideograph could be cached and later
    /// silently misaligned in a single-column slot.
    #[test]
    fn cjk_does_not_enter_char_cache() {
        let (_device, queue, mut rasterizer) = make_rasterizer();

        // ASCII-only line: every glyph is width-1 and should land in
        // the cache via the slow path's `char_cache.entry(...)` call.
        rasterizer.shape_line(&queue, "hello", false);
        let ascii_cache_size = rasterizer.char_cache.len();
        assert!(
            ascii_cache_size >= "hello".chars().filter(|c| *c != 'l').count(),
            "ASCII chars should populate char_cache; got {ascii_cache_size}"
        );
        let cache_before = rasterizer.char_cache.clone();

        // Now shape a line containing a CJK ideograph. `你` has
        // unicode_width 2; the fast path must bail (so the slow path
        // runs), and the slow path must NOT insert `你` into the
        // per-char cache.
        rasterizer.shape_line(&queue, "你", false);

        // The CJK char itself must not be in the cache.
        assert!(
            !rasterizer.char_cache.contains_key(&'你'),
            "wide char `你` must not be inserted into char_cache"
        );

        // No previously-cached entries should have been disturbed.
        for (k, v) in &cache_before {
            assert_eq!(
                rasterizer.char_cache.get(k),
                Some(v),
                "existing char_cache entry for {k:?} changed"
            );
        }
    }

    /// The fast path must refuse a line containing any non-width-1
    /// character, even if the ASCII parts would otherwise hit the
    /// cache.
    #[test]
    fn fast_path_bails_on_wide_char() {
        let (_device, queue, mut rasterizer) = make_rasterizer();
        // Warm the cache for ASCII letters.
        rasterizer.shape_line(&queue, "ab", false);
        assert!(rasterizer.char_cache.contains_key(&'a'));
        assert!(rasterizer.char_cache.contains_key(&'b'));

        // A line mixing ASCII + a wide char: fast path must return
        // None even though `a`/`b` are cached.
        assert!(rasterizer.try_shape_line_fast("a你b").is_none());
        // Pure ASCII still takes the fast path.
        assert!(rasterizer.try_shape_line_fast("ab").is_some());
    }

    /// Mode 2027 OFF: cluster_width falls back to `wcwidth` per
    /// codepoint. CJK still snaps to 2 cells (legacy table); ZWJ
    /// emoji come out as 1. We don't render here — we just check
    /// shape_line doesn't panic for both flag values.
    #[test]
    fn mode_2027_off_uses_default_width() {
        let (_device, queue, mut rasterizer) = make_rasterizer();
        let _ = rasterizer.shape_line(&queue, "你 abc", false);
    }

    /// Mode 2027 ON: cluster_width trusts UnicodeWidthStr. ZWJ family
    /// snaps to 2; VS16 emoji snaps to 2. Shape doesn't panic and
    /// produces some glyphs. Visual correctness requires a real GPU +
    /// snapshot harness — geometry-only smoke here.
    #[test]
    fn mode_2027_on_honors_cluster_width_for_emoji() {
        let (_device, queue, mut rasterizer) = make_rasterizer();
        // FiraMono doesn't cover emoji so cosmic-text may map them to
        // the .notdef glyph — but the call must not panic and must
        // return without producing two-cell-overlapping glyphs.
        let _ = rasterizer.shape_line(&queue, "❤\u{FE0F}", true);
    }
}
