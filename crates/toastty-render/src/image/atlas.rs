//! GPU mirror of [`ImageRegistry`]: per-image texture cache.
//!
//! Architecture choice (M11a, pre-approved): texture-per-image, NOT a
//! packed atlas. Each `Texture` is sized for its source image; no
//! shelf packing. The cache holds up to [`Self::MAX_ACTIVE`] textures
//! and uses LRU eviction; that limit matches `wgpu::Limits::downlevel
//! _defaults().max_sampled_textures_per_shader_stage` so we never blow
//! past the binding budget.

use std::collections::{HashMap, VecDeque};

/// Bookkeeping for one image's GPU texture.
#[derive(Debug, Clone, Copy)]
pub struct ImageTexEntry {
    /// Index into [`ImageTextureCache::textures`].
    pub texture_index: usize,
    /// Pixel dimensions of the source image.
    pub width: u32,
    pub height: u32,
    /// Content version we synced from the registry — bumped on
    /// re-upload so the cache can detect re-uses with stale content.
    pub content_hash: u32,
}

/// LRU cache of `wgpu::Texture` keyed by Kitty image id.
///
/// The cache only tracks *metadata* — actual `wgpu::Texture` storage
/// lives on [`super::pipeline::ImagePipeline`] (so unit tests of the
/// cache logic don't need a GPU).
#[derive(Debug, Default)]
pub struct ImageTextureCache {
    entries: HashMap<u32, ImageTexEntry>,
    /// MRU at the back, LRU at the front.
    lru: VecDeque<u32>,
    /// Maximum number of active textures. wgpu's
    /// `downlevel_defaults` allows 16 sampled textures per shader
    /// stage; we keep one slot for the text mask + one for the text
    /// color, leaving 14 for images. Round down to 14 to be safe.
    max_active: usize,
}

impl ImageTextureCache {
    /// Sensible default cap (14).
    pub const DEFAULT_MAX_ACTIVE: usize = 14;

    /// Fresh cache with `max_active` slots.
    #[must_use]
    pub fn new(max_active: usize) -> Self {
        Self {
            entries: HashMap::new(),
            lru: VecDeque::new(),
            max_active: max_active.max(1),
        }
    }

    /// Maximum number of textures the cache will hold concurrently.
    #[must_use]
    pub fn max_active(&self) -> usize {
        self.max_active
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

    /// Iterate (image_id, entry) in resident order.
    pub fn iter(&self) -> impl Iterator<Item = (u32, ImageTexEntry)> + '_ {
        self.entries.iter().map(|(k, v)| (*k, *v))
    }

    /// Lookup without LRU touch.
    #[must_use]
    pub fn get(&self, id: u32) -> Option<ImageTexEntry> {
        self.entries.get(&id).copied()
    }

    /// Lookup + bump LRU.
    pub fn get_mut_touch(&mut self, id: u32) -> Option<ImageTexEntry> {
        let entry = self.entries.get(&id).copied()?;
        self.lru.retain(|x| *x != id);
        self.lru.push_back(id);
        Some(entry)
    }

    /// Bump `id` to MRU without retrieving the value.
    pub fn touch(&mut self, id: u32) {
        if self.entries.contains_key(&id) {
            self.lru.retain(|x| *x != id);
            self.lru.push_back(id);
        }
    }

    /// Insert (or replace) `id`'s entry. Returns the evicted ids (if
    /// any) — the caller is responsible for dropping the matching
    /// `wgpu::Texture` from external storage.
    pub fn insert(&mut self, id: u32, entry: ImageTexEntry) -> Vec<u32> {
        let mut evicted = Vec::new();
        // Replace path: drop the prior LRU position.
        if self.entries.contains_key(&id) {
            self.lru.retain(|x| *x != id);
        } else {
            // Evict until we have a slot.
            while self.entries.len() >= self.max_active {
                let Some(victim) = self.lru.pop_front() else {
                    break;
                };
                self.entries.remove(&victim);
                evicted.push(victim);
            }
        }
        self.entries.insert(id, entry);
        self.lru.push_back(id);
        evicted
    }

    /// Remove `id` if present. Returns its prior entry.
    pub fn remove(&mut self, id: u32) -> Option<ImageTexEntry> {
        let e = self.entries.remove(&id)?;
        self.lru.retain(|x| *x != id);
        Some(e)
    }

    /// Drop every entry. Returns the dropped ids in insertion order.
    pub fn clear(&mut self) -> Vec<u32> {
        let out: Vec<u32> = self.lru.drain(..).collect();
        self.entries.clear();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(idx: usize) -> ImageTexEntry {
        ImageTexEntry {
            texture_index: idx,
            width: 2,
            height: 2,
            content_hash: 0,
        }
    }

    #[test]
    fn empty_cache() {
        let c = ImageTextureCache::new(4);
        assert!(c.is_empty());
        assert_eq!(c.len(), 0);
        assert_eq!(c.max_active(), 4);
    }

    #[test]
    fn insert_under_cap_does_not_evict() {
        let mut c = ImageTextureCache::new(4);
        for id in 1..=3 {
            assert!(c.insert(id, entry(id as usize)).is_empty());
        }
        assert_eq!(c.len(), 3);
    }

    #[test]
    fn insert_over_cap_evicts_oldest() {
        let mut c = ImageTextureCache::new(2);
        assert!(c.insert(1, entry(0)).is_empty());
        assert!(c.insert(2, entry(1)).is_empty());
        let evicted = c.insert(3, entry(2));
        assert_eq!(evicted, vec![1]);
        assert_eq!(c.len(), 2);
        assert!(c.get(1).is_none());
    }

    #[test]
    fn touch_promotes_to_mru() {
        let mut c = ImageTextureCache::new(2);
        c.insert(1, entry(0));
        c.insert(2, entry(1));
        // Touch id=1; now LRU = [2, 1]
        c.touch(1);
        let evicted = c.insert(3, entry(2));
        // 2 evicted, not 1.
        assert_eq!(evicted, vec![2]);
        assert!(c.get(1).is_some());
    }

    #[test]
    fn replace_keeps_size() {
        let mut c = ImageTextureCache::new(2);
        c.insert(1, entry(0));
        c.insert(2, entry(1));
        let evicted = c.insert(1, entry(99));
        assert!(evicted.is_empty());
        assert_eq!(c.len(), 2);
        assert_eq!(c.get(1).unwrap().texture_index, 99);
    }

    #[test]
    fn remove_drops_entry_and_lru() {
        let mut c = ImageTextureCache::new(2);
        c.insert(1, entry(0));
        c.insert(2, entry(1));
        let removed = c.remove(1).unwrap();
        assert_eq!(removed.texture_index, 0);
        assert_eq!(c.len(), 1);
        // Re-insert is not flagged as evicting anything.
        assert!(c.insert(3, entry(2)).is_empty());
    }

    #[test]
    fn clear_returns_all_ids() {
        let mut c = ImageTextureCache::new(2);
        c.insert(1, entry(0));
        c.insert(2, entry(1));
        let ids = c.clear();
        assert_eq!(ids.len(), 2);
        assert!(c.is_empty());
    }
}
