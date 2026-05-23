//! Per-cell sparse damage tracking.
//!
//! M9 promotes the M3/M8 row-level `dirty: Vec<bool>` to a per-cell sparse
//! damage set. Mutating Perform callbacks mark `(row, col)` ranges via
//! [`Damage::mark_cell`] / [`Damage::mark_range`]; the renderer iterates
//! the damage set to decide which cells need bg + glyph re-emission.
//!
//! ### Design
//!
//! - Each row owns a [`RowDamage`] holding either a small sorted vector
//!   of dirty columns OR an `all_cols` flag indicating the whole row is
//!   dirty (which is what every scroll / `erase_display(2)` / resize
//!   ends up calling). The flag short-circuits the per-column bookkeeping
//!   so a "blast the whole screen" event doesn't grow the column vector.
//! - The top-level [`Damage`] has an `all` shortcut that means "every
//!   row dirty"; the renderer cascades this into `needs_full_clear` so
//!   the next frame uses `LoadOp::Clear` and rebuilds every instance.
//!
//! Why a sorted `SmallVec` instead of a `BitVec`? Most editing happens
//! in tight clusters (one keystroke moves the cursor by 1; CSI K erases
//! a contiguous range). A 4-slot inline `SmallVec` covers the typical case
//! without heap-allocating, and `mark_range` is the only operation that
//! grows past it.

use smallvec::SmallVec;

/// Per-row damage. Either "everything in this row is dirty" (set by
/// scroll / `erase_line` / wrap) or a sorted list of dirty columns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RowDamage {
    /// If true, every cell in this row is dirty — the renderer must not
    /// inspect `cols`.
    pub all_cols: bool,
    /// Sorted, deduped column indices. Always empty when `all_cols` is
    /// set (saves the renderer from a `if all_cols { ... } else { ... }`
    /// fork per row).
    pub cols: SmallVec<[u16; 4]>,
}

impl RowDamage {
    /// True iff no cells in this row are dirty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.all_cols && self.cols.is_empty()
    }

    /// Clear all damage state for this row.
    pub fn clear(&mut self) {
        self.all_cols = false;
        self.cols.clear();
    }

    /// Mark column `col` dirty. No-op when `all_cols` is already set.
    /// Sorted-insert keeps `cols` ordered and deduped so the renderer
    /// can iterate it directly.
    pub fn mark(&mut self, col: u16) {
        if self.all_cols {
            return;
        }
        match self.cols.binary_search(&col) {
            Ok(_) => {} // already present
            Err(idx) => self.cols.insert(idx, col),
        }
    }

    /// Mark the half-open range `[start, end)` dirty, clamped to
    /// `cols_in_row`. If the range covers the entire row, flips the
    /// `all_cols` shortcut (saving a per-column insert burst).
    pub fn mark_range(&mut self, start: u16, end: u16, cols_in_row: u16) {
        if self.all_cols || start >= end || cols_in_row == 0 {
            return;
        }
        let end = end.min(cols_in_row);
        if start >= end {
            return;
        }
        // Whole-row optimisation: avoid growing `cols` to N entries when
        // the caller already meant "all of it".
        if start == 0 && end == cols_in_row {
            self.mark_all();
            return;
        }
        for c in start..end {
            self.mark(c);
        }
    }

    /// Promote this row to "everything dirty". Drops the per-column list.
    pub fn mark_all(&mut self) {
        self.all_cols = true;
        self.cols.clear();
    }
}

/// Top-level damage set. One [`RowDamage`] per visible row, plus an
/// `all` shortcut that means "every row dirty + the renderer should
/// also treat the framebuffer as invalidated" (`needs_full_clear`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Damage {
    /// "Every row is dirty AND the framebuffer is stale." The renderer
    /// cascades this into `LoadOp::Clear` for the upcoming frame, then
    /// the binary calls [`Damage::clear`] after a successful submit.
    pub all: bool,
    /// Per-row damage; `rows.len()` equals the term's visible row count.
    pub rows: Vec<RowDamage>,
}

impl Damage {
    /// Fresh damage set sized to `rows` visible rows. Every row is
    /// pre-marked dirty so the first frame after construction does a
    /// full paint.
    #[must_use]
    pub fn new(rows: u16) -> Self {
        let mut row_damage = Vec::with_capacity(rows as usize);
        for _ in 0..rows {
            let mut r = RowDamage::default();
            r.mark_all();
            row_damage.push(r);
        }
        Self {
            all: true,
            rows: row_damage,
        }
    }

    /// True iff no cells are dirty across any row, and `all` is unset.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.all && self.rows.iter().all(RowDamage::is_empty)
    }

    /// Clear every damage signal. Called by the binary after a successful
    /// render.
    pub fn clear(&mut self) {
        self.all = false;
        for r in &mut self.rows {
            r.clear();
        }
    }

    /// Mark every row dirty AND flip the top-level `all` flag — the
    /// renderer's `needs_full_clear` is keyed off `all`, so this is the
    /// path BSU-watchdog / resize / theme-swap take.
    pub fn mark_all(&mut self) {
        self.all = true;
        for r in &mut self.rows {
            r.mark_all();
        }
    }

    /// Resize the per-row vector to `new_rows`, and mark everything
    /// dirty (the renderer's row cache is invalidated by definition on a
    /// resize, so a partial clean state would be a lie).
    pub fn resize(&mut self, new_rows: u16) {
        self.rows.resize_with(new_rows as usize, RowDamage::default);
        self.mark_all();
    }

    /// Iterate `(row_index, &RowDamage)` for all rows whose damage is
    /// non-empty (or the `all` shortcut is set, in which case every row
    /// is yielded). Used by the renderer's partial-redraw path.
    pub fn iter_rows(&self) -> impl Iterator<Item = (u16, &RowDamage)> {
        self.rows
            .iter()
            .enumerate()
            .filter_map(|(i, r)| {
                if r.is_empty() {
                    None
                } else {
                    // u16 cast is safe: caller's row count is u16 by
                    // construction (Term::new clamps).
                    #[allow(clippy::cast_possible_truncation)]
                    Some((i as u16, r))
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_damage_default_is_empty() {
        let r = RowDamage::default();
        assert!(r.is_empty());
        assert!(!r.all_cols);
        assert!(r.cols.is_empty());
    }

    #[test]
    fn row_damage_mark_inserts_sorted_unique() {
        let mut r = RowDamage::default();
        r.mark(5);
        r.mark(1);
        r.mark(3);
        r.mark(3); // dup, no-op
        r.mark(0);
        // Sorted, deduped.
        assert_eq!(&r.cols[..], &[0, 1, 3, 5]);
    }

    #[test]
    fn row_damage_mark_all_drops_cols() {
        let mut r = RowDamage::default();
        r.mark(1);
        r.mark(2);
        r.mark_all();
        assert!(r.all_cols);
        assert!(r.cols.is_empty());
    }

    #[test]
    fn row_damage_mark_after_all_is_noop() {
        let mut r = RowDamage::default();
        r.mark_all();
        r.mark(7);
        assert!(r.all_cols);
        assert!(r.cols.is_empty());
    }

    #[test]
    fn row_damage_clear_resets() {
        let mut r = RowDamage::default();
        r.mark_all();
        r.clear();
        assert!(r.is_empty());
        assert!(!r.all_cols);

        let mut r = RowDamage::default();
        r.mark(2);
        r.clear();
        assert!(r.is_empty());
    }

    #[test]
    fn row_damage_mark_range_normal() {
        let mut r = RowDamage::default();
        r.mark_range(2, 5, 10);
        assert_eq!(&r.cols[..], &[2, 3, 4]);
    }

    #[test]
    fn row_damage_mark_range_full_row_flips_all_cols() {
        let mut r = RowDamage::default();
        r.mark_range(0, 8, 8);
        assert!(r.all_cols);
        assert!(r.cols.is_empty());
    }

    #[test]
    fn row_damage_mark_range_clamps_to_cols_in_row() {
        let mut r = RowDamage::default();
        r.mark_range(5, 100, 8);
        // end clamped from 100 to 8.
        assert_eq!(&r.cols[..], &[5, 6, 7]);
    }

    #[test]
    fn row_damage_mark_range_invalid_is_noop() {
        let mut r = RowDamage::default();
        r.mark_range(5, 5, 8); // empty range
        assert!(r.is_empty());
        r.mark_range(7, 3, 8); // start > end
        assert!(r.is_empty());
        r.mark_range(0, 4, 0); // cols_in_row 0
        assert!(r.is_empty());
    }

    #[test]
    fn damage_new_marks_every_row_dirty() {
        let d = Damage::new(3);
        assert!(d.all);
        assert_eq!(d.rows.len(), 3);
        for r in &d.rows {
            assert!(r.all_cols);
        }
    }

    #[test]
    fn damage_is_empty_when_clear() {
        let mut d = Damage::new(2);
        d.clear();
        assert!(d.is_empty());
        assert!(!d.all);
    }

    #[test]
    fn damage_mark_all_sets_all_flag_and_each_row() {
        let mut d = Damage::new(3);
        d.clear();
        d.mark_all();
        assert!(d.all);
        for r in &d.rows {
            assert!(r.all_cols);
        }
    }

    #[test]
    fn damage_resize_grows_and_dirties() {
        let mut d = Damage::new(2);
        d.clear();
        d.resize(5);
        assert_eq!(d.rows.len(), 5);
        assert!(d.all);
        for r in &d.rows {
            assert!(r.all_cols);
        }
    }

    #[test]
    fn damage_resize_shrinks_and_dirties() {
        let mut d = Damage::new(5);
        d.clear();
        d.resize(2);
        assert_eq!(d.rows.len(), 2);
        assert!(d.all);
    }

    #[test]
    fn damage_iter_rows_skips_empty() {
        let mut d = Damage::new(4);
        d.clear();
        d.rows[1].mark(0);
        d.rows[3].mark_all();
        let seen: Vec<u16> = d.iter_rows().map(|(i, _)| i).collect();
        assert_eq!(seen, vec![1, 3]);
    }

    #[test]
    fn damage_iter_rows_when_all_yields_all() {
        let d = Damage::new(3);
        let seen: Vec<u16> = d.iter_rows().map(|(i, _)| i).collect();
        assert_eq!(seen, vec![0, 1, 2]);
    }
}
