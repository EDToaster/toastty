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
use wgpu::{Device, Extent3d, Queue, Texture, TextureFormat};

use crate::text::atlas::{Atlas, AtlasLayer, AtlasSlot, GlyphKey};
use crate::text::cluster_width::{snap_cluster_widths, GlyphPos};
use crate::text::instance::GlyphSlot;

/// Mask atlas dims (R8) and color atlas dims (BGRA8) — generously sized
/// per the M4b "panic when full" policy.
pub const ATLAS_W: u32 = 1024;
pub const ATLAS_H: u32 = 1024;

struct PendingGlyph {
    glyph: LayoutGlyph,
    col: u16,
    ch: char,
}

/// Result of laying out a single line: each `(char, glyph_slot)` pair the
/// caller can plug into `build_instances`.
#[derive(Debug, Clone, Default)]
pub struct LineGlyphs {
    /// Map from (column, char) → atlas slot. The column is post-snap,
    /// so we can plug it straight into the cell grid.
    pub by_column: HashMap<(u16, char), GlyphSlot>,
}

/// Cached layout per (text, style fingerprint). M4b doesn't actually
/// cache rows — every render shapes fresh. That's fine for the demo;
/// the dirty-set optimization is in M5.
#[derive(Debug)]
pub struct GlyphRasterizer {
    font_system: FontSystem,
    swash_cache: SwashCache,
    atlas: Atlas,
    /// GPU texture for monochrome (R8) glyphs.
    mask_texture: Texture,
    /// GPU texture for color (BGRA8) glyphs.
    color_texture: Texture,
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
}

impl GlyphRasterizer {
    /// Build a rasterizer with `font_size` in pixels and an optional
    /// font name (falls back to monospace if absent or unloaded).
    ///
    /// Also bundles a "fallback" TTF as a guaranteed-present monospace
    /// face — see `Renderer::with_font`.
    pub fn new(
        device: &Device,
        font_size: f32,
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

        let metrics = Metrics::new(font_size, font_size * 1.25);

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
        let mask_texture = create_atlas_texture(device, TextureFormat::R8Unorm, "toastty-mask-atlas");
        let color_texture =
            create_atlas_texture(device, TextureFormat::Bgra8Unorm, "toastty-color-atlas");

        let family_name = font_name.unwrap_or("monospace").to_string();

        Self {
            font_system,
            swash_cache,
            atlas,
            mask_texture,
            color_texture,
            buffer,
            metrics,
            cell_size,
            family_name,
        }
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
    pub fn shape_line(&mut self, queue: &Queue, text: &str) -> LineGlyphs {
        let family_name = self.family_name.clone();
        let attrs = Attrs::new().family(Family::Name(&family_name));
        self.buffer.set_text(text, &attrs, Shaping::Advanced, None);
        self.buffer.shape_until_scroll(&mut self.font_system, false);

        let cell_w = self.cell_size.0;

        let mut pending: Vec<PendingGlyph> = Vec::new();

        for run in self.buffer.layout_runs() {
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
            let snapped = snap_cluster_widths(&positions, cell_w, 1);
            for (g, snap) in run.glyphs.iter().zip(snapped.iter()) {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let col = (snap.x / cell_w).round() as u16;
                let ch = run.text[g.start..g.end].chars().next().unwrap_or(' ');
                pending.push(PendingGlyph {
                    glyph: g.clone(),
                    col,
                    ch,
                });
            }
        }

        let mut out = LineGlyphs::default();
        for p in &pending {
            if let Some(slot) = self.ensure_atlas_slot(queue, &p.glyph) {
                out.by_column.insert((p.col, p.ch), slot);
            }
        }

        out
    }

    /// Rasterize `glyph` if not already in the atlas; return its slot
    /// translated into `GlyphSlot` form for `build_instances`.
    fn ensure_atlas_slot(&mut self, queue: &Queue, glyph: &LayoutGlyph) -> Option<GlyphSlot> {
        let physical = glyph.physical((0.0, 0.0), 1.0);
        let cache_key: CacheKey = physical.cache_key;

        // Compose a stable u64 key for our Atlas. Combine font_id +
        // glyph_id + integer-quantised size.
        let key = GlyphKey(stable_key(cache_key));

        // Atlas slot already? Fast path.
        if let Some(existing) = self.atlas.lookup(key) {
            return Some(atlas_slot_to_glyph_slot(existing, self.atlas.dimensions()));
        }

        let image = self
            .swash_cache
            .get_image_uncached(&mut self.font_system, cache_key)?;

        let w = image.placement.width;
        let h = image.placement.height;
        if w == 0 || h == 0 || image.data.is_empty() {
            // Whitespace / zero-extent: cache an empty slot so we don't
            // re-rasterize next time.
            let slot = self.atlas.reserve(key, AtlasLayer::Mask, 1, 1)?;
            // Don't upload anything.
            return Some(atlas_slot_to_glyph_slot(slot, self.atlas.dimensions()));
        }

        let layer = match image.content {
            SwashContent::Color => AtlasLayer::Color,
            // Mask + SubpixelMask both rasterize to alpha-only data; we
            // treat them identically in M4b.
            SwashContent::Mask | SwashContent::SubpixelMask => AtlasLayer::Mask,
        };

        let slot = self.atlas.reserve(key, layer, w, h).expect(
            "atlas full — M4b's policy is panic; allocate larger atlases or implement eviction",
        );

        upload_glyph_pixels(queue, self.atlas_texture_for(layer), slot, &image.data);

        Some(atlas_slot_to_glyph_slot(slot, self.atlas.dimensions()))
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
fn atlas_slot_to_glyph_slot(slot: AtlasSlot, _atlas_dims: (u32, u32)) -> GlyphSlot {
    GlyphSlot {
        uv_min: [slot.x as f32, slot.y as f32],
        uv_max: [(slot.x + slot.w) as f32, (slot.y + slot.h) as f32],
        is_color: matches!(slot.layer, AtlasLayer::Color),
    }
}

fn measure_cell(font_system: &mut FontSystem, metrics: Metrics, family: Option<&str>) -> (f32, f32) {
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
