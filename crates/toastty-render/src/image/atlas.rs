//! GPU mirror of [`ImageRegistry`]: per-image texture cache.
//!
//! Architecture choice (M11a, pre-approved): texture-per-image, NOT a
//! packed atlas. Each `Texture` is sized for its source image; no
//! shelf packing. Each image draw binds exactly one texture, so the
//! number of resident textures is not tied to any per-stage binding
//! limit.
//!
//! Residency is demand-driven (SIZE-8): an image is uploaded the first
//! frame one of its placements is drawn, never just because it was
//! transmitted. GPU memory is bounded by a byte budget
//! ([`ImageTextureCache::DEFAULT_BUDGET_BYTES`]); when the resident set
//! exceeds it, the textures drawn least recently are dropped. Textures
//! drawn in the current frame are never evicted, so a working set larger
//! than the budget still renders in full (memory then tracks the
//! working set, not the budget), and nothing is re-uploaded per frame.
//!
//! [`ImageRegistry`]: toastty_term::ImageRegistry

use std::collections::HashMap;

/// Bookkeeping for one image's GPU texture.
#[derive(Debug, Clone, Copy)]
pub struct ImageTexEntry {
    /// Index into the pipeline's texture slots.
    pub texture_index: usize,
    /// Pixel dimensions of the source image.
    pub width: u32,
    pub height: u32,
    /// Content fingerprint we uploaded from the registry. A mismatch
    /// on the next registry sync means the image was replaced in place.
    pub content_hash: u32,
}

impl ImageTexEntry {
    /// GPU bytes held by this entry's texture (RGBA8, no mips).
    #[must_use]
    pub fn byte_size(&self) -> u64 {
        u64::from(self.width.max(1)) * u64::from(self.height.max(1)) * 4
    }
}

#[derive(Debug, Clone, Copy)]
struct Slot {
    entry: ImageTexEntry,
    /// Frame number of the last draw that used this texture.
    last_used: u64,
}

/// Byte-budgeted cache of image textures keyed by kitty image id, with
/// LRU-by-last-draw eviction.
///
/// The cache only tracks *metadata*; the `wgpu::Texture`s live on
/// [`super::pipeline::ImagePipeline`] (so unit tests of the cache logic
/// don't need a GPU).
#[derive(Debug)]
pub struct ImageTextureCache {
    entries: HashMap<u32, Slot>,
    /// Sum of [`ImageTexEntry::byte_size`] over `entries`.
    total_bytes: u64,
    /// Soft cap on `total_bytes`. Only textures not drawn this frame
    /// are evicted to meet it.
    budget_bytes: u64,
    /// Current frame number, bumped by [`Self::begin_frame`].
    frame: u64,
}

impl Default for ImageTextureCache {
    fn default() -> Self {
        Self::new(Self::DEFAULT_BUDGET_BYTES)
    }
}

impl ImageTextureCache {
    /// Default GPU budget for resident image textures: 256 MiB, a bit
    /// under the 320 MiB CPU-side registry quota (kitty's default).
    pub const DEFAULT_BUDGET_BYTES: u64 = 256 * 1024 * 1024;

    /// Fresh cache with a `budget_bytes` soft cap.
    #[must_use]
    pub fn new(budget_bytes: u64) -> Self {
        Self {
            entries: HashMap::new(),
            total_bytes: 0,
            budget_bytes,
            frame: 0,
        }
    }

    /// The soft byte cap.
    #[must_use]
    pub fn budget_bytes(&self) -> u64 {
        self.budget_bytes
    }

    /// Bytes held by resident textures.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Number of resident entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no entries are resident.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Start a new frame. Entries touched after this count as "in use"
    /// and are exempt from [`Self::evict_over_budget`].
    pub fn begin_frame(&mut self) {
        self.frame += 1;
    }

    /// Iterate `(image_id, entry)` in arbitrary order.
    pub fn iter(&self) -> impl Iterator<Item = (u32, ImageTexEntry)> + '_ {
        self.entries.iter().map(|(k, v)| (*k, v.entry))
    }

    /// Lookup without marking the entry used.
    #[must_use]
    pub fn get(&self, id: u32) -> Option<ImageTexEntry> {
        self.entries.get(&id).map(|s| s.entry)
    }

    /// Lookup and mark the entry used this frame.
    pub fn get_touch(&mut self, id: u32) -> Option<ImageTexEntry> {
        let frame = self.frame;
        let slot = self.entries.get_mut(&id)?;
        slot.last_used = frame;
        Some(slot.entry)
    }

    /// Insert (or replace) `id`'s entry, marked used this frame.
    /// Returns the replaced entry, if any: the caller frees its texture.
    /// Never evicts; call [`Self::evict_over_budget`] once the frame's
    /// working set is resident.
    pub fn insert(&mut self, id: u32, entry: ImageTexEntry) -> Option<ImageTexEntry> {
        let prior = self.entries.insert(
            id,
            Slot {
                entry,
                last_used: self.frame,
            },
        );
        self.total_bytes += entry.byte_size();
        prior.map(|s| {
            self.total_bytes -= s.entry.byte_size();
            s.entry
        })
    }

    /// Remove `id` if present. Returns its prior entry.
    pub fn remove(&mut self, id: u32) -> Option<ImageTexEntry> {
        let s = self.entries.remove(&id)?;
        self.total_bytes -= s.entry.byte_size();
        Some(s.entry)
    }

    /// While over budget, drop the least recently drawn entries that
    /// were not used this frame. Returns the evicted `(id, entry)`s: the
    /// caller frees their textures.
    pub fn evict_over_budget(&mut self) -> Vec<(u32, ImageTexEntry)> {
        if self.total_bytes <= self.budget_bytes {
            return Vec::new();
        }
        let frame = self.frame;
        let mut candidates: Vec<(u64, u32)> = self
            .entries
            .iter()
            .filter(|(_, s)| s.last_used != frame)
            .map(|(id, s)| (s.last_used, *id))
            .collect();
        // Oldest first; tie-break by id so eviction is deterministic.
        candidates.sort_unstable();
        let mut evicted = Vec::new();
        for (_, id) in candidates {
            if self.total_bytes <= self.budget_bytes {
                break;
            }
            if let Some(e) = self.remove(id) {
                evicted.push((id, e));
            }
        }
        evicted
    }

    /// Drop every entry. Returns the dropped entries.
    pub fn clear(&mut self) -> Vec<(u32, ImageTexEntry)> {
        self.total_bytes = 0;
        self.entries.drain().map(|(id, s)| (id, s.entry)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An entry of exactly `bytes` bytes (`bytes / 4` × 1 pixels).
    fn entry(idx: usize, bytes: u32) -> ImageTexEntry {
        ImageTexEntry {
            texture_index: idx,
            width: bytes / 4,
            height: 1,
            content_hash: 0,
        }
    }

    #[test]
    fn empty_cache() {
        let c = ImageTextureCache::new(400);
        assert!(c.is_empty());
        assert_eq!(c.len(), 0);
        assert_eq!(c.total_bytes(), 0);
        assert_eq!(c.budget_bytes(), 400);
    }

    #[test]
    fn insert_tracks_bytes_and_never_evicts() {
        let mut c = ImageTextureCache::new(100);
        // 500 entries, far past any entry-count limit and the byte
        // budget: all stay resident until an explicit eviction.
        for id in 0..500u32 {
            assert!(c.insert(id, entry(id as usize, 40)).is_none());
        }
        assert_eq!(c.len(), 500);
        assert_eq!(c.total_bytes(), 500 * 40);
    }

    #[test]
    fn replace_returns_prior_and_adjusts_bytes() {
        let mut c = ImageTextureCache::new(1000);
        c.insert(1, entry(0, 40));
        let prior = c.insert(1, entry(9, 80)).unwrap();
        assert_eq!(prior.texture_index, 0);
        assert_eq!(c.len(), 1);
        assert_eq!(c.total_bytes(), 80);
        assert_eq!(c.get(1).unwrap().texture_index, 9);
    }

    #[test]
    fn remove_and_clear_release_bytes() {
        let mut c = ImageTextureCache::new(1000);
        c.insert(1, entry(0, 40));
        c.insert(2, entry(1, 40));
        assert_eq!(c.remove(1).unwrap().texture_index, 0);
        assert_eq!(c.total_bytes(), 40);
        assert!(c.remove(1).is_none());
        assert_eq!(c.clear().len(), 1);
        assert!(c.is_empty());
        assert_eq!(c.total_bytes(), 0);
    }

    #[test]
    fn under_budget_evicts_nothing() {
        let mut c = ImageTextureCache::new(100);
        c.insert(1, entry(0, 40));
        c.begin_frame();
        c.insert(2, entry(1, 40));
        assert!(c.evict_over_budget().is_empty());
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn over_budget_evicts_least_recently_drawn() {
        let mut c = ImageTextureCache::new(100);
        // Frame 1: insert 1, 2. Frame 2: draw 1 only. Frame 3: insert 3.
        c.begin_frame();
        c.insert(1, entry(0, 40));
        c.insert(2, entry(1, 40));
        c.begin_frame();
        assert!(c.get_touch(1).is_some());
        c.begin_frame();
        c.insert(3, entry(2, 40));
        let evicted: Vec<u32> = c.evict_over_budget().iter().map(|e| e.0).collect();
        // 2 was drawn longest ago; evicting it brings 120 -> 80 bytes.
        assert_eq!(evicted, vec![2]);
        assert!(c.get(1).is_some());
        assert!(c.get(3).is_some());
        assert_eq!(c.total_bytes(), 80);
    }

    #[test]
    fn entries_drawn_this_frame_are_never_evicted() {
        // Working set larger than the budget: everything drawn this
        // frame stays, so a re-draw next frame needs no re-upload.
        let mut c = ImageTextureCache::new(100);
        c.begin_frame();
        for id in 0..10u32 {
            c.insert(id, entry(id as usize, 40));
        }
        assert!(c.evict_over_budget().is_empty());
        assert_eq!(c.len(), 10);
        // Next frame draws the same ten: all hits, nothing evicted.
        c.begin_frame();
        for id in 0..10u32 {
            assert!(c.get_touch(id).is_some());
        }
        assert!(c.evict_over_budget().is_empty());
        // A frame that draws only two lets the other eight go.
        c.begin_frame();
        c.get_touch(3);
        c.get_touch(7);
        let evicted = c.evict_over_budget();
        assert_eq!(c.total_bytes(), 80);
        assert_eq!(evicted.len(), 8);
        assert!(c.get(3).is_some() && c.get(7).is_some());
    }

    #[test]
    fn get_does_not_count_as_a_draw() {
        let mut c = ImageTextureCache::new(40);
        c.begin_frame();
        c.insert(1, entry(0, 40));
        c.begin_frame();
        c.insert(2, entry(1, 40));
        let _ = c.get(1);
        let evicted: Vec<u32> = c.evict_over_budget().iter().map(|e| e.0).collect();
        assert_eq!(evicted, vec![1]);
    }
}
