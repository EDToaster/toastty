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
}
