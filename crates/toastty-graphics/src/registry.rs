//! `ImageRegistry`: LRU-bounded cache of decoded image bytes keyed by id.
//!
//! Architecture choice (M11a, pre-approved): texture-per-image — the
//! atlas is per-image GPU texture rather than a packed mega-atlas. This
//! registry tracks the *decoded CPU bytes*; the renderer's
//! `ImageTextureCache` mirrors entries onto the GPU.
//!
//! Storage: `HashMap<key, Entry>`, plus an LRU `VecDeque` for eviction
//! order. A screen's `ImageStore` keys it by [`ImageKey`]; a plain `u32`
//! works too (tests, standalone use). The cap is in bytes (configured by
//! the binary based on free memory / a fixed budget).
//!
//! Animated images (kitty `a=f` / `a=a` / `a=c`) also hold their frames
//! ([`crate::frames::Animation`]), in their own budget. For those, the
//! [`ImageData`] that [`ImageRegistry::get`] returns is the frame on
//! display, so every placement and placeholder draws the current frame
//! without knowing about animation.
//!
//! Identifier policy:
//! - Caller supplies an explicit id (Kitty `i=`, or a store key).
//! - If `0`, the registry assigns the lowest unused 1-based id.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use crate::frames::{Animation, FrameError, Playback, PreparedFrame};
use crate::kitty::header::{AnimationKeys, CompositionKeys, FrameKeys};
use crate::kitty::reply::ErrorCode;

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

/// One resident image.
#[derive(Debug)]
struct Entry {
    /// The image as displayed: the transmitted pixels, or for an
    /// animated image the current frame, composed.
    image: ImageData,
    /// Frames and playback state, once the image has frames (or an
    /// `a=c`).
    anim: Option<Box<Animation>>,
    /// `a=a` settings given while the image had no frames yet.
    playback: Playback,
    /// Unique stamp of `image`'s content; see
    /// [`ImageRegistry::generation`].
    generation: u64,
}

impl Entry {
    /// Bytes counted against the frame budget: the stored frames.
    fn frame_bytes(&self) -> u64 {
        self.anim.as_ref().map_or(0, |a| a.byte_size())
    }
}

/// Frames of different animations due within this long of each other are
/// shown together, so N animations don't cost N redraws per period
/// (kitty batches repaints the same way, `repaint_delay`).
pub const ANIMATION_SLACK: std::time::Duration = std::time::Duration::from_millis(10);

/// Animation frames get their own budget, this many times the image cap
/// (kitty: a disk cache of 5×). Frames are stored LZ4-compressed, so the
/// budget holds several times its size in frames.
pub const FRAME_BUDGET_FACTOR: u64 = 2;

/// LRU-bounded cache of decoded images.
#[derive(Debug)]
pub struct ImageRegistry<K: RegistryKey = u32> {
    entries: HashMap<K, Entry>,
    /// Eviction order. Most recently used at the back.
    lru: VecDeque<K>,
    /// Sum of each entry's cost (displayed pixels plus `overhead`; frames
    /// not included).
    total_bytes: u64,
    /// Soft cap. Insertions evict until `total_bytes + new <= cap_bytes`.
    cap_bytes: u64,
    /// Bytes charged per image on top of its pixels, so millions of tiny
    /// images can't fit a quota that only counts pixels (each entry costs
    /// real memory in the maps and the LRU). `0` unless set.
    overhead: u64,
    /// Sum of [`Entry::frame_bytes`] across `entries`.
    frame_bytes: u64,
    /// Cap on `frame_bytes`: [`FRAME_BUDGET_FACTOR`] × `cap_bytes`.
    frame_cap_bytes: u64,
    /// Monotonic content version. Bumps on every successful insert /
    /// remove, and whenever an animated image's displayed frame changes.
    /// The renderer compares this to its cached value to decide when to
    /// re-sync GPU textures.
    revision: u32,
    /// Next [`Entry::generation`] to hand out. Starts at 1, never reused.
    next_generation: u64,
    /// Ids sent with the transient usage hint (`N=1`). Under storage
    /// pressure they are evicted before other images without placements
    /// (spec "Usage hints"; §9).
    transient: HashSet<K>,
}

/// What [`ImageRegistry::add_frame`] and [`ImageRegistry::compose_frames`]
/// did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameChange<K = u32> {
    /// 1-based frame created or edited (the reply's `r=`).
    pub frame: u32,
    /// True if the displayed frame's pixels changed.
    pub display_changed: bool,
    /// Ids evicted to make room (oldest first).
    pub evicted: Vec<K>,
}

/// Outcome of [`ImageRegistry::delete_frame`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameDelete {
    /// No such image.
    NotFound,
    /// The image has only its root frame; nothing was deleted.
    OnlyFrame,
    /// A frame was deleted.
    Deleted {
        /// True if the displayed frame's pixels changed.
        display_changed: bool,
    },
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
            frame_bytes: 0,
            frame_cap_bytes: cap_bytes.saturating_mul(FRAME_BUDGET_FACTOR),
            revision: 0,
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

    /// Total bytes charged for resident images: displayed pixels plus
    /// per-image overhead (frames not included).
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Bytes of stored animation frames (compressed), root copies
    /// included.
    #[must_use]
    pub fn frame_bytes(&self) -> u64 {
        self.frame_bytes
    }

    /// Cap on [`Self::frame_bytes`].
    #[must_use]
    pub fn frame_cap_bytes(&self) -> u64 {
        self.frame_cap_bytes
    }

    /// Drop `id` and `e`'s bytes from the totals (it left the registry).
    fn forget_bytes(&mut self, id: K, e: &Entry) {
        self.total_bytes -= self.cost(&e.image);
        self.frame_bytes -= e.frame_bytes();
        self.transient.remove(&id);
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
    /// remove, and when an animated image shows a new frame.
    #[must_use]
    pub fn revision(&self) -> u32 {
        self.revision
    }

    /// True iff `id` is resident.
    #[must_use]
    pub fn contains(&self, id: K) -> bool {
        self.entries.contains_key(&id)
    }

    /// Borrow an image by id: for an animated image, its current frame.
    /// Does NOT touch the LRU.
    #[must_use]
    pub fn get(&self, id: K) -> Option<&ImageData> {
        self.entries.get(&id).map(|e| &e.image)
    }

    /// A stamp of what [`Self::get`] returns for `id`: unique per insert
    /// (a re-transmit under the same id, same size and pixels or not,
    /// gets a new one) and per change of the displayed frame of an
    /// animated image. `None` if not resident. The renderer keys GPU
    /// textures on it.
    #[must_use]
    pub fn generation(&self, id: K) -> Option<u64> {
        self.entries.get(&id).map(|e| e.generation)
    }

    /// Borrow an image by id and mark it MRU.
    pub fn touch(&mut self, id: K) -> Option<&ImageData> {
        if !self.entries.contains_key(&id) {
            return None;
        }
        self.bump_to_back(id);
        self.entries.get(&id).map(|e| &e.image)
    }

    /// Iterate (id, image) in MRU order (oldest first).
    pub fn iter(&self) -> impl Iterator<Item = (K, &ImageData)> {
        self.lru
            .iter()
            .copied()
            .filter_map(|id| self.entries.get(&id).map(|e| (id, &e.image)))
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
    /// Replacing an animated image drops its frames, as in kitty.
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
        // uses post-replace totals. A usage hint belongs to the data it
        // was sent with, so it goes too.
        if let Some(old) = self.entries.remove(&final_id) {
            self.forget_bytes(final_id, &old);
            // Drop the stale LRU entry; we'll push the fresh one below.
            self.lru.retain(|x| *x != final_id);
        }
        self.transient.remove(&final_id);

        // Evict until we have room. Don't ever evict the id we're about
        // to insert (removed above so it's no longer in `lru`).
        //
        // m6: a single LRU pop would evict an actively-displayed image
        // even when an unplaced one is available. Instead pick the LRU
        // victim from the *unpinned* images first; only when none remain
        // fall back to the strict LRU front (which may be pinned).
        let evicted = self.evict_for(Budget::Images, need, None, pinned, true);

        self.total_bytes += need;
        self.lru.push_back(final_id);
        let generation = self.stamp();
        self.entries.insert(
            final_id,
            Entry {
                image: data,
                anim: None,
                playback: Playback::default(),
                generation,
            },
        );
        self.revision = self.revision.wrapping_add(1);
        Ok(Inserted {
            id: final_id,
            evicted,
        })
    }

    /// Whether `need` more bytes fit in `budget`.
    fn fits(&self, budget: Budget, need: u64) -> bool {
        match budget {
            Budget::Images => self.total_bytes + need <= self.cap_bytes,
            Budget::Frames => self.frame_bytes + need <= self.frame_cap_bytes,
        }
    }

    /// Evict LRU entries other than `keep` until `need` more bytes fit
    /// in `budget`: unpinned transient images first (`N=1`), then other
    /// unpinned ones. With `allow_pinned` false, pinned images are never
    /// evicted (and the cap may stay exceeded). For the frame budget only
    /// animated images are candidates. Returns the evicted ids, oldest
    /// first.
    fn evict_for(
        &mut self,
        budget: Budget,
        need: u64,
        keep: Option<K>,
        pinned: &HashSet<K>,
        allow_pinned: bool,
    ) -> Vec<K> {
        let mut evicted = Vec::new();
        let frees = |r: &Self, id: K| {
            budget == Budget::Images || r.entries.get(&id).is_some_and(|e| e.frame_bytes() > 0)
        };
        while !self.fits(budget, need) {
            let candidate = |r: &Self, x: K| Some(x) != keep && !pinned.contains(&x) && frees(r, x);
            let victim = self
                .lru
                .iter()
                .position(|x| candidate(self, *x) && self.transient.contains(x))
                .or_else(|| self.lru.iter().position(|x| candidate(self, *x)))
                // Fall back to the strict LRU front if everything left is
                // pinned (we must still free space to honor the cap).
                .or_else(|| {
                    allow_pinned
                        .then(|| self.lru.iter().position(|x| Some(*x) != keep))
                        .flatten()
                });
            let Some(at) = victim else {
                // Nothing (left) to evict; for an insert the cap check
                // guarantees the new image fits, so this is defensive.
                break;
            };
            // O(1) for the usual case, the front.
            let Some(victim) = self.lru.remove(at) else {
                break;
            };
            if let Some(victim_data) = self.entries.remove(&victim) {
                self.forget_bytes(victim, &victim_data);
                evicted.push(victim);
            }
        }
        if !evicted.is_empty() {
            self.revision = self.revision.wrapping_add(1);
        }
        evicted
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
        let entry = self.entries.remove(&id)?;
        self.forget_bytes(id, &entry);
        self.lru.retain(|x| *x != id);
        self.revision = self.revision.wrapping_add(1);
        Some(entry.image)
    }

    /// Iterate every resident id.
    pub fn ids(&self) -> impl Iterator<Item = K> + '_ {
        self.entries.keys().copied()
    }

    /// Resize the byte cap (and the frame budget with it). If either is
    /// exceeded, evicts LRU entries until both are satisfied. Returns the
    /// evicted ids.
    pub fn set_cap(&mut self, new_cap: u64) -> Vec<K> {
        self.cap_bytes = new_cap;
        self.frame_cap_bytes = new_cap.saturating_mul(FRAME_BUDGET_FACTOR);
        let mut evicted = Vec::new();
        while self.total_bytes > self.cap_bytes {
            let Some(victim) = self.lru.pop_front() else {
                break;
            };
            if let Some(data) = self.entries.remove(&victim) {
                self.forget_bytes(victim, &data);
                evicted.push(victim);
                self.revision = self.revision.wrapping_add(1);
            }
        }
        evicted.extend(self.evict_for(Budget::Frames, 0, None, &HashSet::new(), true));
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

    fn stamp(&mut self) -> u64 {
        let g = self.next_generation;
        self.next_generation += 1;
        g
    }

    /// Record that `id`'s displayed pixels changed.
    fn display_changed(&mut self, id: K) {
        let generation = self.stamp();
        if let Some(e) = self.entries.get_mut(&id) {
            e.generation = generation;
        }
        self.revision = self.revision.wrapping_add(1);
    }

    /// Re-sum `id`'s frame bytes after its frames changed (they were
    /// `before`). Frame commands never change the displayed image's size.
    fn reaccount(&mut self, id: K, before: u64) {
        if let Some(e) = self.entries.get(&id) {
            self.frame_bytes = self.frame_bytes - before + e.frame_bytes();
        }
    }

    /// Give `id` an animation, if it has none yet: its root frame is a
    /// (compressed) copy of the stored image, charged to the frame budget
    /// like any frame, evicting as [`Self::add_frame`] does. Fails with
    /// ENOSPC (`enospc`) when the copy doesn't fit.
    fn ensure_animation(
        &mut self,
        id: K,
        pinned: &HashSet<K>,
        enospc: &str,
    ) -> Result<Vec<K>, FrameError> {
        let Some(e) = self.entries.get(&id) else {
            return Err(not_found(id.into()));
        };
        if e.anim.is_some() {
            return Ok(Vec::new());
        }
        let anim = Animation::new(&e.image, &e.playback);
        let cost = anim.byte_size();
        let evicted = self.evict_for(Budget::Frames, cost, Some(id), pinned, false);
        if !self.fits(Budget::Frames, cost) {
            return Err(FrameError {
                code: ErrorCode::Enospc,
                detail: enospc.to_string(),
            });
        }
        self.frame_bytes += cost;
        self.entries.get_mut(&id).expect("resident").anim = Some(Box::new(anim));
        Ok(evicted)
    }

    /// Number of frames of `id`, root included (1 for a still image).
    /// `None` if not resident.
    #[must_use]
    pub fn frame_count(&self, id: K) -> Option<u32> {
        let e = self.entries.get(&id)?;
        Some(e.anim.as_ref().map_or(1, |a| a.frame_count()))
    }

    /// The animation state of `id`, if it has had an animation command.
    #[must_use]
    pub fn animation(&self, id: K) -> Option<&Animation> {
        self.entries.get(&id)?.anim.as_deref()
    }

    /// `a=f`: add or edit a frame of `id` from the decoded `data`. The
    /// frame's bytes count against the cap: room is made by evicting
    /// images not in `pinned` (never `id` itself or a pinned image), and
    /// failing that the command fails with kitty's ENOSPC.
    pub fn add_frame(
        &mut self,
        id: K,
        keys: &FrameKeys,
        data: ImageData,
        pinned: &HashSet<K>,
    ) -> Result<FrameChange<K>, FrameError> {
        let count = self.frame_count(id).ok_or_else(|| not_found(id.into()))?;
        let enospc = if keys.edit_frame == 0 || keys.edit_frame > count {
            "Cache size exceeded cannot add new frames"
        } else {
            "Failed to cache data for image frame"
        };
        let mut evicted = self.ensure_animation(id, pinned, enospc)?;
        let anim = self.entries[&id].anim.as_ref().expect("just created");
        let prepared = anim.prepare_frame(keys, data)?;
        let mut change = self.commit_prepared(id, prepared, pinned, enospc)?;
        evicted.append(&mut change.evicted);
        change.evicted = evicted;
        Ok(change)
    }

    /// `a=c`: compose a rectangle of one frame of `id` onto another.
    /// `client_id` is the image's id as the client knows it, for kitty's
    /// error messages.
    pub fn compose_frames(
        &mut self,
        id: K,
        client_id: u32,
        keys: &CompositionKeys,
        pinned: &HashSet<K>,
    ) -> Result<FrameChange<K>, FrameError> {
        let enospc = "Failed to cache data for image frame";
        let mut evicted = self.ensure_animation(id, pinned, enospc)?;
        let anim = self.entries[&id].anim.as_ref().expect("just created");
        let prepared = anim.prepare_compose(keys, client_id)?;
        let mut change = self.commit_prepared(id, prepared, pinned, enospc)?;
        evicted.append(&mut change.evicted);
        change.evicted = evicted;
        Ok(change)
    }

    fn commit_prepared(
        &mut self,
        id: K,
        prepared: PreparedFrame,
        pinned: &HashSet<K>,
        enospc: &str,
    ) -> Result<FrameChange<K>, FrameError> {
        let entry = &self.entries[&id];
        let before = entry.frame_bytes();
        let growth = entry.anim.as_ref().expect("animated").growth(&prepared);
        let mut evicted = Vec::new();
        if growth > 0 {
            let growth = growth as u64;
            evicted = self.evict_for(Budget::Frames, growth, Some(id), pinned, false);
            if !self.fits(Budget::Frames, growth) {
                return Err(FrameError {
                    code: ErrorCode::Enospc,
                    detail: enospc.to_string(),
                });
            }
        }
        let frame = prepared.number;
        let entry = self.entries.get_mut(&id).expect("resident");
        let anim = entry.anim.as_mut().expect("animated");
        let display_changed = anim.commit(prepared, &mut entry.image);
        self.reaccount(id, before);
        if display_changed {
            self.display_changed(id);
        }
        Ok(FrameChange {
            frame,
            display_changed,
            evicted,
        })
    }

    /// `a=a`: control `id`'s animation. Returns true if the displayed
    /// frame changed. A no-op for an image that isn't resident. On a
    /// still image only the settings are kept; nothing is allocated.
    pub fn control_animation(&mut self, id: K, keys: &AnimationKeys) -> bool {
        let Some(entry) = self.entries.get_mut(&id) else {
            return false;
        };
        let Some(anim) = entry.anim.as_mut() else {
            entry.playback.apply(keys);
            return false;
        };
        let before = anim.byte_size();
        let changed = anim.control(keys, &mut entry.image);
        self.reaccount(id, before);
        if changed {
            self.display_changed(id);
        }
        changed
    }

    /// `d=f` / `d=F`: delete frame `number` of `id` (see
    /// [`Animation::delete_frame`]). An image with only its root frame is
    /// left alone; the caller decides what that means.
    pub fn delete_frame(&mut self, id: K, number: u32) -> FrameDelete {
        let Some(e) = self.entries.get_mut(&id) else {
            return FrameDelete::NotFound;
        };
        let before = e.frame_bytes();
        let Some(anim) = e.anim.as_mut().filter(|a| a.frame_count() > 1) else {
            return FrameDelete::OnlyFrame;
        };
        let display_changed = anim.delete_frame(number, &mut e.image);
        self.reaccount(id, before);
        if display_changed {
            self.display_changed(id);
        }
        FrameDelete::Deleted { display_changed }
    }

    /// Advance every running animation whose image `is_drawn` to `now`.
    /// Returns the ids whose displayed frame changed, and when the next
    /// frame of any of them is due (`None`: nothing is running).
    pub fn tick_animations(
        &mut self,
        now: Instant,
        is_drawn: impl Fn(K) -> bool,
    ) -> (Vec<K>, Option<Instant>) {
        let mut changed = Vec::new();
        let mut next: Option<Instant> = None;
        for (&id, e) in &mut self.entries {
            let Some(anim) = e.anim.as_mut() else {
                continue;
            };
            if !anim.is_running() || !is_drawn(id) {
                continue;
            }
            let (c, due) = anim.tick(now, ANIMATION_SLACK, &mut e.image);
            if c {
                changed.push(id);
            }
            if let Some(due) = due {
                next = Some(next.map_or(due, |n| n.min(due)));
            }
        }
        for &id in &changed {
            self.display_changed(id);
        }
        (changed, next)
    }

    /// True if any image has a running animation (whether drawn or not).
    #[must_use]
    pub fn has_running_animation(&self) -> bool {
        self.entries
            .values()
            .any(|e| e.anim.as_ref().is_some_and(|a| a.is_running()))
    }
}

/// Which cap [`ImageRegistry::evict_for`] makes room under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Budget {
    /// Displayed images, against `cap_bytes`.
    Images,
    /// Stored animation frames, against `frame_cap_bytes`.
    Frames,
}

/// ENOENT for an animation command on an image that isn't resident. The
/// registry only sees its own key; callers that know the client id check
/// residency first and answer with it.
fn not_found(id: u32) -> FrameError {
    FrameError {
        code: ErrorCode::Enoent,
        detail: format!(
            "Animation command refers to non-existent image with id: {id} and number: 0"
        ),
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

    // ---- §9 animation frames ----------------------------------------

    use crate::frames::FRAME_OVERHEAD;

    fn frame_keys() -> FrameKeys {
        FrameKeys {
            x: 0,
            y: 0,
            base_frame: 0,
            edit_frame: 0,
            gap_ms: 0,
            blend_mode: 0,
            background: 0,
        }
    }

    fn show(n: u32) -> AnimationKeys {
        AnimationKeys {
            state: 0,
            frame: 0,
            gap_ms: 0,
            current_frame: n,
            loops: 0,
        }
    }

    #[test]
    fn frames_are_counted_and_get_returns_the_current_frame() {
        // Every frame is charged its pixels plus FRAME_OVERHEAD.
        let f = 16 + FRAME_OVERHEAD;
        let mut r = ImageRegistry::<u32>::new(64 * 1024);
        assert_eq!(r.frame_cap_bytes(), 64 * 1024 * FRAME_BUDGET_FACTOR);
        r.insert(1, img(2, 2, 1)).unwrap();
        assert_eq!(r.frame_count(1), Some(1));
        assert_eq!((r.total_bytes(), r.frame_bytes()), (16, 0));
        let change = r
            .add_frame(1, &frame_keys(), img(2, 2, 2), &HashSet::new())
            .unwrap();
        assert_eq!((change.frame, change.display_changed), (2, false));
        // The display stays under the image cap; root copy + frame 2 go
        // to the frame budget.
        assert_eq!((r.total_bytes(), r.frame_bytes()), (16, 2 * f));
        assert_eq!(r.get(1).unwrap().pixels[0], 1);
        let (rev, generation) = (r.revision(), r.generation(1));
        assert!(r.control_animation(1, &show(2)));
        assert_eq!(r.get(1).unwrap().pixels[0], 2);
        assert_ne!(r.revision(), rev);
        assert_ne!(r.generation(1), generation);
        // Deleting a frame gives its bytes back; replacing the image
        // drops all of them.
        assert_eq!(
            r.delete_frame(1, 2),
            FrameDelete::Deleted {
                display_changed: true
            }
        );
        assert_eq!(r.frame_bytes(), f);
        assert_eq!(r.delete_frame(1, 1), FrameDelete::OnlyFrame);
        r.insert(1, img(2, 2, 3)).unwrap();
        assert_eq!(r.frame_count(1), Some(1));
        assert_eq!((r.total_bytes(), r.frame_bytes()), (16, 0));
        assert_eq!(r.delete_frame(9, 1), FrameDelete::NotFound);
    }

    #[test]
    fn control_on_a_still_image_allocates_nothing_but_sticks() {
        // #47 review 8: a=a on a still image used to copy the root into
        // the frame budget without a fit check.
        let mut r = ImageRegistry::<u32>::new(64 * 1024);
        for id in 1..=50 {
            r.insert(id, img(2, 2, 1)).unwrap();
            let run = AnimationKeys {
                state: 3,
                frame: 1,
                gap_ms: 120,
                loops: 2,
                ..show(0)
            };
            assert!(!r.control_animation(id, &run));
        }
        assert_eq!(r.frame_bytes(), 0);
        assert!(r.animation(1).is_none());
        // The settings apply once frames arrive.
        r.add_frame(1, &frame_keys(), img(2, 2, 2), &HashSet::new())
            .unwrap();
        let a = r.animation(1).unwrap();
        assert_eq!(a.gap_ms(1), Some(120));
        assert_eq!(a.state(), crate::frames::AnimationState::Running);
        assert!(a.is_running());
    }

    #[test]
    fn the_root_copy_must_fit_the_frame_budget() {
        // A root copy (32 bytes + overhead) and one 1-pixel frame fit the
        // budget once, not twice.
        let per = (32 + FRAME_OVERHEAD) + (4 + FRAME_OVERHEAD);
        let mut r = ImageRegistry::<u32>::new(per * 3 / 2 / FRAME_BUDGET_FACTOR);
        r.insert(1, img(4, 2, 1)).unwrap();
        r.insert(2, img(4, 2, 2)).unwrap();
        let pinned: HashSet<u32> = [1, 2].into();
        let budget = r.frame_cap_bytes();
        assert!(r.add_frame(1, &frame_keys(), img(1, 1, 0), &pinned).is_ok());
        let err = r
            .add_frame(2, &frame_keys(), img(1, 1, 0), &pinned)
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Enospc);
        assert!(r.animation(2).is_none(), "no half-made animation");
        assert!(r.frame_bytes() <= budget, "{} > {budget}", r.frame_bytes());
    }

    #[test]
    fn frames_evict_unplaced_animations_but_never_placed_ones() {
        // Room for exactly four animations of root copy + one frame.
        let per = 2 * (16 + FRAME_OVERHEAD);
        let mut r = ImageRegistry::<u32>::new(4 * per / FRAME_BUDGET_FACTOR);
        assert_eq!(r.frame_cap_bytes(), 4 * per);
        for id in 1..=4 {
            r.insert(id, img(2, 2, 0)).unwrap();
            r.add_frame(id, &frame_keys(), img(2, 2, 1), &HashSet::new())
                .unwrap();
        }
        assert_eq!(r.frame_bytes(), 4 * per);
        // 1 and 3 are placed. A frame for 4 evicts the oldest unplaced
        // animation, 2; still images never count as frame victims.
        let pinned: HashSet<u32> = [1, 3].into();
        let change = r
            .add_frame(4, &frame_keys(), img(2, 2, 1), &pinned)
            .unwrap();
        assert_eq!(change.evicted, vec![2]);
        assert!(r.contains(1) && r.contains(3));
        // Fill what's left, then only placed animations remain: ENOSPC,
        // nothing changes.
        r.add_frame(4, &frame_keys(), img(2, 2, 1), &pinned)
            .unwrap();
        let before = r.frame_bytes();
        let err = r
            .add_frame(4, &frame_keys(), img(4, 2, 1), &pinned)
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Einval, "4x2 is wider than the image");
        let err = r
            .add_frame(4, &frame_keys(), img(2, 2, 1), &pinned)
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Enospc);
        assert_eq!(err.detail, "Cache size exceeded cannot add new frames");
        assert_eq!(r.frame_bytes(), before);
        assert_eq!(r.frame_count(4), Some(4));
        assert!(r.contains(1) && r.contains(3));
        // Frames never push out images: the image budget is untouched.
        assert_eq!(r.total_bytes(), 3 * 16);
    }

    #[test]
    fn tiny_frames_are_charged_their_overhead() {
        // #47 review 3: a flood of 1-pixel frames is bounded by the
        // budget through the per-frame charge, not by its 4 bytes each.
        let mut r = ImageRegistry::<u32>::new(10_000);
        r.insert(1, img(1, 1, 0)).unwrap();
        let tiny = img(1, 1, 1);
        let mut added = 0u64;
        while r
            .add_frame(1, &frame_keys(), tiny.clone(), &HashSet::new())
            .is_ok()
        {
            added += 1;
            assert!(added < 1000, "the budget must stop this flood");
        }
        let per = 4 + FRAME_OVERHEAD;
        assert_eq!(added, r.frame_cap_bytes() / per - 1, "root copy + frames");
        assert!(r.frame_bytes() <= r.frame_cap_bytes());
    }

    #[test]
    fn set_cap_shrinks_the_frame_budget_too() {
        let f = 16 + FRAME_OVERHEAD;
        let mut r = ImageRegistry::<u32>::new(64 * 1024);
        r.insert(1, img(2, 2, 0)).unwrap();
        for _ in 0..4 {
            r.add_frame(1, &frame_keys(), img(2, 2, 1), &HashSet::new())
                .unwrap();
        }
        r.insert(2, img(2, 2, 0)).unwrap();
        // Root copy + 4 frames. A cap that still holds both images but
        // whose frame budget doesn't hold the frames evicts the animated
        // image.
        assert_eq!(r.frame_bytes(), 5 * f);
        let cap = (4 * f / FRAME_BUDGET_FACTOR).max(32);
        let evicted = r.set_cap(cap);
        assert_eq!(evicted, vec![1]);
        assert_eq!((r.total_bytes(), r.frame_bytes()), (16, 0));
    }

    #[test]
    fn tick_skips_images_that_are_not_drawn_and_batches_near_frames() {
        let mut r = ImageRegistry::<u32>::new(64 * 1024);
        r.insert(1, img(2, 2, 1)).unwrap();
        r.add_frame(1, &frame_keys(), img(2, 2, 2), &HashSet::new())
            .unwrap();
        let run = AnimationKeys {
            state: 3,
            frame: 1,
            gap_ms: 50,
            ..show(0)
        };
        r.control_animation(1, &run);
        assert!(r.has_running_animation());
        let now = Instant::now();
        assert_eq!(r.tick_animations(now, |_| false), (vec![], None));
        let (changed, next) = r.tick_animations(now, |_| true);
        assert!(changed.is_empty());
        assert_eq!(next, Some(now + std::time::Duration::from_millis(50)));
        // Due within ANIMATION_SLACK: shown now, with the next frame
        // scheduled from when this one was due (no drift).
        let early = now + std::time::Duration::from_millis(45);
        assert_eq!(r.tick_animations(early, |_| true).0, vec![1]);
        assert_eq!(r.get(1).unwrap().pixels[0], 2);
        let (_, next) = r.tick_animations(early, |_| true);
        assert_eq!(
            next,
            Some(now + std::time::Duration::from_millis(50 + 40)),
            "frame 2 has the default 40 ms gap"
        );
    }
}
