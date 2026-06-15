//! `ImageRegistry`: LRU-bounded cache of decoded image bytes keyed by id.
//!
//! Architecture choice (M11a, pre-approved): texture-per-image — the
//! atlas is per-image GPU texture rather than a packed mega-atlas. This
//! registry tracks the *decoded CPU bytes*; the renderer's
//! `ImageTextureCache` mirrors entries onto the GPU.
//!
//! Storage: `HashMap<id, ImageData>` keyed by Kitty image id, plus an
//! LRU `VecDeque<u32>` for eviction order. The cap is in bytes (configured
//! by the binary based on free memory / a fixed budget).
//!
//! Identifier policy:
//! - Caller supplies an explicit id (Kitty `i=`).
//! - If `0`, the registry assigns the lowest unused 1-based id.

use std::collections::{HashMap, HashSet, VecDeque};

/// Decoded image payload, fully resident in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageData {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Tightly-packed RGBA8 pixels (row-major, top-down, no padding).
    pub pixels: Vec<u8>,
}

impl ImageData {
    /// Total bytes stored in `pixels` (i.e. `width * height * 4` for
    /// RGBA8).
    #[must_use]
    pub fn byte_size(&self) -> u64 {
        self.pixels.len() as u64
    }
}

/// LRU-bounded cache of decoded images keyed by Kitty image id.
#[derive(Debug)]
pub struct ImageRegistry {
    entries: HashMap<u32, ImageData>,
    /// Eviction order. Most recently used at the back.
    lru: VecDeque<u32>,
    /// Sum of `byte_size()` across `entries`.
    total_bytes: u64,
    /// Soft cap. Insertions evict until `total_bytes + new <= cap_bytes`.
    cap_bytes: u64,
    /// Monotonic content version. Bumps on every successful insert /
    /// remove. The renderer compares this to its cached value to decide
    /// when to re-sync GPU textures.
    revision: u32,
}

impl ImageRegistry {
    /// Empty registry with `cap_bytes` byte budget.
    #[must_use]
    pub fn new(cap_bytes: u64) -> Self {
        Self {
            entries: HashMap::new(),
            lru: VecDeque::new(),
            total_bytes: 0,
            cap_bytes,
            revision: 0,
        }
    }

    /// Current byte cap.
    #[must_use]
    pub fn cap_bytes(&self) -> u64 {
        self.cap_bytes
    }

    /// Total bytes resident.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Bytes still free under `cap_bytes`. Saturates at 0.
    #[must_use]
    pub fn budget_remaining(&self) -> u64 {
        self.cap_bytes.saturating_sub(self.total_bytes)
    }

    /// Number of resident images.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no images are resident.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Monotonic content revision counter. Bumps on every insert /
    /// remove.
    #[must_use]
    pub fn revision(&self) -> u32 {
        self.revision
    }

    /// True iff `id` is resident.
    #[must_use]
    pub fn contains(&self, id: u32) -> bool {
        self.entries.contains_key(&id)
    }

    /// Borrow an image by id. Does NOT touch the LRU.
    #[must_use]
    pub fn get(&self, id: u32) -> Option<&ImageData> {
        self.entries.get(&id)
    }

    /// Borrow an image by id and mark it MRU.
    pub fn touch(&mut self, id: u32) -> Option<&ImageData> {
        if !self.entries.contains_key(&id) {
            return None;
        }
        self.bump_to_back(id);
        self.entries.get(&id)
    }

    /// Iterate (id, image) in MRU order (oldest first).
    pub fn iter(&self) -> impl Iterator<Item = (u32, &ImageData)> {
        self.lru
            .iter()
            .copied()
            .filter_map(|id| self.entries.get(&id).map(|d| (id, d)))
    }

    /// Insert `data` under `id`. If `id == 0` the registry assigns the
    /// lowest unused id (starting at 1). If the data is larger than the
    /// cap, returns `Err(InsertError::TooLarge)`. Otherwise evicts LRU
    /// entries until the new image fits.
    ///
    /// An insert under an existing id replaces the prior entry without
    /// disturbing other ids' LRU order.
    pub fn insert(&mut self, id: u32, data: ImageData) -> Result<Inserted, InsertError> {
        self.insert_with_pinned(id, data, &HashSet::new())
    }

    /// Like [`insert`](Self::insert), but `pinned` lists image ids that
    /// currently have live placements. Per the kitty spec — "existing
    /// images without placements will be preferentially deleted" — the
    /// eviction prefers unpinned images and only evicts a pinned image
    /// when no unpinned candidate remains. Both passes still respect LRU
    /// order (oldest first).
    pub fn insert_with_pinned(
        &mut self,
        id: u32,
        data: ImageData,
        pinned: &HashSet<u32>,
    ) -> Result<Inserted, InsertError> {
        let need = data.byte_size();
        if need > self.cap_bytes {
            return Err(InsertError::TooLarge {
                need,
                cap: self.cap_bytes,
            });
        }
        let final_id = if id == 0 { self.next_free_id() } else { id };

        // Replace path: free the old bytes first so eviction calculus
        // uses post-replace totals.
        if let Some(old) = self.entries.remove(&final_id) {
            self.total_bytes -= old.byte_size();
            // Drop the stale LRU entry; we'll push the fresh one below.
            self.lru.retain(|x| *x != final_id);
        }

        // Evict until we have room. Don't ever evict the id we're about
        // to insert (removed above so it's no longer in `lru`).
        //
        // m6: a single LRU pop would evict an actively-displayed image
        // even when an unplaced one is available. Instead pick the LRU
        // victim from the *unpinned* images first; only when none remain
        // fall back to the strict LRU front (which may be pinned).
        let mut evicted = Vec::new();
        while self.total_bytes + need > self.cap_bytes {
            // Prefer the oldest unpinned image.
            let victim = self
                .lru
                .iter()
                .copied()
                .find(|x| !pinned.contains(x))
                // Fall back to the strict LRU front if everything left is
                // pinned (we must still free space to honor the cap).
                .or_else(|| self.lru.front().copied());
            let Some(victim) = victim else {
                // Empty LRU; the cap check above guarantees the insert
                // fits, so this is purely defensive.
                break;
            };
            self.lru.retain(|x| *x != victim);
            if let Some(victim_data) = self.entries.remove(&victim) {
                self.total_bytes -= victim_data.byte_size();
                evicted.push(victim);
            }
        }

        self.total_bytes += need;
        self.lru.push_back(final_id);
        self.entries.insert(final_id, data);
        self.revision = self.revision.wrapping_add(1);
        Ok(Inserted {
            id: final_id,
            evicted,
        })
    }

    /// Remove `id`. Returns the freed image if it was present.
    pub fn remove(&mut self, id: u32) -> Option<ImageData> {
        let data = self.entries.remove(&id)?;
        self.total_bytes -= data.byte_size();
        self.lru.retain(|x| *x != id);
        self.revision = self.revision.wrapping_add(1);
        Some(data)
    }

    /// Iterate every resident id.
    pub fn ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.entries.keys().copied()
    }

    /// Resize the byte cap. If `new_cap < total_bytes`, evicts LRU
    /// entries until the budget is satisfied. Returns the evicted ids.
    pub fn set_cap(&mut self, new_cap: u64) -> Vec<u32> {
        self.cap_bytes = new_cap;
        let mut evicted = Vec::new();
        while self.total_bytes > self.cap_bytes {
            let Some(victim) = self.lru.pop_front() else {
                break;
            };
            if let Some(data) = self.entries.remove(&victim) {
                self.total_bytes -= data.byte_size();
                evicted.push(victim);
                self.revision = self.revision.wrapping_add(1);
            }
        }
        evicted
    }

    fn bump_to_back(&mut self, id: u32) {
        self.lru.retain(|x| *x != id);
        self.lru.push_back(id);
    }

    /// Lowest 1-based id not currently resident. Worst case is O(n) but
    /// `n` is small.
    fn next_free_id(&self) -> u32 {
        let mut candidate = 1u32;
        loop {
            if !self.entries.contains_key(&candidate) {
                return candidate;
            }
            candidate += 1;
        }
    }
}

/// Outcome of a successful [`ImageRegistry::insert`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inserted {
    /// Final id under which the image is stored. Same as the `id`
    /// passed in if it was non-zero; freshly assigned otherwise.
    pub id: u32,
    /// Ids evicted to make room (oldest first).
    pub evicted: Vec<u32>,
}

/// Errors from `ImageRegistry::insert`.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InsertError {
    /// Decoded image is larger than `cap_bytes`. No partial state is
    /// committed.
    #[error("image too large: need {need} bytes, cap is {cap}")]
    TooLarge { need: u64, cap: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(w: u32, h: u32, fill: u8) -> ImageData {
        ImageData {
            width: w,
            height: h,
            pixels: vec![fill; (w * h * 4) as usize],
        }
    }

    #[test]
    fn empty_registry() {
        let r = ImageRegistry::new(1024);
        assert_eq!(r.len(), 0);
        assert!(r.is_empty());
        assert_eq!(r.total_bytes(), 0);
        assert_eq!(r.budget_remaining(), 1024);
    }

    #[test]
    fn insert_assigns_id_when_zero() {
        let mut r = ImageRegistry::new(64 * 1024);
        let res = r.insert(0, img(2, 2, 0)).unwrap();
        assert_eq!(res.id, 1);
        assert!(res.evicted.is_empty());
        let res = r.insert(0, img(2, 2, 0)).unwrap();
        assert_eq!(res.id, 2);
    }

    #[test]
    fn insert_uses_explicit_id() {
        let mut r = ImageRegistry::new(64 * 1024);
        let res = r.insert(42, img(2, 2, 0)).unwrap();
        assert_eq!(res.id, 42);
        assert!(r.contains(42));
    }

    #[test]
    fn insert_replaces_existing_id() {
        let mut r = ImageRegistry::new(64 * 1024);
        r.insert(1, img(2, 2, 1)).unwrap();
        let before_bytes = r.total_bytes();
        r.insert(1, img(4, 4, 2)).unwrap();
        // Size changed from 16 → 64 bytes; total reflects only the new.
        assert_eq!(r.total_bytes(), 4 * 4 * 4);
        assert_ne!(r.total_bytes(), before_bytes);
        let d = r.get(1).unwrap();
        assert_eq!(d.pixels[0], 2);
    }

    #[test]
    fn too_large_returns_err() {
        // 2x2 RGBA = 16 bytes; equals the cap so it fits.
        let mut r = ImageRegistry::new(16);
        assert!(r.insert(1, img(2, 2, 0)).is_ok());
        // Truly oversized: 4x4 = 64 bytes > cap.
        let err = r.insert(2, img(4, 4, 0)).unwrap_err();
        match err {
            InsertError::TooLarge { need, cap } => {
                assert_eq!(need, 64);
                assert_eq!(cap, 16);
            }
        }
    }

    #[test]
    fn lru_evicts_oldest() {
        // Cap: 3 entries × 16 bytes = 48; we'll add a 4th to trigger
        // eviction.
        let mut r = ImageRegistry::new(48);
        r.insert(1, img(2, 2, 0)).unwrap();
        r.insert(2, img(2, 2, 0)).unwrap();
        r.insert(3, img(2, 2, 0)).unwrap();
        assert_eq!(r.total_bytes(), 48);
        // 4th forces eviction of the oldest (id=1).
        let res = r.insert(4, img(2, 2, 0)).unwrap();
        assert_eq!(res.evicted, vec![1]);
        assert!(!r.contains(1));
        assert!(r.contains(2));
        assert!(r.contains(3));
        assert!(r.contains(4));
    }

    #[test]
    fn touch_promotes_to_mru() {
        let mut r = ImageRegistry::new(48);
        r.insert(1, img(2, 2, 0)).unwrap();
        r.insert(2, img(2, 2, 0)).unwrap();
        r.insert(3, img(2, 2, 0)).unwrap();
        // Touch 1; now LRU = [2, 3, 1].
        r.touch(1);
        let res = r.insert(4, img(2, 2, 0)).unwrap();
        // 2 should be evicted now, not 1.
        assert_eq!(res.evicted, vec![2]);
        assert!(r.contains(1));
    }

    #[test]
    fn remove_drops_from_lru_and_decreases_total() {
        let mut r = ImageRegistry::new(64);
        r.insert(1, img(2, 2, 0)).unwrap();
        r.insert(2, img(2, 2, 0)).unwrap();
        assert_eq!(r.total_bytes(), 32);
        let removed = r.remove(1).unwrap();
        assert_eq!(removed.byte_size(), 16);
        assert_eq!(r.total_bytes(), 16);
        assert!(!r.contains(1));
    }

    #[test]
    fn revision_increments_on_mutation() {
        let mut r = ImageRegistry::new(1024);
        let r0 = r.revision();
        r.insert(1, img(1, 1, 0)).unwrap();
        let r1 = r.revision();
        assert_ne!(r0, r1);
        r.remove(1);
        let r2 = r.revision();
        assert_ne!(r1, r2);
    }

    #[test]
    fn auto_assign_fills_holes_lowest_first() {
        let mut r = ImageRegistry::new(1024);
        r.insert(1, img(1, 1, 0)).unwrap();
        r.insert(3, img(1, 1, 0)).unwrap();
        let res = r.insert(0, img(1, 1, 0)).unwrap();
        assert_eq!(res.id, 2);
    }

    #[test]
    fn budget_remaining_tracks_inserts() {
        let mut r = ImageRegistry::new(100);
        assert_eq!(r.budget_remaining(), 100);
        r.insert(1, img(2, 2, 0)).unwrap(); // 16 bytes
        assert_eq!(r.budget_remaining(), 84);
    }

    #[test]
    fn set_cap_evicts_to_fit() {
        let mut r = ImageRegistry::new(64);
        r.insert(1, img(2, 2, 0)).unwrap();
        r.insert(2, img(2, 2, 0)).unwrap();
        r.insert(3, img(2, 2, 0)).unwrap();
        r.insert(4, img(2, 2, 0)).unwrap();
        assert_eq!(r.total_bytes(), 64);
        let evicted = r.set_cap(32);
        assert_eq!(evicted.len(), 2);
        assert!(r.total_bytes() <= 32);
    }

    #[test]
    fn ids_iteration_covers_all() {
        let mut r = ImageRegistry::new(1024);
        r.insert(1, img(1, 1, 0)).unwrap();
        r.insert(7, img(1, 1, 0)).unwrap();
        let mut ids: Vec<u32> = r.ids().collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 7]);
    }

    // ---- m6: placement-aware eviction -------------------------------

    #[test]
    fn eviction_prefers_unpinned_over_pinned() {
        // Cap holds two 16-byte images. id=1 is the LRU front but PINNED
        // (has a live placement); id=2 is unpinned. Inserting a third
        // must evict the unpinned id=2, not the pinned-but-older id=1.
        let mut r = ImageRegistry::new(32);
        r.insert(1, img(2, 2, 0)).unwrap();
        r.insert(2, img(2, 2, 0)).unwrap();
        let mut pinned = HashSet::new();
        pinned.insert(1u32);
        let res = r.insert_with_pinned(3, img(2, 2, 0), &pinned).unwrap();
        assert_eq!(res.evicted, vec![2], "unpinned image must be evicted first");
        assert!(r.contains(1), "pinned image must survive");
        assert!(!r.contains(2));
        assert!(r.contains(3));
    }

    #[test]
    fn eviction_falls_back_to_pinned_when_no_unpinned_candidate() {
        // Both resident images are pinned and we must still free space.
        // Eviction falls back to the strict LRU front (id=1).
        let mut r = ImageRegistry::new(32);
        r.insert(1, img(2, 2, 0)).unwrap();
        r.insert(2, img(2, 2, 0)).unwrap();
        let mut pinned = HashSet::new();
        pinned.insert(1u32);
        pinned.insert(2u32);
        let res = r.insert_with_pinned(3, img(2, 2, 0), &pinned).unwrap();
        assert_eq!(
            res.evicted,
            vec![1],
            "fall back to LRU front when all pinned"
        );
        assert!(!r.contains(1));
        assert!(r.contains(2));
        assert!(r.contains(3));
    }

    #[test]
    fn eviction_prefers_unpinned_respecting_lru_order() {
        // Three images, cap holds three (48 bytes). Pin the oldest (1).
        // Insert a 4th: should evict the oldest UNPINNED (2), keeping the
        // pinned-oldest (1) and newer (3).
        let mut r = ImageRegistry::new(48);
        r.insert(1, img(2, 2, 0)).unwrap();
        r.insert(2, img(2, 2, 0)).unwrap();
        r.insert(3, img(2, 2, 0)).unwrap();
        let mut pinned = HashSet::new();
        pinned.insert(1u32);
        let res = r.insert_with_pinned(4, img(2, 2, 0), &pinned).unwrap();
        assert_eq!(res.evicted, vec![2]);
        assert!(r.contains(1));
        assert!(!r.contains(2));
        assert!(r.contains(3));
        assert!(r.contains(4));
    }

    #[test]
    fn insert_with_empty_pinned_matches_plain_insert() {
        // Sanity: insert_with_pinned with no pins behaves like insert
        // (strict LRU).
        let mut r = ImageRegistry::new(48);
        r.insert(1, img(2, 2, 0)).unwrap();
        r.insert(2, img(2, 2, 0)).unwrap();
        r.insert(3, img(2, 2, 0)).unwrap();
        let res = r
            .insert_with_pinned(4, img(2, 2, 0), &HashSet::new())
            .unwrap();
        assert_eq!(res.evicted, vec![1]);
    }

    #[test]
    fn replace_doesnt_evict_self() {
        // Cap: 32 bytes. One 16-byte entry. Replacing it should NOT
        // evict it (which would leave total_bytes 0 then add 16) — the
        // entry should just be swapped in place.
        let mut r = ImageRegistry::new(32);
        r.insert(1, img(2, 2, 1)).unwrap();
        let res = r.insert(1, img(2, 2, 9)).unwrap();
        assert!(res.evicted.is_empty());
        assert!(r.contains(1));
        assert_eq!(r.get(1).unwrap().pixels[0], 9);
    }
}
