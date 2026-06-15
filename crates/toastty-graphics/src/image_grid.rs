//! `ImageGrid`: per-Term parallel structure tracking image placements.
//!
//! Architecture choice (M11a, pre-approved): we keep image placements
//! *parallel* to the cell grid rather than folding them into `Cell`. This
//! avoids bloating `Cell` (40 bytes already) and keeps text mutations
//! (the hot path) cheap.
//!
//! A `Placement` covers a contiguous rectangle of cells with a slice
//! (sub-rect) of a single image. Multiple placements can overlap; render
//! order is broken by `z` then insertion order. `z >= 0` draws above
//! text, `z < 0` draws below.
//!
//! Lookups are linear today (`O(n)` per cell test). Image placements are
//! few — kitty's protocol limits each image to a handful of placements,
//! and real apps rarely show more than a few images at once — so this is
//! pragmatic. If it ever shows up in profiling, swap for an interval
//! tree.

use std::ops::Range;

/// A single image placement on the grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    /// Source image id (matches `ImageRegistry`).
    pub image_id: u32,
    /// Stable per-image placement id. Kitty's `p=` field; zero means
    /// "unnamed".
    pub placement_id: u32,
    /// Row span (inclusive start, exclusive end), in grid cells.
    pub row_range: Range<u16>,
    /// Column span (inclusive start, exclusive end), in grid cells.
    pub col_range: Range<u16>,
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
    /// cell-aligned position derived from `col_range` / `row_range`. It
    /// does NOT change which cells the placement occupies. Defaults to
    /// `(0, 0)`.
    pub pix_offset: (u32, u32),
    /// M13: parent linkage for a *relative placement*. `Some((image_id,
    /// placement_id))` when this placement is positioned relative to
    /// another placement (kitty's `P=`/`Q=` keys); `None` for an
    /// ordinary absolute placement. When set, this placement's
    /// `row_range`/`col_range` are resolved from the parent's origin plus
    /// [`Self::rel_offset`], and re-resolved whenever the parent moves.
    pub parent: Option<(u32, u32)>,
    /// M13: `(cols, rows)` cell offset from the parent placement's origin
    /// (kitty's `H=`/`V=`). Only meaningful when [`Self::parent`] is
    /// `Some`. Signed. Defaults to `(0, 0)`.
    pub rel_offset: (i32, i32),
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
}

/// Parallel image placement layer for a `Term` grid.
#[derive(Debug, Default)]
pub struct ImageGrid {
    placements: Vec<(PlacementHandle, Placement)>,
    next_handle: u32,
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
        handle
    }

    /// Find the placement matching `(image_id, placement_id)`, if any.
    /// Used to resolve relative-placement parents (M13). When several
    /// match (only possible for the unnamed `placement_id == 0` case),
    /// the most recently added is returned.
    #[must_use]
    pub fn find(&self, image_id: u32, placement_id: u32) -> Option<&Placement> {
        self.placements
            .iter()
            .rev()
            .map(|(_, p)| p)
            .find(|p| p.image_id == image_id && p.placement_id == placement_id)
    }

    /// Create a relative placement (M13).
    ///
    /// `child` carries the resolved geometry SPANS in its `row_range` /
    /// `col_range` (anchored at origin, like `add`); its `parent` and
    /// `rel_offset` fields drive positioning. The parent referenced by
    /// `child.parent` must already exist (else [`RelativeError::NoParent`]).
    /// Linking is rejected if it would form a cycle
    /// ([`RelativeError::Cycle`]) or push the chain past
    /// [`MAX_RELATIVE_DEPTH`] ([`RelativeError::TooDeep`]).
    ///
    /// On success the child's `row_range`/`col_range` are rebased onto
    /// the parent's current origin plus `rel_offset`, the child is
    /// inserted, and its handle is returned.
    pub fn add_relative(&mut self, mut child: Placement) -> Result<PlacementHandle, RelativeError> {
        let (pimg, pplace) = child.parent.ok_or(RelativeError::NoParent)?;
        // A placement cannot be its own parent.
        if pimg == child.image_id && pplace == child.placement_id {
            return Err(RelativeError::Cycle);
        }
        // Parent must exist.
        let parent = self.find(pimg, pplace).ok_or(RelativeError::NoParent)?;
        let parent_origin = (parent.row_range.start, parent.col_range.start);
        // Walk the parent chain from the parent upward: detect a cycle
        // back to the child, and enforce the depth cap. Depth counts the
        // number of ancestors (parent = depth 1).
        let mut depth = 1usize;
        let mut cursor = parent.parent;
        while let Some((aimg, aplace)) = cursor {
            if aimg == child.image_id && aplace == child.placement_id {
                return Err(RelativeError::Cycle);
            }
            depth += 1;
            if depth > MAX_RELATIVE_DEPTH {
                return Err(RelativeError::TooDeep);
            }
            cursor = self.find(aimg, aplace).and_then(|p| p.parent);
        }
        // Rebase the child's spans onto the parent origin + offset.
        let span_rows = child.row_range.end - child.row_range.start;
        let span_cols = child.col_range.end - child.col_range.start;
        let (start_row, start_col) = offset_origin(parent_origin, child.rel_offset);
        child.row_range = start_row..start_row.saturating_add(span_rows);
        child.col_range = start_col..start_col.saturating_add(span_cols);
        Ok(self.add(child))
    }

    /// Re-resolve every relative placement's position against its current
    /// parent (M13). Called after the grid mutates (scroll / shift /
    /// insert / delete lines) so children follow their parents. Processes
    /// placements in dependency order via repeated passes (chains resolve
    /// outward from roots); bounded by [`MAX_RELATIVE_DEPTH`] passes.
    /// A relative placement whose parent has disappeared is left at its
    /// last resolved position (it will be cleaned up by the normal
    /// off-screen/eviction paths).
    pub fn resolve_relative_positions(&mut self) {
        let has_relative = self.placements.iter().any(|(_, p)| p.parent.is_some());
        if !has_relative {
            return;
        }
        for _ in 0..MAX_RELATIVE_DEPTH {
            let mut changed = false;
            // Snapshot parent origins by (image_id, placement_id) before
            // mutating, so each pass uses a consistent view.
            let origins: Vec<(u32, u32, u16, u16)> = self
                .placements
                .iter()
                .map(|(_, p)| {
                    (
                        p.image_id,
                        p.placement_id,
                        p.row_range.start,
                        p.col_range.start,
                    )
                })
                .collect();
            for (_, p) in &mut self.placements {
                let Some((pimg, pplace)) = p.parent else {
                    continue;
                };
                let Some(&(_, _, prow, pcol)) = origins
                    .iter()
                    .find(|(img, place, _, _)| *img == pimg && *place == pplace)
                else {
                    continue;
                };
                let span_rows = p.row_range.end - p.row_range.start;
                let span_cols = p.col_range.end - p.col_range.start;
                let (start_row, start_col) = offset_origin((prow, pcol), p.rel_offset);
                let new_rows = start_row..start_row.saturating_add(span_rows);
                let new_cols = start_col..start_col.saturating_add(span_cols);
                if p.row_range != new_rows || p.col_range != new_cols {
                    p.row_range = new_rows;
                    p.col_range = new_cols;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
    }

    /// Remove by handle. Returns the removed placement if it was
    /// present.
    pub fn remove(&mut self, handle: PlacementHandle) -> Option<Placement> {
        let pos = self.placements.iter().position(|(h, _)| *h == handle)?;
        Some(self.placements.swap_remove(pos).1)
    }

    /// Remove every placement matching `pred`. Returns the removed
    /// placements in their original order.
    pub fn remove_where(&mut self, mut pred: impl FnMut(&Placement) -> bool) -> Vec<Placement> {
        let mut removed = Vec::new();
        let mut i = 0;
        while i < self.placements.len() {
            if pred(&self.placements[i].1) {
                removed.push(self.placements.remove(i).1);
            } else {
                i += 1;
            }
        }
        removed
    }

    /// Drop every placement of `image_id`. Returns the dropped
    /// placements so the caller can mark cells dirty.
    pub fn remove_image(&mut self, image_id: u32) -> Vec<Placement> {
        self.remove_where(|p| p.image_id == image_id)
    }

    /// Drop every placement on `row`. Returns the dropped placements so
    /// the caller can mark cells dirty.
    pub fn clear_row(&mut self, row: u16) -> Vec<Placement> {
        self.remove_where(|p| p.row_range.contains(&row))
    }

    /// Drop every placement. Returns the dropped placements.
    pub fn clear(&mut self) -> Vec<Placement> {
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

    /// Shift every placement starting at or below `scroll_top` down by
    /// `n` rows, clamped to `scroll_bottom` (exclusive). Placements
    /// whose `start` would land at or past `scroll_bottom` are dropped
    /// and returned so the caller can mark cells dirty.
    ///
    /// `n == 0` is a no-op. Symmetric to [`Self::shift_rows_up`]; used
    /// by RI (Reverse Index) when the grid scrolls down.
    pub fn shift_rows_down(
        &mut self,
        n: u16,
        scroll_top: u16,
        scroll_bottom: u16,
    ) -> Vec<Placement> {
        if n == 0 {
            return Vec::new();
        }
        let mut removed = Vec::new();
        let mut i = 0;
        while i < self.placements.len() {
            let p = &mut self.placements[i].1;
            if p.row_range.start < scroll_top {
                // Above the scroll region — unaffected.
                i += 1;
                continue;
            }
            let new_start = p.row_range.start.saturating_add(n);
            let new_end = p.row_range.end.saturating_add(n);
            if new_start >= scroll_bottom {
                // Entire placement scrolled past the bottom — drop.
                removed.push(self.placements.remove(i).1);
                continue;
            }
            // Clip the bottom edge to the scroll region.
            let new_end = new_end.min(scroll_bottom);
            p.row_range = new_start..new_end;
            i += 1;
        }
        removed
    }

    /// Shift every placement starting at or below `scroll_top` up by `n`
    /// rows. Placements whose entire row range scrolls above 0 are
    /// dropped and returned so the caller can mark cells dirty.
    ///
    /// `n == 0` is a no-op. Used by `linefeed` when the grid scrolls.
    pub fn shift_rows_up(&mut self, n: u16, scroll_top: u16) -> Vec<Placement> {
        if n == 0 {
            return Vec::new();
        }
        let mut removed = Vec::new();
        let mut i = 0;
        while i < self.placements.len() {
            let p = &mut self.placements[i].1;
            if p.row_range.start < scroll_top {
                // Above the scroll region — unaffected.
                i += 1;
                continue;
            }
            let start = p.row_range.start;
            let end = p.row_range.end;
            // Shift; saturate at zero.
            let new_start = start.saturating_sub(n);
            let new_end = end.saturating_sub(n);
            // If the placement's entire range scrolled off (end <= n),
            // drop it.
            if new_end <= scroll_top.saturating_sub(0) && end <= n {
                removed.push(self.placements.remove(i).1);
                continue;
            }
            // Drop when the entire range scrolled above row 0.
            if new_end == 0 {
                removed.push(self.placements.remove(i).1);
                continue;
            }
            p.row_range = new_start..new_end;
            i += 1;
        }
        removed
    }

    /// Like [`Self::shift_rows_down`], but only affects placements that
    /// start within `[scroll_top, scroll_bottom)` and clips the bottom
    /// edge to `scroll_bottom` (exclusive). Placements above or below
    /// the region are untouched; a placement whose top would scroll past
    /// `scroll_bottom` is dropped and returned.
    ///
    /// Used by DECSTBM partial-region scroll-down (Reverse Index inside
    /// margins): images inside the region scroll with the text and clip
    /// at the region boundaries.
    ///
    /// `n == 0` is a no-op.
    pub fn shift_rows_down_within(
        &mut self,
        n: u16,
        scroll_top: u16,
        scroll_bottom: u16,
    ) -> Vec<Placement> {
        if n == 0 {
            return Vec::new();
        }
        let mut removed = Vec::new();
        let mut i = 0;
        while i < self.placements.len() {
            let p = &mut self.placements[i].1;
            // Only placements wholly within the region participate.
            if p.row_range.start < scroll_top || p.row_range.start >= scroll_bottom {
                i += 1;
                continue;
            }
            let new_start = p.row_range.start.saturating_add(n);
            let new_end = p.row_range.end.saturating_add(n);
            if new_start >= scroll_bottom {
                // Entire placement scrolled past the region bottom — drop.
                removed.push(self.placements.remove(i).1);
                continue;
            }
            let new_end = new_end.min(scroll_bottom);
            p.row_range = new_start..new_end;
            i += 1;
        }
        removed
    }

    /// Like [`Self::shift_rows_up`], but clips the bottom edge of each
    /// shifted placement to `scroll_bottom` (exclusive) and only affects
    /// placements that start at or below `scroll_top`. Placements whose
    /// entire row range scrolls above `scroll_top` are dropped and
    /// returned so the caller can mark cells dirty.
    ///
    /// Used by DECSTBM partial-region scroll-up: images inside the
    /// margin region scroll with the text and are clipped at the region
    /// boundaries; images above or below the region are untouched.
    ///
    /// `n == 0` is a no-op.
    pub fn shift_rows_up_within(
        &mut self,
        n: u16,
        scroll_top: u16,
        scroll_bottom: u16,
    ) -> Vec<Placement> {
        if n == 0 {
            return Vec::new();
        }
        let mut removed = Vec::new();
        let mut i = 0;
        while i < self.placements.len() {
            let p = &mut self.placements[i].1;
            // Only placements wholly within the region participate; a
            // placement that starts above the region or starts at/below
            // the region bottom is left alone (it isn't "entirely within
            // the page area").
            if p.row_range.start < scroll_top || p.row_range.start >= scroll_bottom {
                i += 1;
                continue;
            }
            let new_start = p.row_range.start.saturating_sub(n);
            let new_end = p.row_range.end.saturating_sub(n);
            // Clamp the top edge to the region top — a placement can't
            // scroll above its own region; if its whole span scrolls
            // above `scroll_top`, drop it.
            if new_end <= scroll_top {
                removed.push(self.placements.remove(i).1);
                continue;
            }
            let new_start = new_start.max(scroll_top);
            // Clip the bottom edge to the region (defensive; shifting up
            // only shrinks the end).
            let new_end = new_end.min(scroll_bottom);
            p.row_range = new_start..new_end;
            i += 1;
        }
        removed
    }

    /// Clip every placement to the new geometry `rows` x `cols` after a
    /// resize. Ranges are trimmed to `[0, rows)` / `[0, cols)`; any
    /// placement whose row or column range starts past the new edge (it
    /// has no visible cells) is dropped. The placement's `pix_offset` is
    /// preserved untouched — only the cell ranges are clipped. Returns
    /// `true` if anything changed.
    pub fn clip_to(&mut self, rows: u16, cols: u16) -> bool {
        let mut changed = false;
        let mut i = 0;
        while i < self.placements.len() {
            let p = &mut self.placements[i].1;
            // A placement that begins at/after the new edge has no
            // visible cells — drop it entirely.
            if p.row_range.start >= rows || p.col_range.start >= cols {
                self.placements.remove(i);
                changed = true;
                continue;
            }
            if p.row_range.end > rows {
                p.row_range.end = rows;
                changed = true;
            }
            if p.col_range.end > cols {
                p.col_range.end = cols;
                changed = true;
            }
            i += 1;
        }
        changed
    }

    /// Returns true iff any placement covers cell `(row, col)`.
    #[must_use]
    pub fn covers(&self, row: u16, col: u16) -> bool {
        self.placements
            .iter()
            .any(|(_, p)| p.row_range.contains(&row) && p.col_range.contains(&col))
    }

    /// Returns true iff any placement covers any cell on `row`.
    #[must_use]
    pub fn any_on_row(&self, row: u16) -> bool {
        self.placements
            .iter()
            .any(|(_, p)| p.row_range.contains(&row))
    }
}

/// Apply a signed `(cols, rows)` cell offset to a `(row, col)` cell
/// origin, clamping at the grid edge (row/col 0). Used to resolve
/// relative placements (M13). Note the offset tuple is `(H=cols,
/// V=rows)` to match kitty's key ordering, while the origin tuple is
/// `(row, col)`.
fn offset_origin(origin: (u16, u16), offset: (i32, i32)) -> (u16, u16) {
    let (row, col) = origin;
    let (h_cols, v_rows) = offset;
    let new_row = (i64::from(row) + i64::from(v_rows)).max(0) as u16;
    let new_col = (i64::from(col) + i64::from(h_cols)).max(0) as u16;
    (new_row, new_col)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(image_id: u32, rows: Range<u16>, cols: Range<u16>, z: i32) -> Placement {
        Placement {
            image_id,
            placement_id: 0,
            row_range: rows,
            col_range: cols,
            src_rect: SrcRect::FULL,
            z,
            pix_offset: (0, 0),
            parent: None,
            rel_offset: (0, 0),
        }
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
        g.add(p(1, 0..2, 0..4, 0));
        g.add(p(2, 2..4, 0..4, 0));
        let dropped = g.clear_row(0);
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
    fn shift_rows_up_zero_is_noop() {
        let mut g = ImageGrid::new();
        g.add(p(1, 5..10, 0..5, 0));
        let dropped = g.shift_rows_up(0, 0);
        assert!(dropped.is_empty());
        let p0 = g.iter().next().unwrap();
        assert_eq!(p0.row_range, 5..10);
    }

    #[test]
    fn shift_rows_up_one_shifts_below_scroll_top() {
        let mut g = ImageGrid::new();
        g.add(p(1, 5..10, 0..5, 0));
        let dropped = g.shift_rows_up(1, 0);
        assert!(dropped.is_empty());
        let p0 = g.iter().next().unwrap();
        assert_eq!(p0.row_range, 4..9);
    }

    #[test]
    fn shift_rows_up_drops_placements_scrolled_above() {
        let mut g = ImageGrid::new();
        g.add(p(1, 0..2, 0..5, 0));
        let dropped = g.shift_rows_up(3, 0);
        assert_eq!(dropped.len(), 1);
        assert!(g.is_empty());
    }

    #[test]
    fn shift_rows_up_above_scroll_top_unaffected() {
        let mut g = ImageGrid::new();
        // Placement entirely above scroll top: rows 0..2 with scroll_top=5
        g.add(p(1, 0..2, 0..5, 0));
        let dropped = g.shift_rows_up(2, 5);
        assert!(dropped.is_empty());
        let p0 = g.iter().next().unwrap();
        assert_eq!(p0.row_range, 0..2);
    }

    #[test]
    fn covers_returns_true_when_cell_in_range() {
        let mut g = ImageGrid::new();
        g.add(p(1, 2..5, 3..8, 0));
        assert!(g.covers(3, 5));
        assert!(!g.covers(1, 5));
        assert!(!g.covers(3, 8));
    }

    #[test]
    fn any_on_row_finds_any_placement() {
        let mut g = ImageGrid::new();
        g.add(p(1, 2..5, 3..8, 0));
        assert!(g.any_on_row(2));
        assert!(g.any_on_row(4));
        assert!(!g.any_on_row(5));
        assert!(!g.any_on_row(1));
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
        Placement {
            image_id,
            placement_id,
            row_range: 0..1,
            col_range: 0..1,
            src_rect: SrcRect::FULL,
            z: 0,
            pix_offset: (0, 0),
            parent: Some(parent),
            rel_offset: offset,
        }
    }

    #[test]
    fn add_relative_resolves_against_parent_origin() {
        let mut g = ImageGrid::new();
        // Parent at row 5, col 3, named (img=1, place=10).
        let mut parent = p(1, 5..7, 3..6, 0);
        parent.placement_id = 10;
        g.add(parent);
        // Child offset H=2 (cols), V=1 (rows) → row 6, col 5.
        let h = g.add_relative(rel(2, 20, (1, 10), (2, 1))).unwrap();
        let child = g.iter().find(|p| p.image_id == 2).unwrap();
        assert_eq!(child.row_range.start, 6);
        assert_eq!(child.col_range.start, 5);
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
    fn add_relative_self_reference_is_cycle() {
        let mut g = ImageGrid::new();
        let err = g.add_relative(rel(2, 20, (2, 20), (1, 1))).unwrap_err();
        assert_eq!(err, RelativeError::Cycle);
    }

    #[test]
    fn add_relative_loop_back_is_cycle() {
        let mut g = ImageGrid::new();
        // (1,10) -> parent (2,20); now add (2,20) -> parent (1,10): cycle.
        let mut a = p(1, 0..1, 0..1, 0);
        a.placement_id = 10;
        a.parent = Some((2, 20));
        g.add(a);
        let err = g.add_relative(rel(2, 20, (1, 10), (1, 1))).unwrap_err();
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
        // Move the parent up by 2 rows (simulate a scroll).
        g.shift_rows_up(2, 0);
        g.resolve_relative_positions();
        let parent = g.find(1, 10).unwrap();
        assert_eq!(parent.row_range.start, 3);
        let child = g.find(2, 20).unwrap();
        // child = parent origin (3,3) + (H=2 cols, V=1 row) = row 4, col 5.
        assert_eq!(child.row_range.start, 4);
        assert_eq!(child.col_range.start, 5);
    }
}
