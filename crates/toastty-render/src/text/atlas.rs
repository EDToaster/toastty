//! Pure shelf-packer + glyph-key cache for the text atlas.
//!
//! Two virtual atlas layers per decision §3:
//! - [`AtlasLayer::Mask`]: monochrome glyphs (R8 in the GPU layer).
//! - [`AtlasLayer::Color`]: emoji and color-bitmap glyphs (BGRA8).
//!
//! Keeping emoji and text in separate atlases prevents emoji churn from
//! evicting text glyphs (a glyphon trick we stole on purpose).
//!
//! This module is pure CPU. The wgpu textures themselves live in
//! `glyph_rasterizer` — this just owns the packing math + a key→slot
//! cache. That separation keeps the unit tests GPU-free.

use std::collections::HashMap;

/// Which of the two virtual atlas layers a slot lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AtlasLayer {
    /// 8-bit alpha; for monochrome glyph masks.
    Mask,
    /// 32-bit BGRA8; for color/emoji glyphs.
    Color,
}

/// A reserved rectangle in one of the two atlas layers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AtlasSlot {
    pub layer: AtlasLayer,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Opaque key for cached glyphs.
///
/// Wraps a `u64` so callers can compose font id, glyph id, subpixel bin,
/// size, etc. without this module knowing about cosmic-text specifics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlyphKey(pub u64);

/// One shelf in the shelf packer. A shelf is a horizontal strip of fixed
/// height; new rectangles are placed left-to-right within the shelf.
#[derive(Debug, Clone, Copy)]
struct Shelf {
    /// Y position of the shelf's top edge.
    top: u32,
    /// Height of the shelf — the tallest glyph it has accepted.
    height: u32,
    /// X cursor where the next glyph would land.
    cursor_x: u32,
}

/// A bounded shelf-pack allocator. Pure CPU, deterministic.
#[derive(Debug, Clone)]
pub(crate) struct ShelfPacker {
    width: u32,
    height: u32,
    shelves: Vec<Shelf>,
    /// Y of the next shelf's top edge if we have to create a new one.
    next_shelf_top: u32,
}

impl ShelfPacker {
    fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            shelves: Vec::new(),
            next_shelf_top: 0,
        }
    }

    /// Try to reserve a `(w, h)` rectangle. Returns `None` if no shelf can
    /// fit it and we're out of vertical room for a new shelf.
    fn reserve(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        if w == 0 || h == 0 || w > self.width {
            return None;
        }

        // First-fit: walk existing shelves looking for one with room.
        // Allow up to 25% slack on the shelf height to avoid creating a
        // tall, near-empty shelf for one outlier.
        for shelf in &mut self.shelves {
            let height_ok = h <= shelf.height
                || (h <= shelf.height + shelf.height / 4 && shelf.cursor_x == 0);
            if !height_ok {
                continue;
            }
            if shelf.cursor_x + w <= self.width {
                let x = shelf.cursor_x;
                shelf.cursor_x += w;
                if h > shelf.height {
                    shelf.height = h;
                }
                return Some((x, shelf.top));
            }
        }

        // No fit — open a new shelf if there's vertical room.
        if self.next_shelf_top + h > self.height {
            return None;
        }
        let top = self.next_shelf_top;
        let new_shelf = Shelf {
            top,
            height: h,
            cursor_x: w,
        };
        self.shelves.push(new_shelf);
        self.next_shelf_top += h;
        Some((0, top))
    }
}

/// The atlas: two virtual layers plus a key cache.
///
/// `reserve` first checks the cache; on miss it packs into the requested
/// layer. The cache makes repeat calls with the same key idempotent —
/// critical because the renderer would otherwise re-rasterize the same
/// glyph every frame.
///
/// # M4b limitations
///
/// - **Eviction:** none. If a layer fills up, `reserve` returns `None`
///   and the higher-level caller panics. Allocate generously (e.g.
///   1024×1024 per layer) and the demo won't hit it.
///   TODO(atlas-evict): implement LRU shelf reset once we have a real
///   workload to size against.
#[derive(Debug, Clone)]
pub struct Atlas {
    mask: ShelfPacker,
    color: ShelfPacker,
    cache: HashMap<GlyphKey, AtlasSlot>,
}

impl Atlas {
    /// Create an atlas with `(width, height)` per layer (same size in
    /// both layers; pick generously).
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            mask: ShelfPacker::new(width, height),
            color: ShelfPacker::new(width, height),
            cache: HashMap::new(),
        }
    }

    /// Dimensions per layer.
    #[must_use]
    pub fn dimensions(&self) -> (u32, u32) {
        (self.mask.width, self.mask.height)
    }

    /// Reserve space for a glyph. Returns the existing slot if the key
    /// has been seen, else packs into `layer` and caches the result.
    ///
    /// Returns `None` only when the target layer is full.
    pub fn reserve(
        &mut self,
        key: GlyphKey,
        layer: AtlasLayer,
        w: u32,
        h: u32,
    ) -> Option<AtlasSlot> {
        if let Some(slot) = self.cache.get(&key) {
            return Some(*slot);
        }
        let packer = match layer {
            AtlasLayer::Mask => &mut self.mask,
            AtlasLayer::Color => &mut self.color,
        };
        let (x, y) = packer.reserve(w, h)?;
        let slot = AtlasSlot { layer, x, y, w, h };
        self.cache.insert(key, slot);
        Some(slot)
    }

    /// Returns the slot for `key` if it has been reserved.
    #[must_use]
    pub fn lookup(&self, key: GlyphKey) -> Option<AtlasSlot> {
        self.cache.get(&key).copied()
    }

    /// Number of cached glyph keys (sum across layers).
    #[must_use]
    pub fn cached_glyph_count(&self) -> usize {
        self.cache.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect_overlaps(a: AtlasSlot, b: AtlasSlot) -> bool {
        if a.layer != b.layer {
            return false;
        }
        let ax2 = a.x + a.w;
        let ay2 = a.y + a.h;
        let bx2 = b.x + b.w;
        let by2 = b.y + b.h;
        !(ax2 <= b.x || bx2 <= a.x || ay2 <= b.y || by2 <= a.y)
    }

    #[test]
    fn fresh_atlas_has_expected_dims() {
        let a = Atlas::new(64, 64);
        assert_eq!(a.dimensions(), (64, 64));
        assert_eq!(a.cached_glyph_count(), 0);
    }

    #[test]
    fn reserve_returns_some_for_typical_glyph() {
        let mut a = Atlas::new(64, 64);
        let slot = a.reserve(GlyphKey(1), AtlasLayer::Mask, 8, 14).unwrap();
        assert_eq!(slot.layer, AtlasLayer::Mask);
        assert_eq!(slot.w, 8);
        assert_eq!(slot.h, 14);
    }

    #[test]
    fn duplicate_keys_return_identical_slot() {
        let mut a = Atlas::new(64, 64);
        let first = a.reserve(GlyphKey(42), AtlasLayer::Mask, 8, 14).unwrap();
        let second = a.reserve(GlyphKey(42), AtlasLayer::Mask, 8, 14).unwrap();
        assert_eq!(first, second);
        // Cache should hold exactly one entry.
        assert_eq!(a.cached_glyph_count(), 1);
    }

    #[test]
    fn distinct_keys_get_distinct_slots() {
        let mut a = Atlas::new(64, 64);
        let s1 = a.reserve(GlyphKey(1), AtlasLayer::Mask, 8, 14).unwrap();
        let s2 = a.reserve(GlyphKey(2), AtlasLayer::Mask, 8, 14).unwrap();
        assert_ne!(s1, s2);
        assert!(!rect_overlaps(s1, s2));
    }

    #[test]
    fn lookup_returns_cached_slot_only_after_reserve() {
        let mut a = Atlas::new(32, 32);
        let key = GlyphKey(7);
        assert!(a.lookup(key).is_none());
        let slot = a.reserve(key, AtlasLayer::Mask, 4, 4).unwrap();
        assert_eq!(a.lookup(key), Some(slot));
    }

    #[test]
    fn many_packs_never_overlap() {
        // Pack a bunch of small rects; check no two overlap.
        let mut a = Atlas::new(64, 64);
        let mut slots = Vec::new();
        for i in 0..50_u64 {
            let slot = a.reserve(GlyphKey(i), AtlasLayer::Mask, 8, 8).unwrap();
            for prev in &slots {
                assert!(
                    !rect_overlaps(slot, *prev),
                    "slots overlap: {slot:?} and {prev:?}",
                );
            }
            slots.push(slot);
        }
    }

    #[test]
    fn color_and_mask_layers_are_independent() {
        // A slot in `Color` should not overlap a same-coords slot in
        // `Mask`, because `rect_overlaps` already treats different layers
        // as non-overlapping — but more importantly, the two packers
        // shouldn't share state.
        let mut a = Atlas::new(64, 64);
        let m = a.reserve(GlyphKey(1), AtlasLayer::Mask, 64, 60).unwrap();
        let c = a.reserve(GlyphKey(2), AtlasLayer::Color, 8, 8).unwrap();
        assert_eq!(m.layer, AtlasLayer::Mask);
        assert_eq!(c.layer, AtlasLayer::Color);
    }

    #[test]
    fn exhaustion_returns_none_no_panic() {
        // 8x8 atlas can fit at most one 6x6 glyph; the second 6x6 should
        // fail (different key so cache won't hit).
        let mut a = Atlas::new(8, 8);
        assert!(a.reserve(GlyphKey(1), AtlasLayer::Mask, 6, 6).is_some());
        // Second glyph won't fit in the remaining 2 wide x 8 tall, and
        // there's no vertical room for a new shelf.
        assert!(a.reserve(GlyphKey(2), AtlasLayer::Mask, 6, 6).is_none());
    }

    #[test]
    fn rejects_zero_dim_request() {
        let mut a = Atlas::new(64, 64);
        assert!(a.reserve(GlyphKey(1), AtlasLayer::Mask, 0, 8).is_none());
        assert!(a.reserve(GlyphKey(2), AtlasLayer::Mask, 8, 0).is_none());
    }

    #[test]
    fn rejects_oversize_request() {
        let mut a = Atlas::new(16, 16);
        // Wider than atlas — must fail without panicking.
        assert!(a.reserve(GlyphKey(1), AtlasLayer::Mask, 32, 8).is_none());
    }

    #[test]
    fn opens_new_shelves_as_glyphs_get_taller() {
        // First shelf 8 high; second 12 high; need both.
        let mut a = Atlas::new(64, 24);
        let s1 = a.reserve(GlyphKey(1), AtlasLayer::Mask, 8, 8).unwrap();
        let s2 = a.reserve(GlyphKey(2), AtlasLayer::Mask, 8, 12).unwrap();
        assert_eq!(s1.y, 0);
        // The second glyph either fits in shelf 1 (if slack allows) or
        // opens a new shelf at y=8.
        assert!(s2.y == 0 || s2.y == 8);
        assert!(!rect_overlaps(s1, s2));
    }

    #[test]
    fn fills_first_shelf_horizontally_before_opening_second() {
        // 64-wide atlas, 16 high — should pack four 16x8 glyphs across
        // the first shelf at y=0 before opening a second one.
        let mut a = Atlas::new(64, 16);
        let mut prev_x = 0;
        for i in 0..4_u64 {
            let slot = a.reserve(GlyphKey(i), AtlasLayer::Mask, 16, 8).unwrap();
            assert_eq!(slot.y, 0, "glyph {i} should be on shelf 1");
            assert_eq!(slot.x, prev_x);
            prev_x += 16;
        }
        // Fifth opens a second shelf.
        let slot = a.reserve(GlyphKey(99), AtlasLayer::Mask, 16, 8).unwrap();
        assert_eq!(slot.y, 8);
    }

    #[test]
    fn empty_atlas_lookup_is_none() {
        let a = Atlas::new(32, 32);
        assert_eq!(a.lookup(GlyphKey(123)), None);
    }

    #[test]
    fn color_layer_independently_exhausts() {
        // Filling the mask layer to capacity must not prevent the color
        // layer from accepting fresh glyphs.
        let mut a = Atlas::new(8, 8);
        assert!(a.reserve(GlyphKey(1), AtlasLayer::Mask, 6, 6).is_some());
        // Mask layer exhausted (proved by previous test).
        assert!(a.reserve(GlyphKey(2), AtlasLayer::Color, 6, 6).is_some());
    }
}
