//! `ImageStore`: one screen's kitty images (see
//! `docs/decisions/image-store.md`).
//!
//! kitty keeps a separate graphics manager per screen (main and alt), each
//! with its own images, ids, numbers and storage quota. A `Term` owns one
//! `ImageStore` per screen. Every id-, number- and quota-related operation
//! goes through the active screen's store, so nothing done on one screen
//! can free, replace or evict an image the other screen still shows.
//!
//! Two kinds of identifier:
//! - The **client id** (kitty's `i=`), which programs use to address an
//!   image. It is `0` for an *anonymous* image (no `i=` and no `I=`);
//!   such an image can't be addressed by id, so no `i=` or id-range
//!   command ever reaches it, as in kitty.
//! - The **key**, which stores the bytes. Keys are handed out by the owner
//!   (the `Term`), unique across both screens and never reused for a new
//!   transmission. `Placement::key` and the renderer's texture cache use
//!   keys, so a re-transmitted id or the other screen's image under the
//!   same id is always distinct content on the GPU.

use std::collections::{HashMap, HashSet};

use crate::registry::{ImageData, ImageKey, ImageRegistry, InsertError};

/// Bytes each stored image costs against the quota on top of its pixels.
/// A quota that counted only pixels would let a client store millions of
/// 1x1 images, each of which really costs a few hundred bytes of maps,
/// LRU slot and bookkeeping; this bounds the image count by the quota
/// too (320 MB / 256 B is ~1.3M images at worst, all of them 1x1).
pub const IMAGE_OVERHEAD_BYTES: u64 = 256;

/// One screen's images.
#[derive(Debug)]
pub struct ImageStore {
    /// Decoded bytes, keyed by key. Owns the LRU and the byte quota.
    registry: ImageRegistry<ImageKey>,
    /// Client id → key, for addressable images.
    by_client: HashMap<u32, ImageKey>,
    /// Key → client id (`0` = anonymous).
    client_of: HashMap<ImageKey, u32>,
    /// Image number (`I=`) → client ids of the live images transmitted
    /// with it, oldest first (`d=n` / `d=N`, `d=f` / `d=F`, `a=p,I=`).
    /// Keeping every id means that once the newest is freed the
    /// next-newest is still reachable, as in kitty (#35); an image re-sent
    /// by `i=` keeps its number (#37).
    numbers: HashMap<u32, Vec<u32>>,
}

/// Outcome of [`ImageStore::insert`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    /// Key the bytes were stored under (the one passed in).
    pub key: ImageKey,
    /// Client id the image answers to; `0` for an anonymous image.
    pub client_id: u32,
    /// Key of the image this one replaced (same client id), whose
    /// placements must go (spec: "the existing image and all its
    /// placements must be deleted", PLC-2).
    pub replaced: Option<ImageKey>,
    /// Keys evicted to make room (oldest first).
    pub evicted: Vec<ImageKey>,
}

impl ImageStore {
    /// Empty store with a `cap_bytes` quota. Each image costs its pixel
    /// bytes plus [`IMAGE_OVERHEAD_BYTES`].
    #[must_use]
    pub fn new(cap_bytes: u64) -> Self {
        Self {
            registry: ImageRegistry::with_overhead(cap_bytes, IMAGE_OVERHEAD_BYTES),
            by_client: HashMap::new(),
            client_of: HashMap::new(),
            numbers: HashMap::new(),
        }
    }

    /// The bytes, keyed by key (for the renderer).
    #[must_use]
    pub fn registry(&self) -> &ImageRegistry<ImageKey> {
        &self.registry
    }

    /// Store `data` under `key` (fresh, from the owner's allocator).
    ///
    /// `client_id` / `number` are the header's `i=` / `I=`:
    /// - `i=` given: the image answers to it, replacing any image with that
    ///   id (returned in [`Stored::replaced`]).
    /// - only `I=`: kitty assigns the lowest unused client id.
    /// - neither: anonymous (`client_id == 0`), not addressable.
    ///
    /// `pinned` lists keys with live placements; quota eviction prefers
    /// the others (kitty spec).
    pub fn insert(
        &mut self,
        key: ImageKey,
        client_id: u32,
        number: u32,
        data: ImageData,
        pinned: &HashSet<ImageKey>,
    ) -> Result<Stored, InsertError> {
        debug_assert_ne!(key, ImageKey::NONE, "ImageKey::NONE is never stored");
        let client_id = match (client_id, number) {
            (0, 0) => 0,
            (0, _) => self.lowest_free_client_id(),
            (id, _) => id,
        };
        // Check the quota before touching anything, so a rejected upload
        // leaves the old image in place.
        let need = self.registry.cost(&data);
        if need > self.registry.cap_bytes() {
            return Err(InsertError::TooLarge {
                need,
                cap: self.registry.cap_bytes(),
            });
        }
        let replaced = if client_id == 0 {
            None
        } else {
            self.by_client.get(&client_id).copied()
        };
        if let Some(old) = replaced {
            // Re-sending by `i=` keeps the id's numbers (#37): drop the old
            // bytes and key, but not the number membership.
            self.registry.remove(old);
            self.client_of.remove(&old);
        }
        let inserted = self.registry.insert_with_pinned(key, data, pinned)?;
        for victim in &inserted.evicted {
            self.forget_key(*victim);
        }
        self.client_of.insert(key, client_id);
        if client_id != 0 {
            self.by_client.insert(client_id, key);
        }
        if number != 0 && client_id != 0 {
            let ids = self.numbers.entry(number).or_default();
            if !ids.contains(&client_id) {
                ids.push(client_id);
            }
        }
        Ok(Stored {
            key,
            client_id,
            replaced,
            evicted: inserted.evicted,
        })
    }

    /// Key of the image answering to `client_id`, if any.
    #[must_use]
    pub fn key_of(&self, client_id: u32) -> Option<ImageKey> {
        if client_id == 0 {
            return None;
        }
        self.by_client.get(&client_id).copied()
    }

    /// Client id of the image stored under `key` (`0` = anonymous).
    #[must_use]
    pub fn client_of(&self, key: ImageKey) -> Option<u32> {
        self.client_of.get(&key).copied()
    }

    /// Client id of the newest live image transmitted with number `n`.
    #[must_use]
    pub fn client_of_number(&self, number: u32) -> Option<u32> {
        self.numbers
            .get(&number)?
            .iter()
            .rev()
            .copied()
            .find(|id| self.by_client.contains_key(id))
    }

    /// True iff an image answers to `client_id`.
    #[must_use]
    pub fn contains(&self, client_id: u32) -> bool {
        self.key_of(client_id).is_some()
    }

    /// The image answering to `client_id`.
    #[must_use]
    pub fn get(&self, client_id: u32) -> Option<&ImageData> {
        self.registry.get(self.key_of(client_id)?)
    }

    /// The image stored under `key`.
    #[must_use]
    pub fn get_key(&self, key: ImageKey) -> Option<&ImageData> {
        self.registry.get(key)
    }

    /// True iff `key` is stored.
    #[must_use]
    pub fn contains_key(&self, key: ImageKey) -> bool {
        self.registry.contains(key)
    }

    /// Mark `key` most recently used.
    pub fn touch(&mut self, key: ImageKey) {
        self.registry.touch(key);
    }

    /// Free the image answering to `client_id`. Returns its key.
    pub fn remove_client(&mut self, client_id: u32) -> Option<ImageKey> {
        let key = self.key_of(client_id)?;
        self.remove_key(key);
        Some(key)
    }

    /// Free the image stored under `key` (addressable or anonymous).
    /// Returns whether anything was stored there.
    pub fn remove_key(&mut self, key: ImageKey) -> bool {
        let had = self.registry.remove(key).is_some();
        self.forget_key(key);
        had
    }

    /// Free everything (RIS: kitty's `clear_graphics`, after which no id,
    /// number or byte survives).
    pub fn clear(&mut self) {
        let cap = self.registry.cap_bytes();
        *self = Self::new(cap);
    }

    /// Client ids in `lo..=hi` (anonymous images never match).
    #[must_use]
    pub fn clients_in(&self, lo: u32, hi: u32) -> Vec<u32> {
        let mut ids: Vec<u32> = self
            .by_client
            .keys()
            .copied()
            .filter(|id| (lo..=hi).contains(id))
            .collect();
        ids.sort_unstable();
        ids
    }

    /// Resize the quota; returns the evicted keys.
    pub fn set_cap(&mut self, cap_bytes: u64) -> Vec<ImageKey> {
        let evicted = self.registry.set_cap(cap_bytes);
        for key in &evicted {
            self.forget_key(*key);
        }
        evicted
    }

    /// The quota in bytes.
    #[must_use]
    pub fn cap_bytes(&self) -> u64 {
        self.registry.cap_bytes()
    }

    /// Bytes still free under the quota.
    #[must_use]
    pub fn budget_remaining(&self) -> u64 {
        self.registry.budget_remaining()
    }

    /// Number of stored images (anonymous included).
    #[must_use]
    pub fn len(&self) -> usize {
        self.registry.len()
    }

    /// True when nothing is stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.registry.is_empty()
    }

    /// `(client id, image)` in LRU order (least recently used first).
    /// Anonymous images appear with client id `0`.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &ImageData)> {
        self.registry
            .iter()
            .map(|(key, d)| (self.client_of.get(&key).copied().unwrap_or(0), d))
    }

    /// Every stored key.
    pub fn keys(&self) -> impl Iterator<Item = ImageKey> + '_ {
        self.registry.ids()
    }

    /// Drop the id / number bookkeeping for a key whose bytes are gone.
    fn forget_key(&mut self, key: ImageKey) {
        if let Some(id) = self.client_of.remove(&key)
            && id != 0
            && self.by_client.get(&id) == Some(&key)
        {
            self.by_client.remove(&id);
            self.numbers.retain(|_, ids| {
                ids.retain(|&x| x != id);
                !ids.is_empty()
            });
        }
    }

    /// Lowest client id no image answers to (kitty's
    /// `get_free_client_id`, used for `I=` without `i=`, and the id a
    /// program can be told to use next).
    #[must_use]
    pub fn lowest_free_client_id(&self) -> u32 {
        let mut id = 1u32;
        while self.by_client.contains_key(&id) {
            id += 1;
        }
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What one `img` costs against the quota (16 pixel bytes + overhead).
    const COST: u64 = 16 + IMAGE_OVERHEAD_BYTES;

    fn img(fill: u8) -> ImageData {
        ImageData {
            width: 2,
            height: 2,
            pixels: vec![fill; 16],
        }
    }

    fn k(n: u32) -> ImageKey {
        ImageKey(n)
    }

    fn none() -> HashSet<ImageKey> {
        HashSet::new()
    }

    #[test]
    fn explicit_id_replaces_and_reports_the_old_key() {
        let mut s = ImageStore::new(1 << 20);
        let a = s.insert(k(10), 7, 0, img(1), &none()).unwrap();
        assert_eq!((a.key, a.client_id, a.replaced), (k(10), 7, None));
        let b = s.insert(k(11), 7, 0, img(2), &none()).unwrap();
        assert_eq!(b.replaced, Some(k(10)));
        assert_eq!(s.key_of(7), Some(k(11)));
        assert!(!s.contains_key(k(10)));
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn anonymous_images_are_not_addressable() {
        // PLC-4: kitty keeps them out of the id space entirely.
        let mut s = ImageStore::new(1 << 20);
        let a = s.insert(k(5), 0, 0, img(1), &none()).unwrap();
        assert_eq!(a.client_id, 0);
        assert!(!s.contains(0));
        assert!(s.clients_in(0, u32::MAX).is_empty());
        // A client id that happens to equal the key is a different image.
        s.insert(k(6), 5, 0, img(2), &none()).unwrap();
        assert!(s.contains_key(k(5)), "anonymous image untouched");
        assert_eq!(s.remove_client(5), Some(k(6)));
        assert!(s.contains_key(k(5)));
        assert_eq!(s.client_of(k(5)), Some(0));
    }

    #[test]
    fn number_only_gets_the_lowest_free_client_id() {
        // kitty-ref: I=5 -> i=1, I=6 -> i=2, (i=1 explicit), I=7 -> i=3.
        let mut s = ImageStore::new(1 << 20);
        assert_eq!(s.lowest_free_client_id(), 1);
        assert_eq!(s.insert(k(1), 0, 5, img(1), &none()).unwrap().client_id, 1);
        assert_eq!(s.insert(k(2), 0, 6, img(1), &none()).unwrap().client_id, 2);
        s.insert(k(3), 1, 0, img(1), &none()).unwrap();
        assert_eq!(s.insert(k(4), 0, 7, img(1), &none()).unwrap().client_id, 3);
        assert_eq!(s.client_of_number(7), Some(3));
        // i=1 re-sent the I=5 image; it keeps number 5 (#37's rule,
        // checked on kitty-ref 0.49.2: `a=t,I=5` -> i=1, `a=t,i=1`, then
        // `a=p,I=5` replies `i=1,I=5;OK`).
        assert_eq!(s.client_of_number(5), Some(1));
        s.remove_client(3);
        assert_eq!(s.client_of_number(7), None);
        assert_eq!(s.lowest_free_client_id(), 3);
    }

    #[test]
    fn eviction_and_cap_changes_forget_ids() {
        let mut s = ImageStore::new(2 * COST);
        s.insert(k(1), 1, 0, img(1), &none()).unwrap();
        s.insert(k(2), 2, 0, img(1), &none()).unwrap();
        let c = s.insert(k(3), 3, 0, img(1), &none()).unwrap();
        assert_eq!(c.evicted, vec![k(1)]);
        assert!(!s.contains(1));
        assert_eq!(s.set_cap(COST), vec![k(2)]);
        assert!(!s.contains(2));
        assert!(s.contains(3));
    }

    #[test]
    fn too_large_leaves_the_old_image() {
        let mut s = ImageStore::new(COST);
        s.insert(k(1), 7, 0, img(1), &none()).unwrap();
        let big = ImageData {
            width: 4,
            height: 4,
            pixels: vec![0; 64],
        };
        assert!(s.insert(k(2), 7, 0, big, &none()).is_err());
        assert_eq!(s.key_of(7), Some(k(1)));
    }

    #[test]
    fn quota_charges_a_per_image_overhead() {
        // A quota of 100 KiB fits 25 600 1x1 images by pixel bytes alone;
        // with the overhead it holds at most cap / (4 + overhead), and
        // the oldest are evicted to make room.
        let cap = 100 * 1024;
        let mut s = ImageStore::new(cap);
        let tiny = || ImageData {
            width: 1,
            height: 1,
            pixels: vec![0; 4],
        };
        for n in 1..=1000 {
            s.insert(k(n), n, 0, tiny(), &none()).unwrap();
        }
        let max = usize::try_from(cap / (4 + IMAGE_OVERHEAD_BYTES)).unwrap();
        assert_eq!(s.len(), max);
        assert!(s.contains(1000));
        assert!(!s.contains(1), "oldest evicted");
        assert!(s.registry().total_bytes() <= cap);
        // Even a zero-pixel image costs the overhead.
        let empty = ImageData {
            width: 0,
            height: 0,
            pixels: Vec::new(),
        };
        let mut small = ImageStore::new(IMAGE_OVERHEAD_BYTES - 1);
        assert!(small.insert(k(1), 1, 0, empty, &none()).is_err());
    }

    #[test]
    fn clear_frees_everything() {
        let mut s = ImageStore::new(1 << 20);
        s.insert(k(1), 1, 5, img(1), &none()).unwrap();
        s.insert(k(2), 0, 0, img(1), &none()).unwrap();
        s.clear();
        assert!(s.is_empty());
        assert!(!s.contains(1));
        assert_eq!(s.client_of_number(5), None);
        assert_eq!(s.cap_bytes(), 1 << 20);
        assert_eq!(s.lowest_free_client_id(), 1);
    }

    #[test]
    fn iter_reports_client_ids_in_lru_order() {
        let mut s = ImageStore::new(1 << 20);
        s.insert(k(100), 2, 0, img(1), &none()).unwrap();
        s.insert(k(101), 0, 0, img(1), &none()).unwrap();
        s.insert(k(102), 9, 0, img(1), &none()).unwrap();
        s.touch(k(100));
        let ids: Vec<u32> = s.iter().map(|(id, _)| id).collect();
        assert_eq!(ids, vec![0, 9, 2]);
    }
}
