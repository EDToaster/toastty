//! `ImageGrid`: per-screen parallel structure tracking image placements.
//!
//! Architecture choice (M11a, pre-approved): we keep image placements
//! *parallel* to the cell grid rather than folding them into `Cell`. This
//! avoids bloating `Cell` (40 bytes already) and keeps text mutations
//! (the hot path) cheap.
//!
//! A `Placement` reserves a rectangle of cells and shows a slice
//! (sub-rect) of a single image in it. Multiple placements can overlap;
//! render order is broken by `z` then insertion order. `z >= 0` draws
//! above text, `z < 0` draws below.
//!
//! ## Anchoring (see `docs/decisions/image-placement-model.md`)
//!
//! A placement is anchored to an **absolute line id** (`Placement::line`)
//! in the line-id space of the screen's `Grid` (`Grid::bottom_id`), not to
//! a screen row. A full-screen scroll changes which line ids are on
//! screen, so placements ride along with their text into scrollback for
//! free: no per-scroll work, and nothing is ever squashed. Only scrolls
//! that move text *without* moving line ids (DECSTBM margin scrolls,
//! IL/DL) touch placements, via [`ImageGrid::scroll_region`].
//!
//! The reserved cell box (`rows` × `cols`) is never trimmed. Anything
//! that falls outside the window is cropped at draw time by the
//! renderer. The one permanent crop is [`Placement::clip_rows`], which
//! DECSTBM margin scrolls narrow so an image scrolled against a margin
//! stays cut there (kitty does the same).
//!
//! Lookups are linear today (`O(n)` per query). Image placements are
//! few, so this is pragmatic. If it ever shows up in profiling, swap for
//! an interval tree.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

/// A single image placement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    /// Source image id (matches `ImageRegistry`).
    pub image_id: u32,
    /// Stable per-image placement id. Kitty's `p=` field; zero means
    /// "unnamed".
    pub placement_id: u32,
    /// Absolute line id of the top row of the reserved cell box, in the
    /// owning screen's `Grid` line-id space (see `Grid::bottom_id`).
    /// Signed so a box whose top hangs above the oldest line (or above
    /// line 0) is representable. The screen row is
    /// `line - top_line`, where `top_line` is the id of screen row 0.
    pub line: i64,
    /// Left column of the reserved cell box. Signed: a relative child
    /// with a negative `H=` offset may start left of column 0 and is
    /// cropped at draw time, as in kitty (REL-6). Ordinary placements are
    /// always `>= 0`.
    pub col: i32,
    /// Height of the reserved cell box, in rows. Never trimmed.
    pub rows: u16,
    /// Width of the reserved cell box, in columns. Never trimmed: a box
    /// wider than the screen is cropped at draw time.
    pub cols: u16,
    /// Rows of the box still shown, relative to [`Self::line`]
    /// (`0..rows` = unclipped). DECSTBM margin scrolls narrow this so an
    /// image pushed against a margin is permanently cut there; nothing
    /// else touches it.
    pub clip_rows: Range<u16>,
    /// Sub-rect of the source image to show, in source pixels.
    /// Stored as `(x, y, w, h)`. Use `full_image` to cover the whole
    /// image (the renderer interprets `w == 0 || h == 0` as "full").
    pub src_rect: SrcRect,
    /// Z order. `z >= 0` renders above text; `z < 0` renders below.
    /// Ties broken by insertion order.
    pub z: i32,
    /// M3: intra-cell pixel offset `(x, y)` within the FIRST cell at
    /// which to start displaying the image. Kitty's `X=` / `Y=` keys:
    /// "the x/y offset within the first cell at which to start
    /// displaying the image" (must be smaller than the cell size). This
    /// is a sub-cell pixel shift applied at render time on top of the
    /// cell-aligned position derived from `line` / `col`. It does NOT
    /// change which cells the placement occupies. Defaults to `(0, 0)`.
    pub pix_offset: (u32, u32),
    /// M13: parent linkage for a *relative placement*. `Some((image_id,
    /// placement_id))` when this placement is positioned relative to
    /// another placement (kitty's `P=`/`Q=` keys); `None` for an
    /// ordinary absolute placement. When set, this placement's
    /// `line`/`col` are resolved from the parent's origin plus
    /// [`Self::rel_offset`], and re-resolved whenever the parent moves.
    pub parent: Option<(u32, u32)>,
    /// The parent placement this child is bound to, resolved **once**
    /// when it is created (kitty: `P=` without `Q=` picks the parent
    /// image's oldest placement at that moment and keeps it). Positions,
    /// cycle checks and the delete cascade all follow this handle, never
    /// [`Self::parent`]'s ids again. `None` for an ordinary placement, and
    /// for a child of a Unicode-placeholder parent, which is a group of
    /// tiles rather than one placement and is found by ids.
    // TODO(#28): placeholder parents stop being tiles there.
    pub parent_ref: Option<PlacementHandle>,
    /// M13: `(cols, rows)` cell offset from the parent placement's origin
    /// (kitty's `H=`/`V=`). Only meaningful when [`Self::parent`] is
    /// `Some`. Signed. Defaults to `(0, 0)`.
    pub rel_offset: (i32, i32),
    /// Set on the per-cell tiles the Unicode-placeholder pipeline makes.
    /// Those are text: IL/DL move them with their cells, unlike ordinary
    /// placements.
    // TODO(#28): delete once placeholders are resolved from the cells at
    // render time and no longer live in `ImageGrid`.
    pub from_placeholder: bool,
}

impl Placement {
    /// A placement of `image_id` reserving `rows` × `cols` cells,
    /// anchored at line 0, column 0, with every other field at its
    /// default (unnamed, full image, `z = 0`, no offset, no parent). The
    /// host rebases `line`/`col` onto the cursor.
    #[must_use]
    pub fn new(image_id: u32, rows: u16, cols: u16) -> Self {
        Self {
            image_id,
            placement_id: 0,
            line: 0,
            col: 0,
            rows,
            cols,
            clip_rows: 0..rows,
            src_rect: SrcRect::FULL,
            z: 0,
            pix_offset: (0, 0),
            parent: None,
            parent_ref: None,
            rel_offset: (0, 0),
            from_placeholder: false,
        }
    }

    /// Screen columns of the reserved box that are at or right of column
    /// 0 (the box may still extend past the right edge). Empty when the
    /// whole box lies left of the screen.
    #[must_use]
    pub fn col_range(&self) -> Range<u16> {
        let clamp = |c: i64| u16::try_from(c.clamp(0, i64::from(u16::MAX))).unwrap_or(u16::MAX);
        let start = i64::from(self.col);
        clamp(start)..clamp(start + i64::from(self.cols))
    }

    /// Absolute line ids of the rows still shown (the reserved box minus
    /// any margin clip).
    #[must_use]
    pub fn shown_lines(&self) -> Range<i64> {
        self.line + i64::from(self.clip_rows.start)..self.line + i64::from(self.clip_rows.end)
    }

    /// Screen rows of the rows still shown, given `top_line` (the line id
    /// of screen row 0). Negative rows are in scrollback; rows `>=` the
    /// screen height hang below it.
    #[must_use]
    pub fn screen_rows(&self, top_line: i64) -> Range<i64> {
        let r = self.shown_lines();
        r.start - top_line..r.end - top_line
    }

    /// True iff the shown part covers screen cell `(row, col)`.
    #[must_use]
    pub fn covers(&self, top_line: i64, row: u16, col: u16) -> bool {
        self.on_row(top_line, row) && self.col_range().contains(&col)
    }

    /// True iff the shown part intersects screen row `row`.
    #[must_use]
    pub fn on_row(&self, top_line: i64, row: u16) -> bool {
        self.screen_rows(top_line).contains(&i64::from(row))
    }
}

/// Pixel rectangle on the source image, in image pixels.
///
/// `w == 0 || h == 0` means "use full image size" — the renderer needs
/// the image dimensions to materialize the full rect and we don't store
/// those on `Placement`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SrcRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl SrcRect {
    /// Sentinel meaning "the full source image".
    pub const FULL: Self = Self {
        x: 0,
        y: 0,
        w: 0,
        h: 0,
    };

    /// True iff this is the "full image" sentinel.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.w == 0 || self.h == 0
    }
}

/// Opaque handle returned from `ImageGrid::add` for later removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PlacementHandle(pub u32);

/// Maximum depth of a relative-placement parent chain (M13). Reference
/// kitty caps the chain at a small depth to bound resolution work;
/// exceeding it yields `ETOODEEP`.
pub const MAX_RELATIVE_DEPTH: usize = 8;

/// Why a relative placement could not be created (M13). Mapped by the
/// kitty handler onto the corresponding protocol error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelativeError {
    /// The referenced parent image/placement does not exist → `ENOPARENT`.
    NoParent,
    /// Linking to the parent would create a cycle → `ECYCLE`.
    Cycle,
    /// The parent chain is deeper than [`MAX_RELATIVE_DEPTH`] → `ETOODEEP`.
    TooDeep,
    /// An existing placement names itself as its parent → `EINVAL`
    /// (kitty: "Put command refers to itself as its own parent"). The same
    /// self-reference for a placement that doesn't exist yet is
    /// [`Self::NoParent`], again as in kitty.
    SelfParent,
}

/// Parallel image placement layer for one screen of a `Term`.
#[derive(Debug, Default)]
pub struct ImageGrid {
    placements: Vec<(PlacementHandle, Placement)>,
    next_handle: u32,
    /// Cached `(min shown end, max shown start)` line over all
    /// placements, so [`Self::retain_lines`] (run on every full-screen
    /// LF) is O(1) when nothing can be evicted. `None` = stale; every
    /// mutation clears it and the next `retain_lines` recomputes it.
    bounds: Option<(i64, i64)>,
}

impl ImageGrid {
    /// Fresh empty grid.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert `p`, returning a handle for future removal.
    pub fn add(&mut self, p: Placement) -> PlacementHandle {
        let handle = PlacementHandle(self.next_handle);
        self.next_handle = self.next_handle.wrapping_add(1);
        self.placements.push((handle, p));
        self.bounds = None;
        handle
    }

    /// Find the placement matching `(image_id, placement_id)`, if any.
    /// Used to resolve relative-placement parents (M13). `placement_id ==
    /// 0` (a parent reference with `P=` but no `Q=`, which kitty accepts)
    /// matches the image's oldest placement, whatever its id, so a child
    /// stays on the same parent when more placements of the image arrive.
    #[must_use]
    pub fn find(&self, image_id: u32, placement_id: u32) -> Option<&Placement> {
        self.placements.iter().map(|(_, p)| p).find(|p| {
            p.image_id == image_id && (placement_id == 0 || p.placement_id == placement_id)
        })
    }

    /// Create, or replace, a relative placement (M13).
    ///
    /// `child` carries its cell span in `rows` / `cols`; its `parent`
    /// and `rel_offset` fields drive positioning. Validation follows
    /// kitty 0.49.2:
    ///
    /// - naming itself as its parent is [`RelativeError::SelfParent`]
    ///   when the placement already exists, else
    ///   [`RelativeError::NoParent`];
    /// - the parent must exist ([`RelativeError::NoParent`]);
    /// - linking must not close a cycle ([`RelativeError::Cycle`]) or
    ///   push the chain past [`MAX_RELATIVE_DEPTH`]
    ///   ([`RelativeError::TooDeep`]).
    ///
    /// Everything is checked **before** anything changes, so a rejected
    /// re-placement leaves the old placement (and its children) in place
    /// (REL-1). On success the child's `line`/`col` are rebased onto the
    /// parent's origin plus `rel_offset`. A named child that already
    /// exists is replaced in place ([`Self::upsert`]), keeping its own
    /// children attached; they are re-resolved so they follow it.
    pub fn add_relative(&mut self, mut child: Placement) -> Result<PlacementHandle, RelativeError> {
        let (pimg, pplace) = child.parent.ok_or(RelativeError::NoParent)?;
        let me = (child.image_id, child.placement_id);
        let named = child.image_id != 0 && child.placement_id != 0;
        // The placement this command would replace, if any.
        let existing = if named {
            self.find_entry(me.0, me.1).map(|(h, _)| h)
        } else {
            None
        };
        if (pimg, pplace) == me {
            return Err(if existing.is_some() {
                RelativeError::SelfParent
            } else {
                RelativeError::NoParent
            });
        }
        // Bind the reference to one concrete placement, now.
        let (ph, parent) = self
            .find_entry(pimg, pplace)
            .ok_or(RelativeError::NoParent)?;
        // `P=` alone can land on the placement being replaced: still a
        // self-reference (kitty: EINVAL).
        if Some(ph) == existing {
            return Err(RelativeError::SelfParent);
        }
        let parent_origin = self.origin_of(parent);
        let bind = (!parent.from_placeholder).then_some(ph);
        // Walk the bound chain from the parent upward: detect a cycle back
        // to the placement being replaced, and enforce the depth cap.
        // Depth counts the ancestors (parent = depth 1).
        let mut depth = 1usize;
        let mut cursor = parent;
        while let Some((ah, ancestor)) = self.parent_entry(cursor) {
            if Some(ah) == existing {
                return Err(RelativeError::Cycle);
            }
            depth += 1;
            if depth > MAX_RELATIVE_DEPTH {
                return Err(RelativeError::TooDeep);
            }
            cursor = ancestor;
        }
        child.parent_ref = bind;
        (child.line, child.col) = offset_origin(parent_origin, child.rel_offset);
        child.clip_rows = inherited_clip(parent, child.rows);
        Ok(self.upsert(child))
    }

    /// `(handle, placement)` for `(image_id, placement_id)`;
    /// `placement_id == 0` matches the image's oldest placement.
    fn find_entry(
        &self,
        image_id: u32,
        placement_id: u32,
    ) -> Option<(PlacementHandle, &Placement)> {
        self.placements
            .iter()
            .find(|(_, p)| {
                p.image_id == image_id && (placement_id == 0 || p.placement_id == placement_id)
            })
            .map(|(h, p)| (*h, p))
    }

    /// The placement `p` is bound to, with its handle.
    fn parent_entry(&self, p: &Placement) -> Option<(PlacementHandle, &Placement)> {
        if let Some(h) = p.parent_ref {
            return self
                .placements
                .iter()
                .find(|(q, _)| *q == h)
                .map(|(h, q)| (*h, q));
        }
        let (img, place) = p.parent?;
        self.find_entry(img, place)
    }

    /// The `(line, col)` a child measures its offset from: the parent's
    /// top-left cell, or for a Unicode-placeholder parent the minimum x
    /// and minimum y over its tiles (REL-5).
    fn origin_of(&self, parent: &Placement) -> (i64, i32) {
        if !parent.from_placeholder {
            return (parent.line, parent.col);
        }
        self.placeholder_origin(parent.image_id, parent.placement_id)
            .unwrap_or((parent.line, parent.col))
    }

    /// Min line / min col over the placeholder tiles of `(image_id,
    /// placement_id)`.
    // TODO(#28): becomes `Term::placeholder_origin`.
    fn placeholder_origin(&self, image_id: u32, placement_id: u32) -> Option<(i64, i32)> {
        self.placements
            .iter()
            .map(|(_, p)| p)
            .filter(|p| {
                p.from_placeholder && p.image_id == image_id && p.placement_id == placement_id
            })
            .fold(None, |acc: Option<(i64, i32)>, p| {
                Some(acc.map_or((p.line, p.col), |(l, c)| (l.min(p.line), c.min(p.col))))
            })
    }

    /// Insert `p`, or, when it is named (`image_id` and `placement_id`
    /// both non-zero) and a placement with the same pair exists, replace
    /// that one in place. Replacing keeps the old placement's position in
    /// the draw order and its relative children, which are re-resolved
    /// so they follow it (REL-3). Kitty: "If you send two placements with
    /// the same image id and placement id the second one will replace the
    /// first."
    pub fn upsert(&mut self, p: Placement) -> PlacementHandle {
        if p.image_id != 0
            && p.placement_id != 0
            && let Some((h, slot)) = self
                .placements
                .iter_mut()
                .find(|(_, q)| q.image_id == p.image_id && q.placement_id == p.placement_id)
        {
            *slot = p;
            let h = *h;
            self.bounds = None;
            self.resolve_relative_positions();
            return h;
        }
        self.add(p)
    }

    /// Re-resolve every relative placement's position against its current
    /// parent (M13). Called after a mutation that moves placements
    /// individually (margin scroll, IL/DL) so children follow their
    /// parents. Full-screen scrolls move nothing (placements are anchored
    /// to line ids), so they need no resolve. Processes placements in
    /// dependency order via repeated passes (chains resolve outward from
    /// roots); bounded by [`MAX_RELATIVE_DEPTH`] passes. A relative
    /// placement whose parent has disappeared is left at its last
    /// resolved position (it will be cleaned up by the normal
    /// eviction paths).
    pub fn resolve_relative_positions(&mut self) {
        let has_relative = self.placements.iter().any(|(_, p)| p.parent.is_some());
        if !has_relative {
            return;
        }
        self.bounds = None;
        let index: HashMap<PlacementHandle, usize> = self
            .placements
            .iter()
            .enumerate()
            .map(|(i, (h, _))| (*h, i))
            .collect();
        for _ in 0..MAX_RELATIVE_DEPTH {
            // Compute every child's target from a consistent view of the
            // parents before mutating.
            let targets: Vec<(usize, (i64, i32), Range<u16>)> = self
                .placements
                .iter()
                .enumerate()
                .filter_map(|(i, (_, p))| {
                    p.parent?;
                    let parent = match p.parent_ref {
                        Some(h) => &self.placements[*index.get(&h)?].1,
                        None => self.parent_entry(p)?.1,
                    };
                    let origin = offset_origin(self.origin_of(parent), p.rel_offset);
                    Some((i, origin, inherited_clip(parent, p.rows)))
                })
                .collect();
            let mut changed = false;
            for (i, origin, clip) in targets {
                let p = &mut self.placements[i].1;
                if (p.line, p.col) != origin || p.clip_rows != clip {
                    (p.line, p.col) = origin;
                    p.clip_rows = clip;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
    }

    /// The `(line, col)` a relative child of `(image_id, placement_id)`
    /// measures its offset from (see `origin_of`). `placement_id == 0`
    /// means the image's oldest placement.
    #[must_use]
    pub fn parent_origin(&self, image_id: u32, placement_id: u32) -> Option<(i64, i32)> {
        let (_, parent) = self.find_entry(image_id, placement_id)?;
        Some(self.origin_of(parent))
    }

    /// Remove by handle. Returns the removed placement if it was
    /// present.
    /// Its orphaned relative children are dropped as well (not returned).
    pub fn remove(&mut self, handle: PlacementHandle) -> Option<Placement> {
        let pos = self.placements.iter().position(|(h, _)| *h == handle)?;
        self.bounds = None;
        let p = self.placements.swap_remove(pos).1;
        self.sweep_orphans(&mut Vec::new());
        Some(p)
    }

    /// Remove every placement matching `pred`. Returns the removed
    /// placements in their original order; the survivors keep theirs.
    /// Single pass, O(n) however many are removed.
    ///
    /// Relative placements whose parent is gone go too, recursively: the
    /// spec ties a relative placement's lifetime to its parent's, and kitty
    /// deletes the whole group (REL-2). They are returned after the direct
    /// matches; [`Self::remove_where_split`] keeps them apart.
    pub fn remove_where(&mut self, pred: impl FnMut(&Placement) -> bool) -> Vec<Placement> {
        let (mut removed, cascaded) = self.remove_where_split(pred);
        removed.extend(cascaded);
        removed
    }

    /// [`Self::remove_where`], returning the direct matches and the
    /// relative placements that went with them (because a parent went)
    /// separately. kitty frees a cascaded child's image on some deletes
    /// and not others, so the caller needs to tell them apart.
    pub fn remove_where_split(
        &mut self,
        mut pred: impl FnMut(&Placement) -> bool,
    ) -> (Vec<Placement>, Vec<Placement>) {
        if !self.placements.iter().any(|(_, p)| pred(p)) {
            return (Vec::new(), Vec::new());
        }
        let mut removed = Vec::new();
        let mut kept = Vec::with_capacity(self.placements.len());
        for (h, p) in self.placements.drain(..) {
            if pred(&p) {
                removed.push(p);
            } else {
                kept.push((h, p));
            }
        }
        self.placements = kept;
        self.bounds = None;
        let mut cascaded = Vec::new();
        self.sweep_orphans(&mut cascaded);
        (removed, cascaded)
    }

    /// Remove (into `removed`) every relative placement whose bound
    /// parent no longer exists, repeating until the chains are cut.
    /// O(n) per pass (a set of live handles, then one `retain`), at most
    /// [`MAX_RELATIVE_DEPTH`] + 1 passes; nothing to do when no placement
    /// has a parent.
    fn sweep_orphans(&mut self, removed: &mut Vec<Placement>) {
        for _ in 0..=MAX_RELATIVE_DEPTH {
            if !self.placements.iter().any(|(_, p)| p.parent.is_some()) {
                return;
            }
            let live: HashSet<PlacementHandle> = self.placements.iter().map(|(h, _)| *h).collect();
            // Placeholder-parented children are found by ids (rare).
            let orphan: Vec<bool> = self
                .placements
                .iter()
                .map(|(_, p)| match p.parent_ref {
                    Some(h) => !live.contains(&h),
                    None => p.parent.is_some() && self.parent_entry(p).is_none(),
                })
                .collect();
            if !orphan.contains(&true) {
                return;
            }
            let mut kept = Vec::with_capacity(self.placements.len());
            for (i, entry) in self.placements.drain(..).enumerate() {
                if orphan[i] {
                    removed.push(entry.1);
                } else {
                    kept.push(entry);
                }
            }
            self.placements = kept;
            self.bounds = None;
        }
    }

    /// Drop every placement of `image_id`. Returns the dropped
    /// placements so the caller can mark cells dirty.
    pub fn remove_image(&mut self, image_id: u32) -> Vec<Placement> {
        self.remove_where(|p| p.image_id == image_id)
    }

    /// Drop every placement whose shown part intersects screen `row`.
    /// Returns the dropped placements so the caller can mark cells dirty.
    pub fn clear_row(&mut self, top_line: i64, row: u16) -> Vec<Placement> {
        self.remove_where(|p| p.on_row(top_line, row))
    }

    /// Drop every placement. Returns the dropped placements.
    pub fn clear(&mut self) -> Vec<Placement> {
        self.bounds = None;
        let out: Vec<Placement> = self.placements.drain(..).map(|(_, p)| p).collect();
        out
    }

    /// Iterate every placement in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = &Placement> {
        self.placements.iter().map(|(_, p)| p)
    }

    /// Iterate `(handle, placement)` in insertion order.
    pub fn iter_with_handles(&self) -> impl Iterator<Item = (PlacementHandle, &Placement)> {
        self.placements.iter().map(|(h, p)| (*h, p))
    }

    /// Number of placements.
    #[must_use]
    pub fn len(&self) -> usize {
        self.placements.len()
    }

    /// True when no placements are tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.placements.is_empty()
    }

    /// Move every placement by `delta` line ids. Used when the line-id
    /// space itself shifts under the placements (a resize that rebuilds
    /// the ring, or a grid op that renumbers rows), to keep each
    /// placement at the same screen row. `delta == 0` is a no-op.
    pub fn shift_lines(&mut self, delta: i64) {
        if delta == 0 {
            return;
        }
        self.bounds = None;
        for (_, p) in &mut self.placements {
            p.line += delta;
        }
    }

    /// Re-anchor every placement through `f`, which maps the `(line,
    /// col)` of its top-left cell before a reflow to the same text's cell
    /// after it. Used when a reflow renumbers the lines, so each placement
    /// follows the text it sits on. Relative children are re-resolved from
    /// their (mapped) parents afterwards.
    pub fn map_anchors(&mut self, f: impl Fn(i64, u16) -> (i64, u16)) {
        if self.placements.is_empty() {
            return;
        }
        self.bounds = None;
        for (_, p) in &mut self.placements {
            // A child left of column 0 (negative `H=`) is re-resolved
            // from its parent below; map only real cells.
            if let Ok(col) = u16::try_from(p.col) {
                let (line, col) = f(p.line, col);
                (p.line, p.col) = (line, i32::from(col));
            } else {
                p.line = f(p.line, 0).0;
            }
        }
        self.resolve_relative_positions();
    }

    /// Move every placement whose shown part reaches screen row
    /// `from_row` or below (`end > from_row`) by `delta` lines, untrimmed,
    /// then re-resolve relative children. Used for a full-screen scroll
    /// down (SD / RI at the top), which moves the on-screen text down
    /// while scrollback and the line ids stay put. Images on screen move
    /// with their text, including one that straddles the top edge (its
    /// hidden top rows come back on screen, as in kitty). One pushed
    /// partly off the bottom keeps its whole box, so a later scroll up
    /// brings it all back. Placements entirely in scrollback stay with
    /// the scrollback text (kitty shifts those too; keeping them on
    /// their history text is a deliberate divergence).
    pub fn shift_rows_from(&mut self, top_line: i64, from_row: i64, delta: i64) {
        if delta == 0 {
            return;
        }
        self.bounds = None;
        for (_, p) in &mut self.placements {
            if p.screen_rows(top_line).end > from_row {
                p.line += delta;
            }
        }
        self.resolve_relative_positions();
    }

    /// Scroll the placements inside a screen-row band whose text moves
    /// without its line ids moving: a DECSTBM margin scroll, or IL/DL.
    ///
    /// `top_line` is the line id of screen row 0, `region` the band's
    /// screen rows (exclusive end) and `delta` the distance the text
    /// moved (negative = up). Per the kitty spec ("only images that are
    /// entirely within the page area (between the margins) must be
    /// scrolled"), only placements whose shown rows lie entirely inside
    /// `region` move; the rest are left alone. A moved placement is
    /// clipped at the region edges through [`Placement::clip_rows`], and
    /// one with nothing left inside the region is dropped and returned.
    ///
    /// Returns the dropped placements. `delta == 0` is a no-op.
    pub fn scroll_region(
        &mut self,
        top_line: i64,
        region: Range<u16>,
        delta: i32,
    ) -> Vec<Placement> {
        self.scroll_region_where(top_line, region, delta, |_| true)
    }

    /// [`Self::scroll_region`], moving only the placements `only` accepts
    /// (e.g. those inside DECSLRM left/right margins, or placeholder
    /// tiles on IL/DL). The rest are left alone.
    pub fn scroll_region_where(
        &mut self,
        top_line: i64,
        region: Range<u16>,
        delta: i32,
        only: impl Fn(&Placement) -> bool,
    ) -> Vec<Placement> {
        if delta == 0 || region.is_empty() {
            return Vec::new();
        }
        let (rt, rb) = (i64::from(region.start), i64::from(region.end));
        let delta = i64::from(delta);
        self.bounds = None;
        let mut removed = Vec::new();
        let mut i = 0;
        while i < self.placements.len() {
            let p = &mut self.placements[i].1;
            let shown = p.screen_rows(top_line);
            // Relative children don't scroll on their own: they follow
            // their parent through `resolve_relative_positions`, so a
            // child inside the region whose parent is outside keeps its
            // place and its whole box (kitty-ref).
            if shown.is_empty()
                || shown.start < rt
                || shown.end > rb
                || p.parent.is_some()
                || !only(p)
            {
                i += 1;
                continue;
            }
            p.line += delta;
            let start = (shown.start + delta).max(rt);
            let end = (shown.end + delta).min(rb);
            if start >= end {
                // Rare (a whole image scrolled out of its region), so the
                // O(n) shift of `remove` is fine here.
                removed.push(self.placements.remove(i).1);
                continue;
            }
            // Back into box-relative rows. Both lie inside the old clip
            // (the shift only shrinks it), so the casts can't truncate.
            let base = p.line - top_line;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            {
                p.clip_rows = (start - base) as u16..(end - base) as u16;
            }
            i += 1;
        }
        if !removed.is_empty() {
            self.sweep_orphans(&mut removed);
        }
        removed
    }

    /// Drop every placement whose shown rows lie entirely outside the
    /// line ids `first..=last`: those scrolled out of the history
    /// (`first` = oldest retained line) or pushed below the screen
    /// (`last` = the live bottom line). Returns the dropped placements.
    ///
    /// O(1) when nothing can be evicted (the common case on every LF):
    /// it checks cached line bounds before scanning.
    pub fn retain_lines(&mut self, first: i64, last: i64) -> Vec<Placement> {
        if self.placements.is_empty() {
            return Vec::new();
        }
        let (min_end, max_start) = *self.bounds.get_or_insert_with(|| {
            self.placements
                .iter()
                .fold((i64::MAX, i64::MIN), |(e, s), (_, p)| {
                    let shown = p.shown_lines();
                    // An empty shown band must always be swept.
                    let end = if shown.is_empty() {
                        i64::MIN
                    } else {
                        shown.end
                    };
                    (e.min(end), s.max(shown.start))
                })
        });
        if min_end > first && max_start <= last {
            return Vec::new();
        }
        self.remove_where(|p| {
            let shown = p.shown_lines();
            shown.is_empty() || shown.end <= first || shown.start > last
        })
    }

    /// Returns true iff any placement's shown part covers screen cell
    /// `(row, col)`.
    #[must_use]
    pub fn covers(&self, top_line: i64, row: u16, col: u16) -> bool {
        self.placements
            .iter()
            .any(|(_, p)| p.covers(top_line, row, col))
    }

    /// Returns true iff any placement's shown part intersects screen
    /// row `row`.
    #[must_use]
    pub fn any_on_row(&self, top_line: i64, row: u16) -> bool {
        self.placements.iter().any(|(_, p)| p.on_row(top_line, row))
    }
}

/// The rows of a `rows`-tall child left showing when its parent has been
/// cut by a margin scroll: the child loses as many rows at the top (and
/// bottom) as its parent did. Measured on kitty-ref 0.49.2: with parent
/// and child inside a DECSTBM region, two LFs cut the parent's top row at
/// the margin, and the child, though still inside the region, lost its
/// top row too. A parent outside the region keeps a whole child.
fn inherited_clip(parent: &Placement, rows: u16) -> Range<u16> {
    let top = parent.clip_rows.start.min(rows);
    let bottom = parent.rows.saturating_sub(parent.clip_rows.end).min(rows);
    top..rows.saturating_sub(bottom).max(top)
}

/// Apply a signed `(cols, rows)` cell offset to a `(line, col)` origin.
/// Used to resolve relative placements (M13). Neither axis clamps: a
/// child above the screen is simply in scrollback, and one left of
/// column 0 is cropped at draw time, as in kitty (REL-6). Note the offset
/// tuple is `(H=cols, V=rows)` to match kitty's key ordering, while the
/// origin tuple is `(line, col)`.
fn offset_origin(origin: (i64, i32), offset: (i32, i32)) -> (i64, i32) {
    let (line, col) = origin;
    let (h_cols, v_rows) = offset;
    (line + i64::from(v_rows), col.saturating_add(h_cols))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A placement whose box covers `rows` (as line ids) × `cols`.
    fn p(image_id: u32, rows: Range<i64>, cols: Range<u16>, z: i32) -> Placement {
        let mut pl = Placement::new(
            image_id,
            u16::try_from(rows.end - rows.start).unwrap(),
            cols.end - cols.start,
        );
        pl.line = rows.start;
        pl.col = i32::from(cols.start);
        pl.z = z;
        pl
    }

    #[test]
    fn empty_grid_has_no_placements() {
        let g = ImageGrid::new();
        assert!(g.is_empty());
        assert_eq!(g.len(), 0);
    }

    #[test]
    fn add_then_iter_yields_in_order() {
        let mut g = ImageGrid::new();
        let a = g.add(p(1, 0..2, 0..4, 0));
        let b = g.add(p(2, 2..4, 4..8, -1));
        assert_eq!(g.len(), 2);
        let collected: Vec<u32> = g.iter().map(|p| p.image_id).collect();
        assert_eq!(collected, vec![1, 2]);
        assert_ne!(a, b);
    }

    #[test]
    fn remove_returns_some_and_drops_entry() {
        let mut g = ImageGrid::new();
        let h = g.add(p(7, 0..1, 0..1, 0));
        assert_eq!(g.remove(h).map(|x| x.image_id), Some(7));
        assert!(g.is_empty());
        assert_eq!(g.remove(h), None);
    }

    #[test]
    fn remove_image_drops_every_placement_of_that_id() {
        let mut g = ImageGrid::new();
        g.add(p(1, 0..1, 0..1, 0));
        g.add(p(2, 0..1, 1..2, 0));
        g.add(p(1, 1..2, 0..1, 0));
        let dropped = g.remove_image(1);
        assert_eq!(dropped.len(), 2);
        assert!(dropped.iter().all(|p| p.image_id == 1));
        assert_eq!(g.len(), 1);
        assert_eq!(g.iter().next().unwrap().image_id, 2);
    }

    #[test]
    fn clear_row_only_drops_placements_intersecting_row() {
        let mut g = ImageGrid::new();
        g.add(p(1, 10..12, 0..4, 0));
        g.add(p(2, 12..14, 0..4, 0));
        // Screen row 0 is line 10.
        let dropped = g.clear_row(10, 0);
        assert_eq!(dropped.len(), 1);
        assert_eq!(g.len(), 1);
        assert_eq!(g.iter().next().unwrap().image_id, 2);
    }

    #[test]
    fn clear_drops_everything() {
        let mut g = ImageGrid::new();
        g.add(p(1, 0..1, 0..1, 0));
        g.add(p(2, 0..1, 1..2, 0));
        let dropped = g.clear();
        assert_eq!(dropped.len(), 2);
        assert!(g.is_empty());
    }

    #[test]
    fn screen_rows_follow_top_line() {
        let pl = p(1, 100..105, 0..5, 0);
        assert_eq!(pl.screen_rows(100), 0..5);
        // Three lines scrolled in at the bottom: the box is now 3 rows
        // into scrollback, but still whole.
        assert_eq!(pl.screen_rows(103), -3..2);
        assert_eq!(pl.rows, 5);
        assert_eq!(pl.clip_rows, 0..5);
    }

    #[test]
    fn shift_lines_moves_every_placement() {
        let mut g = ImageGrid::new();
        g.add(p(1, 5..10, 0..5, 0));
        g.shift_lines(-2);
        assert_eq!(g.iter().next().unwrap().line, 3);
        g.shift_lines(0);
        assert_eq!(g.iter().next().unwrap().line, 3);
    }

    #[test]
    fn scroll_region_zero_is_noop() {
        let mut g = ImageGrid::new();
        g.add(p(1, 5..10, 0..5, 0));
        assert!(g.scroll_region(0, 0..20, 0).is_empty());
        assert_eq!(g.iter().next().unwrap().line, 5);
    }

    #[test]
    fn scroll_region_moves_placement_entirely_inside() {
        let mut g = ImageGrid::new();
        g.add(p(1, 5..8, 0..5, 0));
        assert!(g.scroll_region(0, 2..12, -2).is_empty());
        let pl = g.iter().next().unwrap();
        assert_eq!(pl.line, 3);
        assert_eq!(pl.clip_rows, 0..3);
    }

    #[test]
    fn scroll_region_leaves_straddling_placement_alone() {
        // SCR-5: rows 2..8 straddle the bottom margin of region 1..7.
        let mut g = ImageGrid::new();
        g.add(p(1, 2..8, 0..5, 0));
        g.scroll_region(0, 1..7, -2);
        let pl = g.iter().next().unwrap();
        assert_eq!(pl.line, 2);
        assert_eq!(pl.clip_rows, 0..6);
        // Straddling the top margin too.
        g.scroll_region(0, 3..20, -2);
        assert_eq!(g.iter().next().unwrap().line, 2);
    }

    #[test]
    fn scroll_region_up_clips_at_top_margin() {
        let mut g = ImageGrid::new();
        g.add(p(1, 4..8, 0..5, 0));
        g.scroll_region(0, 3..10, -2);
        let pl = g.iter().next().unwrap();
        // Box moved up 2 (rows 2..6); the first row is above the margin.
        assert_eq!(pl.line, 2);
        assert_eq!(pl.rows, 4, "the reserved box is never trimmed");
        assert_eq!(pl.clip_rows, 1..4);
        assert_eq!(pl.screen_rows(0), 3..6);
        // The clip sticks: scrolling further keeps cutting from the top.
        g.scroll_region(0, 3..10, -1);
        let pl = g.iter().next().unwrap();
        assert_eq!(pl.line, 1);
        assert_eq!(pl.clip_rows, 2..4);
        assert_eq!(pl.screen_rows(0), 3..5);
    }

    #[test]
    fn scroll_region_down_clips_at_bottom_margin() {
        let mut g = ImageGrid::new();
        g.add(p(1, 4..8, 0..5, 0));
        g.scroll_region(0, 3..9, 3);
        let pl = g.iter().next().unwrap();
        assert_eq!(pl.line, 7);
        assert_eq!(pl.clip_rows, 0..2);
        assert_eq!(pl.screen_rows(0), 7..9);
    }

    #[test]
    fn scroll_region_drops_placement_scrolled_out() {
        let mut g = ImageGrid::new();
        g.add(p(1, 4..6, 0..5, 0));
        let dropped = g.scroll_region(0, 3..9, -5);
        assert_eq!(dropped.len(), 1);
        assert!(g.is_empty());
    }

    #[test]
    fn scroll_region_uses_screen_rows_not_lines() {
        // Line 104 is screen row 4 when the top line is 100.
        let mut g = ImageGrid::new();
        g.add(p(1, 104..106, 0..5, 0));
        g.scroll_region(100, 3..9, -1);
        assert_eq!(g.iter().next().unwrap().screen_rows(100), 3..5);
    }

    #[test]
    fn scroll_region_leaves_relative_children_to_their_parent() {
        // #40 review: a child inside the region whose parent is outside
        // used to be moved and cropped, then snapped back by the resolve
        // with its crop left behind. kitty keeps it whole.
        let mut g = ImageGrid::new();
        let mut parent = p(1, 1..3, 0..4, 0);
        parent.placement_id = 1;
        g.add(parent);
        let mut child = rel(2, 2, (1, 1), (0, 4));
        child.rows = 3;
        child.clip_rows = 0..3;
        g.add_relative(child).unwrap();
        g.scroll_region(0, 5..20, -2);
        g.resolve_relative_positions();
        let c = g.find(2, 2).unwrap();
        assert_eq!((c.line, c.clip_rows.clone()), (5, 0..3));
    }

    #[test]
    fn retain_lines_drops_only_fully_outside() {
        let mut g = ImageGrid::new();
        g.add(p(1, 0..3, 0..1, 0)); // lines 0..3: evicted when first = 3
        g.add(p(2, 2..5, 0..1, 0)); // straddles the oldest line: kept
        g.add(p(3, 18..25, 0..1, 0)); // hangs below the bottom: kept
        g.add(p(4, 21..23, 0..1, 0)); // entirely below the bottom: dropped
        let dropped = g.retain_lines(3, 20);
        let ids: Vec<u32> = dropped.iter().map(|p| p.image_id).collect();
        assert_eq!(ids, vec![1, 4]);
        let kept: Vec<u32> = g.iter().map(|p| p.image_id).collect();
        assert_eq!(kept, vec![2, 3]);
    }

    #[test]
    fn retain_lines_respects_clip() {
        // A box at lines 0..4 shown only on lines 2..4 is gone once the
        // oldest retained line is 4.
        let mut g = ImageGrid::new();
        let mut pl = p(1, 0..4, 0..1, 0);
        pl.clip_rows = 2..4;
        g.add(pl);
        assert!(g.retain_lines(3, 100).is_empty());
        assert_eq!(g.retain_lines(4, 100).len(), 1);
    }

    #[test]
    fn covers_returns_true_when_cell_in_range() {
        let mut g = ImageGrid::new();
        g.add(p(1, 2..5, 3..8, 0));
        assert!(g.covers(0, 3, 5));
        assert!(!g.covers(0, 1, 5));
        assert!(!g.covers(0, 3, 8));
        // Two lines scrolled: screen row 1 is now line 3.
        assert!(g.covers(2, 1, 5));
        assert!(!g.covers(2, 3, 5));
    }

    #[test]
    fn any_on_row_finds_any_placement() {
        let mut g = ImageGrid::new();
        g.add(p(1, 2..5, 3..8, 0));
        assert!(g.any_on_row(0, 2));
        assert!(g.any_on_row(0, 4));
        assert!(!g.any_on_row(0, 5));
        assert!(!g.any_on_row(0, 1));
    }

    #[test]
    fn src_rect_full_is_zero_size() {
        let r = SrcRect::FULL;
        assert_eq!(r.w, 0);
        assert_eq!(r.h, 0);
        assert!(r.is_full());
    }

    #[test]
    fn remove_where_filters_and_returns_matches() {
        let mut g = ImageGrid::new();
        g.add(p(1, 0..1, 0..1, 0));
        g.add(p(2, 0..1, 1..2, 5));
        g.add(p(3, 0..1, 2..3, -1));
        let dropped = g.remove_where(|p| p.z > 0);
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].image_id, 2);
        assert_eq!(g.len(), 2);
    }

    #[test]
    fn handles_are_unique_per_add() {
        let mut g = ImageGrid::new();
        let h1 = g.add(p(1, 0..1, 0..1, 0));
        let h2 = g.add(p(1, 0..1, 0..1, 0));
        let h3 = g.add(p(1, 0..1, 0..1, 0));
        assert_ne!(h1, h2);
        assert_ne!(h2, h3);
        assert_ne!(h1, h3);
    }

    /// Build a relative-placement child: image/placement ids, parent
    /// ref, `(H,V)` offset, and a 1x1 span anchored at origin.
    fn rel(image_id: u32, placement_id: u32, parent: (u32, u32), offset: (i32, i32)) -> Placement {
        let mut pl = Placement::new(image_id, 1, 1);
        pl.placement_id = placement_id;
        pl.parent = Some(parent);
        pl.rel_offset = offset;
        pl
    }

    #[test]
    fn add_relative_resolves_against_parent_origin() {
        let mut g = ImageGrid::new();
        // Parent at line 5, col 3, named (img=1, place=10).
        let mut parent = p(1, 5..7, 3..6, 0);
        parent.placement_id = 10;
        g.add(parent);
        // Child offset H=2 (cols), V=1 (rows) → line 6, col 5.
        let h = g.add_relative(rel(2, 20, (1, 10), (2, 1))).unwrap();
        let child = g.iter().find(|p| p.image_id == 2).unwrap();
        assert_eq!(child.line, 6);
        assert_eq!(child.col, 5);
        assert!(g.remove(h).is_some());
    }

    #[test]
    fn add_relative_missing_parent_is_no_parent() {
        let mut g = ImageGrid::new();
        let err = g.add_relative(rel(2, 20, (99, 99), (1, 1))).unwrap_err();
        assert_eq!(err, RelativeError::NoParent);
        assert!(g.is_empty());
    }

    #[test]
    fn add_relative_self_reference_matches_kitty() {
        // REL-7, kitty 0.49.2: a placement that doesn't exist yet naming
        // itself → ENOPARENT; an existing one → EINVAL.
        let mut g = ImageGrid::new();
        let err = g.add_relative(rel(2, 20, (2, 20), (1, 1))).unwrap_err();
        assert_eq!(err, RelativeError::NoParent);
        let mut existing = p(2, 0..1, 0..1, 0);
        existing.placement_id = 20;
        g.add(existing);
        let err = g.add_relative(rel(2, 20, (2, 20), (1, 1))).unwrap_err();
        assert_eq!(err, RelativeError::SelfParent);
        assert!(g.find(2, 20).is_some(), "the existing placement survives");
    }

    #[test]
    fn add_relative_loop_back_is_cycle() {
        let mut g = ImageGrid::new();
        // (2,20) is a child of (1,10); re-placing (1,10) as a child of
        // (2,20) would close the loop.
        let mut a = p(1, 0..1, 0..1, 0);
        a.placement_id = 10;
        g.add(a);
        g.add_relative(rel(2, 20, (1, 10), (1, 1))).unwrap();
        let err = g.add_relative(rel(1, 10, (2, 20), (1, 1))).unwrap_err();
        assert_eq!(err, RelativeError::Cycle);
    }

    #[test]
    fn add_relative_chain_too_deep() {
        let mut g = ImageGrid::new();
        // Root (img=1, place=1), no parent.
        let mut root = p(1, 0..1, 0..1, 0);
        root.placement_id = 1;
        g.add(root);
        // Chain place=2..=MAX_RELATIVE_DEPTH+1 each parented to the prior.
        // place=(MAX+1) then has MAX ancestors — the deepest allowed.
        for i in 2..=MAX_RELATIVE_DEPTH as u32 + 1 {
            g.add_relative(rel(1, i, (1, i - 1), (1, 0))).unwrap();
        }
        // The next link would make the chain exceed MAX_RELATIVE_DEPTH.
        let last = MAX_RELATIVE_DEPTH as u32 + 1;
        let err = g
            .add_relative(rel(1, last + 1, (1, last), (1, 0)))
            .unwrap_err();
        assert_eq!(err, RelativeError::TooDeep);
    }

    #[test]
    fn resolve_relative_positions_follows_parent_move() {
        let mut g = ImageGrid::new();
        let mut parent = p(1, 5..7, 3..6, 0);
        parent.placement_id = 10;
        g.add(parent);
        g.add_relative(rel(2, 20, (1, 10), (2, 1))).unwrap();
        // Re-place the parent (same image + placement id) two rows up:
        // `upsert` replaces it in place and its child follows (REL-3).
        let mut parent = p(1, 3..5, 3..6, 0);
        parent.placement_id = 10;
        g.upsert(parent);
        assert_eq!(g.len(), 2);
        let child = g.find(2, 20).unwrap();
        // child = parent origin (3,3) + (H=2 cols, V=1 row) = line 4, col 5.
        assert_eq!(child.line, 4);
        assert_eq!(child.col, 5);
    }

    #[test]
    fn relative_child_keeps_offset_when_parent_scrolls_off() {
        // SCR-8 groundwork: a parent anchored to a line id keeps its
        // origin while it scrolls into history, so the child stays at
        // its offset (no clamp at screen row 0).
        let mut g = ImageGrid::new();
        let mut parent = p(1, 5..7, 0..3, 0);
        parent.placement_id = 10;
        g.add(parent);
        g.add_relative(rel(2, 20, (1, 10), (0, 6))).unwrap();
        // A full-screen scroll by 20 lines changes only the top line.
        g.resolve_relative_positions();
        let child = g.find(2, 20).unwrap();
        assert_eq!(child.line, 11);
        assert_eq!(child.screen_rows(25), -14..-13);
    }

    #[test]
    fn shift_rows_from_moves_top_straddler_and_resolves_children() {
        // SD with fixed line ids (#23): a parent straddling the top edge
        // (screen rows -2..4) moves with its text, and its child keeps
        // its offset. A placement entirely in scrollback stays.
        let mut g = ImageGrid::new();
        let mut parent = p(1, 98..104, 0..20, 0);
        parent.placement_id = 1;
        g.add(parent);
        g.add_relative(rel(2, 2, (1, 1), (0, 4))).unwrap();
        g.add(p(3, 90..95, 0..4, 0)); // rows -10..-5: history
        g.shift_rows_from(100, 0, 2);
        let parent = g.find(1, 1).unwrap();
        assert_eq!(parent.screen_rows(100), 0..6);
        assert_eq!(g.find(2, 2).unwrap().line - parent.line, 4);
        assert_eq!(g.find(3, 0).unwrap().screen_rows(100), -10..-5);
    }

    #[test]
    fn remove_where_keeps_survivor_order() {
        let mut g = ImageGrid::new();
        for id in 1..=6 {
            g.add(p(id, 0..1, 0..1, 0));
        }
        let gone = g.remove_where(|p| p.image_id % 2 == 0);
        let ids: Vec<u32> = gone.iter().map(|p| p.image_id).collect();
        assert_eq!(ids, vec![2, 4, 6]);
        let kept: Vec<u32> = g.iter().map(|p| p.image_id).collect();
        assert_eq!(kept, vec![1, 3, 5]);
        assert!(g.remove_where(|_| false).is_empty());
    }

    #[test]
    fn retain_lines_bounds_cache_tracks_mutations() {
        let mut g = ImageGrid::new();
        g.add(p(1, 10..12, 0..1, 0));
        assert!(g.retain_lines(5, 100).is_empty()); // caches (12, 10)
        g.shift_lines(-8); // now 2..4: the cache must not hide it
        assert_eq!(g.retain_lines(5, 100).len(), 1);
        g.add(p(2, 10..12, 0..1, 0));
        assert!(g.retain_lines(5, 100).is_empty());
        g.add(p(3, 1..3, 0..1, 0)); // a new, older placement
        assert_eq!(g.retain_lines(5, 100).len(), 1);
        // Below the live bottom.
        assert_eq!(g.retain_lines(0, 9).len(), 1);
        assert!(g.is_empty());
    }

    #[test]
    fn negative_column_offset_goes_off_screen() {
        // REL-6: kitty lets the child start left of column 0 and crops
        // it; the cell span on screen is what's left of the box.
        let mut g = ImageGrid::new();
        let mut parent = p(1, 5..7, 2..4, 0);
        parent.placement_id = 10;
        g.add(parent);
        let mut child = rel(2, 20, (1, 10), (-5, -2));
        child.cols = 4;
        g.add_relative(child).unwrap();
        let child = g.find(2, 20).unwrap();
        assert_eq!((child.line, child.col), (3, -3));
        assert_eq!(child.col_range(), 0..1);
    }

    #[test]
    fn removing_a_parent_cascades_through_the_chain() {
        // REL-2: the group goes together, direct matches first.
        let mut g = ImageGrid::new();
        let mut parent = p(1, 5..7, 2..4, 0);
        parent.placement_id = 10;
        g.add(parent);
        g.add(p(9, 0..1, 0..1, 0));
        g.add_relative(rel(2, 20, (1, 10), (1, 0))).unwrap();
        g.add_relative(rel(3, 30, (2, 20), (0, 1))).unwrap();
        let (direct, cascaded) = g.remove_where_split(|p| p.image_id == 1);
        let ids: Vec<u32> = direct.iter().map(|p| p.image_id).collect();
        assert_eq!(ids, vec![1]);
        let ids: Vec<u32> = cascaded.iter().map(|p| p.image_id).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&2) && ids.contains(&3));
        assert_eq!(g.len(), 1, "the unrelated placement stays");
    }

    #[test]
    fn retain_lines_and_scroll_region_cascade_too() {
        let mut g = ImageGrid::new();
        let mut parent = p(1, 0..2, 0..2, 0);
        parent.placement_id = 10;
        g.add(parent);
        g.add_relative(rel(2, 20, (1, 10), (0, 6))).unwrap();
        // The parent leaves the history; the child's rows (6..8) don't.
        let gone = g.retain_lines(3, 100);
        assert_eq!(gone.len(), 2);
        assert!(g.is_empty());
    }

    #[test]
    fn rejected_replacement_changes_nothing() {
        // REL-1: validation happens before the old placement is touched.
        let mut g = ImageGrid::new();
        let mut parent = p(1, 5..7, 2..4, 0);
        parent.placement_id = 10;
        g.add(parent);
        g.add_relative(rel(2, 20, (1, 10), (1, 0))).unwrap();
        let before: Vec<Placement> = g.iter().cloned().collect();
        // Parent relative to its own child → cycle.
        assert_eq!(
            g.add_relative(rel(1, 10, (2, 20), (0, 0))).unwrap_err(),
            RelativeError::Cycle
        );
        let after: Vec<Placement> = g.iter().cloned().collect();
        assert_eq!(before, after);
    }

    #[test]
    fn upsert_replaces_in_place_and_keeps_children() {
        let mut g = ImageGrid::new();
        let mut parent = p(1, 5..7, 2..4, 0);
        parent.placement_id = 10;
        let h = g.add(parent);
        g.add_relative(rel(2, 20, (1, 10), (3, 1))).unwrap();
        let mut moved = p(1, 0..2, 0..2, 0);
        moved.placement_id = 10;
        assert_eq!(g.upsert(moved), h, "same handle: replaced in place");
        assert_eq!(g.len(), 2);
        let child = g.find(2, 20).unwrap();
        assert_eq!((child.line, child.col), (1, 3));
    }

    #[test]
    fn placeholder_parent_origin_is_the_min_of_its_tiles() {
        // REL-5 (tile model until #28): min line and min col over the
        // tiles of the parent, independently.
        let mut g = ImageGrid::new();
        for (line, col) in [(3, 7), (3, 8), (4, 2), (4, 3)] {
            let mut t = p(7, line..line + 1, col..col + 1, 0);
            t.placement_id = 3;
            t.from_placeholder = true;
            g.add(t);
        }
        assert_eq!(g.parent_origin(7, 3), Some((3, 2)));
        g.add_relative(rel(8, 1, (7, 3), (1, 1))).unwrap();
        let child = g.find(8, 1).unwrap();
        assert_eq!((child.line, child.col), (4, 3));
    }
}
