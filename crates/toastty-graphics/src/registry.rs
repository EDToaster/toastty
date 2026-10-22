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

/// Key of stored image bytes in an [`crate::ImageStore`]: unique per
/// transmission across both screens, and never the client's `i=` id. A
/// newtype so a client id can't be used to look bytes up by mistake
/// (that would compile and silently draw another image). `ImageKey(0)`
/// ([`ImageKey::NONE`]) is never handed out and finds nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct ImageKey(pub u32);

impl ImageKey {
    /// The key no image is stored under.
    pub const NONE: Self = Self(0);
}

impl From<u32> for ImageKey {
    fn from(v: u32) -> Self {
        Self(v)
    }
}

impl From<ImageKey> for u32 {
    fn from(k: ImageKey) -> Self {
        k.0
    }
}

/// What an [`ImageRegistry`] can be keyed by: a plain `u32` id (tests,
/// standalone use) or an [`ImageKey`] (a screen's `ImageStore`).
pub trait RegistryKey:
    Copy + Eq + std::hash::Hash + std::fmt::Debug + From<u32> + Into<u32>
{
}

impl<K: Copy + Eq + std::hash::Hash + std::fmt::Debug + From<u32> + Into<u32>> RegistryKey for K {}

/// LRU-bounded cache of decoded images.
#[derive(Debug)]
pub struct ImageRegistry<K: RegistryKey = u32> {
    entries: HashMap<K, ImageData>,
    /// Eviction order. Most recently used at the back.
    lru: VecDeque<K>,
    /// Sum of each entry's cost (`byte_size() + overhead`).
    total_bytes: u64,
    /// Soft cap. Insertions evict until `total_bytes + new <= cap_bytes`.
    cap_bytes: u64,
    /// Bytes charged per image on top of its pixels, so millions of tiny
    /// images can't fit a quota that only counts pixels (each entry costs
    /// real memory in the maps and the LRU). `0` unless set.
    overhead: u64,
    /// Monotonic content version. Bumps on every successful insert /
    /// remove. The renderer compares this to its cached value to decide
    /// when to re-sync GPU textures.
    revision: u32,
    /// Per-image generation, stamped on every insert (including an
    /// in-place replace under the same id) from `next_generation`. The
    /// renderer keys its GPU textures on it, so any re-transmit is seen
    /// as new content regardless of size or pixels.
    generations: HashMap<K, u64>,
    /// Next generation to hand out. Starts at 1, never reused.
    next_generation: u64,
    /// Ids sent with the transient usage hint (`N=1`). Under storage
    /// pressure they are evicted before other images without placements
    /// (spec "Usage hints"; §9).
    transient: HashSet<K>,
}

impl<K: RegistryKey> ImageRegistry<K> {
    /// Empty registry with `cap_bytes` byte budget.
    #[must_use]
    pub fn new(cap_bytes: u64) -> Self {
        Self::with_overhead(cap_bytes, 0)
    }

    /// Empty registry charging `overhead` bytes per image on top of its
    /// pixels against `cap_bytes`.
    #[must_use]
    pub fn with_overhead(cap_bytes: u64, overhead: u64) -> Self {
        Self {
            entries: HashMap::new(),
            lru: VecDeque::new(),
            total_bytes: 0,
            cap_bytes,
            overhead,
            revision: 0,
            generations: HashMap::new(),
            next_generation: 1,
            transient: HashSet::new(),
        }
    }

    /// What `data` costs against the cap: its pixel bytes plus the
    /// per-image overhead.
    #[must_use]
    pub fn cost(&self, data: &ImageData) -> u64 {
        data.byte_size() + self.overhead
    }

    /// Current byte cap.
    #[must_use]
    pub fn cap_bytes(&self) -> u64 {
        self.cap_bytes
    }

    /// Total bytes charged (pixels plus per-image overhead).
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
    pub fn contains(&self, id: K) -> bool {
        self.entries.contains_key(&id)
    }

    /// Borrow an image by id. Does NOT touch the LRU.
    #[must_use]
    pub fn get(&self, id: K) -> Option<&ImageData> {
        self.entries.get(&id)
    }

    /// The generation `id`'s current data was stored under: unique per
    /// insert, so a re-transmit under the same id (same size, same
    /// pixels or not) always yields a new value. `None` if not resident.
    #[must_use]
    pub fn generation(&self, id: K) -> Option<u64> {
        self.generations.get(&id).copied()
    }

    /// Borrow an image by id and mark it MRU.
    pub fn touch(&mut self, id: K) -> Option<&ImageData> {
        if !self.entries.contains_key(&id) {
            return None;
        }
        self.bump_to_back(id);
        self.entries.get(&id)
    }

    /// Iterate (id, image) in MRU order (oldest first).
    pub fn iter(&self) -> impl Iterator<Item = (K, &ImageData)> {
        self.lru
            .iter()
            .copied()
            .filter_map(|id| self.entries.get(&id).map(|d| (id, d)))
    }

    /// Insert `data` under `id`. If `id` is 0 the registry assigns the
    /// lowest unused id (starting at 1). If the data is larger than the
    /// cap, returns `Err(InsertError::TooLarge)`. Otherwise evicts LRU
    /// entries until the new image fits.
    ///
    /// An insert under an existing id replaces the prior entry without
    /// disturbing other ids' LRU order.
    pub fn insert(&mut self, id: K, data: ImageData) -> Result<Inserted<K>, InsertError> {
        self.insert_with_pinned(id, data, &HashSet::new())
    }

    /// Like [`insert`](Self::insert), but `pinned` lists image ids that
    /// currently have live placements. Per the kitty spec — "existing
    /// images without placements will be preferentially deleted" — the
    /// eviction prefers unpinned images and only evicts a pinned image
    /// when no unpinned candidate remains. Among unpinned images, ones
    /// marked transient ([`set_transient`](Self::set_transient), `N=1`)
    /// go first. Every pass respects LRU order (oldest first).
    ///
    /// The new entry starts out non-transient; mark it after inserting.
    pub fn insert_with_pinned(
        &mut self,
        id: K,
        data: ImageData,
        pinned: &HashSet<K>,
    ) -> Result<Inserted<K>, InsertError> {
        let need = self.cost(&data);
        if need > self.cap_bytes {
            return Err(InsertError::TooLarge {
                need,
                cap: self.cap_bytes,
            });
        }
        let final_id = if id.into() == 0 {
            self.next_free_id()
        } else {
            id
        };

        // Replace path: free the old bytes first so eviction calculus
        // uses post-replace totals.
        if let Some(old) = self.entries.remove(&final_id) {
            self.total_bytes -= self.cost(&old);
            // Drop the stale LRU entry; we'll push the fresh one below.
            self.lru.retain(|x| *x != final_id);
        }
        // A usage hint belongs to the data it was sent with.
        self.transient.remove(&final_id);

        // Evict until we have room. Don't ever evict the id we're about
        // to insert (removed above so it's no longer in `lru`).
        //
        // m6: a single LRU pop would evict an actively-displayed image
        // even when an unplaced one is available. Instead pick the LRU
        // victim from the *unpinned* images first; only when none remain
        // fall back to the strict LRU front (which may be pinned).
        let mut evicted = Vec::new();
        while self.total_bytes + need > self.cap_bytes {
            // Prefer the oldest unpinned transient image, then the oldest
            // unpinned one.
            let victim = self
                .lru
                .iter()
                .position(|x| !pinned.contains(x) && self.transient.contains(x))
                .or_else(|| self.lru.iter().position(|x| !pinned.contains(x)))
                // Fall back to the strict LRU front if everything left is
                // pinned (we must still free space to honor the cap).
                .or_else(|| (!self.lru.is_empty()).then_some(0));
            let Some(at) = victim else {
                // Empty LRU; the cap check above guarantees the insert
                // fits, so this is purely defensive.
                break;
            };
            // O(1) for the usual case, the front.
            let Some(victim) = self.lru.remove(at) else {
                break;
            };
            self.transient.remove(&victim);
            if let Some(victim_data) = self.entries.remove(&victim) {
                self.total_bytes -= self.cost(&victim_data);
                self.generations.remove(&victim);
                evicted.push(victim);
            }
        }

        self.total_bytes += need;
        self.lru.push_back(final_id);
        self.entries.insert(final_id, data);
        self.generations.insert(final_id, self.next_generation);
        self.next_generation += 1;
        self.revision = self.revision.wrapping_add(1);
        Ok(Inserted {
            id: final_id,
            evicted,
        })
    }

    /// Mark `id` as transient (`N=1`) or not. No-op for an unknown id.
    pub fn set_transient(&mut self, id: K, transient: bool) {
        if transient && self.entries.contains_key(&id) {
            self.transient.insert(id);
        } else {
            self.transient.remove(&id);
        }
    }

    /// True iff `id` is stored and marked transient.
    #[must_use]
    pub fn is_transient(&self, id: K) -> bool {
        self.transient.contains(&id)
    }

    /// Remove `id`. Returns the freed image if it was present.
    pub fn remove(&mut self, id: K) -> Option<ImageData> {
        self.transient.remove(&id);
        let data = self.entries.remove(&id)?;
        self.total_bytes -= self.cost(&data);
        self.generations.remove(&id);
        self.lru.retain(|x| *x != id);
        self.revision = self.revision.wrapping_add(1);
        Some(data)
    }

    /// Iterate every resident id.
    pub fn ids(&self) -> impl Iterator<Item = K> + '_ {
        self.entries.keys().copied()
    }

    /// Resize the byte cap. If `new_cap < total_bytes`, evicts LRU
    /// entries until the budget is satisfied. Returns the evicted ids.
    pub fn set_cap(&mut self, new_cap: u64) -> Vec<K> {
        self.cap_bytes = new_cap;
        let mut evicted = Vec::new();
        while self.total_bytes > self.cap_bytes {
            let Some(victim) = self.lru.pop_front() else {
                break;
            };
            self.transient.remove(&victim);
            if let Some(data) = self.entries.remove(&victim) {
                self.total_bytes -= self.cost(&data);
                self.generations.remove(&victim);
                evicted.push(victim);
                self.revision = self.revision.wrapping_add(1);
            }
        }
        evicted
    }

    fn bump_to_back(&mut self, id: K) {
        self.lru.retain(|x| *x != id);
        self.lru.push_back(id);
    }

    /// The id an insert with id `0` would get right now: the lowest
    /// 1-based id not currently resident. Worst case is O(n) but `n` is
    /// small.
    #[must_use]
    pub fn next_free_id(&self) -> K {
        let mut candidate = 1u32;
        loop {
            if !self.entries.contains_key(&K::from(candidate)) {
                return K::from(candidate);
            }
            candidate += 1;
        }
    }
}

/// Outcome of a successful [`ImageRegistry::insert`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inserted<K = u32> {
    /// Final id under which the image is stored. Same as the `id`
    /// passed in if it was non-zero; freshly assigned otherwise.
    pub id: K,
    /// Ids evicted to make room (oldest first).
    pub evicted: Vec<K>,
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
        let r = ImageRegistry::<u32>::new(1024);
        assert_eq!(r.len(), 0);
        assert!(r.is_empty());
        assert_eq!(r.total_bytes(), 0);
        assert_eq!(r.budget_remaining(), 1024);
    }

    #[test]
    fn insert_assigns_id_when_zero() {
        let mut r = ImageRegistry::<u32>::new(64 * 1024);
        let res = r.insert(0, img(2, 2, 0)).unwrap();
        assert_eq!(res.id, 1);
        assert!(res.evicted.is_empty());
        let res = r.insert(0, img(2, 2, 0)).unwrap();
        assert_eq!(res.id, 2);
    }

    #[test]
    fn insert_uses_explicit_id() {
        let mut r = ImageRegistry::<u32>::new(64 * 1024);
        let res = r.insert(42, img(2, 2, 0)).unwrap();
        assert_eq!(res.id, 42);
        assert!(r.contains(42));
    }

    #[test]
    fn insert_replaces_existing_id() {
        let mut r = ImageRegistry::<u32>::new(64 * 1024);
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
        let mut r = ImageRegistry::<u32>::new(16);
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
        let mut r = ImageRegistry::<u32>::new(48);
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
        let mut r = ImageRegistry::<u32>::new(48);
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
        let mut r = ImageRegistry::<u32>::new(64);
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
        let mut r = ImageRegistry::<u32>::new(1024);
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
        let mut r = ImageRegistry::<u32>::new(1024);
        r.insert(1, img(1, 1, 0)).unwrap();
        r.insert(3, img(1, 1, 0)).unwrap();
        let res = r.insert(0, img(1, 1, 0)).unwrap();
        assert_eq!(res.id, 2);
    }

    #[test]
    fn budget_remaining_tracks_inserts() {
        let mut r = ImageRegistry::<u32>::new(100);
        assert_eq!(r.budget_remaining(), 100);
        r.insert(1, img(2, 2, 0)).unwrap(); // 16 bytes
        assert_eq!(r.budget_remaining(), 84);
    }

    #[test]
    fn set_cap_evicts_to_fit() {
        let mut r = ImageRegistry::<u32>::new(64);
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
        let mut r = ImageRegistry::<u32>::new(1024);
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
        let mut r = ImageRegistry::<u32>::new(32);
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
        let mut r = ImageRegistry::<u32>::new(32);
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
        let mut r = ImageRegistry::<u32>::new(48);
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
        let mut r = ImageRegistry::<u32>::new(48);
        r.insert(1, img(2, 2, 0)).unwrap();
        r.insert(2, img(2, 2, 0)).unwrap();
        r.insert(3, img(2, 2, 0)).unwrap();
        let res = r
            .insert_with_pinned(4, img(2, 2, 0), &HashSet::new())
            .unwrap();
        assert_eq!(res.evicted, vec![1]);
    }

    /// §9 usage hints: an unplaced transient image is evicted before an
    /// older non-transient one, but a placed (pinned) image is still kept
    /// over an unplaced one.
    #[test]
    fn transient_images_are_evicted_first() {
        let mut r = ImageRegistry::<u32>::new(48);
        r.insert(1, img(2, 2, 1)).unwrap(); // oldest, normal
        r.insert(2, img(2, 2, 2)).unwrap();
        r.set_transient(2, true);
        r.insert(3, img(2, 2, 3)).unwrap();
        assert!(r.is_transient(2));
        let ins = r.insert(4, img(2, 2, 4)).unwrap();
        assert_eq!(ins.evicted, vec![2], "transient goes first");
        assert!(!r.is_transient(2));
        // Pinned transient is protected like any pinned image.
        r.set_transient(3, true);
        let pinned: HashSet<u32> = [3].into_iter().collect();
        let ins = r.insert_with_pinned(5, img(2, 2, 5), &pinned).unwrap();
        assert_eq!(
            ins.evicted,
            vec![1],
            "oldest unpinned, not pinned transient"
        );
    }

    #[test]
    fn transient_flag_is_cleared_by_replace_and_remove() {
        let mut r = ImageRegistry::<u32>::new(1024);
        r.insert(1, img(1, 1, 1)).unwrap();
        r.set_transient(1, true);
        r.insert(1, img(1, 1, 2)).unwrap();
        assert!(!r.is_transient(1), "a re-send without N=1 is not transient");
        r.set_transient(1, true);
        r.remove(1);
        assert!(!r.is_transient(1));
        r.set_transient(9, true);
        assert!(!r.is_transient(9), "unknown ids are not marked");
    }

    #[test]
    fn replace_doesnt_evict_self() {
        // Cap: 32 bytes. One 16-byte entry. Replacing it should NOT
        // evict it (which would leave total_bytes 0 then add 16) — the
        // entry should just be swapped in place.
        let mut r = ImageRegistry::<u32>::new(32);
        r.insert(1, img(2, 2, 1)).unwrap();
        let res = r.insert(1, img(2, 2, 9)).unwrap();
        assert!(res.evicted.is_empty());
        assert!(r.contains(1));
        assert_eq!(r.get(1).unwrap().pixels[0], 9);
    }

    #[test]
    fn generation_is_unique_per_insert_even_for_identical_replace() {
        let img = |fill: u8| ImageData {
            width: 2,
            height: 2,
            pixels: vec![fill; 16],
        };
        let mut r = ImageRegistry::<u32>::new(1024);
        assert_eq!(r.generation(7), None);
        r.insert(7, img(1)).unwrap();
        let g1 = r.generation(7).unwrap();
        // Same id, same size, identical bytes: still a new generation.
        r.insert(7, img(1)).unwrap();
        let g2 = r.generation(7).unwrap();
        assert_ne!(g1, g2);
        r.insert(8, img(1)).unwrap();
        let g3 = r.generation(8).unwrap();
        assert!(g3 != g1 && g3 != g2);
        r.remove(7);
        assert_eq!(r.generation(7), None);
    }

    #[test]
    fn reused_anonymous_id_gets_a_new_generation() {
        // Delete-then-anonymous-transmit reuses the lowest free id; the
        // generation must still differ so the GPU copy is refreshed.
        let img = ImageData {
            width: 1,
            height: 1,
            pixels: vec![0; 4],
        };
        let mut r = ImageRegistry::<u32>::new(1024);
        let id = r.insert(0, img.clone()).unwrap().id;
        let g1 = r.generation(id).unwrap();
        r.remove(id);
        let id2 = r.insert(0, img).unwrap().id;
        assert_eq!(id, id2);
        assert_ne!(r.generation(id2).unwrap(), g1);
    }

    #[test]
    fn evicted_images_lose_their_generation() {
        let img = ImageData {
            width: 2,
            height: 2,
            pixels: vec![0; 16],
        };
        let mut r = ImageRegistry::<u32>::new(32);
        r.insert(1, img.clone()).unwrap();
        r.insert(2, img.clone()).unwrap();
        let ins = r.insert(3, img).unwrap();
        assert_eq!(ins.evicted, vec![1]);
        assert_eq!(r.generation(1), None);
        r.set_cap(16);
        assert_eq!(r.generation(2), None);
        assert!(r.generation(3).is_some());
    }
}
