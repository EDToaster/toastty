//! Ring-buffer grid (decision #6).
//!
//! `Grid` owns a heap-allocated, fixed-capacity array of `Row`s plus a head
//! index. Logical row `idx` maps to physical slot `(head + idx) % cap` —
//! the cheap O(1) random read that makes the renderer's per-frame viewport
//! pull cheap (decision #6).

use crate::cell::{Cell, Style};
use smallvec::SmallVec;

/// A single row in the grid. `soft_wrap` indicates the row continues onto
/// the next one (load-bearing for reflow — decision #6).
#[derive(Debug, Clone, Default)]
pub struct Row {
    pub cells: SmallVec<[Cell; 16]>,
    pub soft_wrap: bool,
}

impl Row {
    /// Build a row with `cols` blank cells. Used by `Grid::new` and by the
    /// "scroll up" path when the bottom row falls off into history.
    #[must_use]
    pub fn blank(cols: u16) -> Self {
        let mut cells = SmallVec::with_capacity(cols as usize);
        for _ in 0..cols {
            cells.push(Cell::BLANK);
        }
        Self {
            cells,
            soft_wrap: false,
        }
    }

    /// Reset every cell to blank but keep allocated capacity.
    pub fn clear(&mut self) {
        for c in &mut self.cells {
            *c = Cell::BLANK;
        }
        self.soft_wrap = false;
    }

    /// Resize the row in place. Growing fills with blanks; shrinking
    /// truncates. The row's allocation is reused when possible.
    pub fn resize_cols(&mut self, cols: u16) {
        let cols = cols as usize;
        if self.cells.len() < cols {
            self.cells.resize(cols, Cell::BLANK);
        } else {
            self.cells.truncate(cols);
        }
    }

    /// Write a cell at `col`, growing the row with blanks if necessary.
    /// Out-of-range writes against a clamped column are a logic bug in
    /// the caller — we silently grow, but never beyond `max_cols`.
    pub fn put(&mut self, col: u16, cell: Cell, max_cols: u16) {
        let col = col as usize;
        if col >= self.cells.len() {
            let target = (col + 1).min(max_cols as usize);
            self.cells.resize(target, Cell::BLANK);
        }
        if let Some(slot) = self.cells.get_mut(col) {
            *slot = cell;
        }
    }

    /// Erase cells in `[start, end)` (open at end) — used by EL handlers.
    pub fn erase(&mut self, start: u16, end: u16, style: Style) {
        let start = start as usize;
        let end = (end as usize).min(self.cells.len());
        if start >= end {
            return;
        }
        let blank = Cell { ch: ' ', style };
        for c in &mut self.cells[start..end] {
            *c = blank;
        }
    }
}

/// Ring-buffer grid: `cap` rows of `cols` cells each, with a moving head.
///
/// `visible_rows` is the number of rows the renderer treats as on-screen;
/// the slots past that are scrollback (for the primary grid) or unused
/// (for the alt grid, where `cap == visible_rows`).
#[derive(Debug)]
pub struct Grid {
    rows: Box<[Row]>,
    /// Index into `rows` corresponding to logical row 0.
    head: usize,
    cols: u16,
    visible_rows: u16,
}

impl Grid {
    /// Build a new grid with `visible_rows` visible rows and `cap` total
    /// ring slots. `cap` must be >= `visible_rows` (caller's invariant —
    /// asserted in debug builds).
    #[must_use]
    pub fn new(visible_rows: u16, cols: u16, cap: usize) -> Self {
        debug_assert!(cap >= visible_rows as usize, "cap < visible_rows");
        debug_assert!(visible_rows > 0 && cols > 0, "zero-sized grid");
        let cap = cap.max(visible_rows as usize).max(1);
        let mut v: Vec<Row> = Vec::with_capacity(cap);
        for _ in 0..cap {
            v.push(Row::blank(cols));
        }
        Self {
            rows: v.into_boxed_slice(),
            head: 0,
            cols,
            visible_rows,
        }
    }

    pub fn cols(&self) -> u16 {
        self.cols
    }

    pub fn visible_rows(&self) -> u16 {
        self.visible_rows
    }

    pub fn cap(&self) -> usize {
        self.rows.len()
    }

    fn slot(&self, idx: u16) -> usize {
        (self.head + idx as usize) % self.rows.len()
    }

    /// Borrow visible row `idx` (0-based, top of viewport = 0).
    pub fn row(&self, idx: u16) -> &Row {
        &self.rows[self.slot(idx)]
    }

    /// Mutably borrow visible row `idx`.
    pub fn row_mut(&mut self, idx: u16) -> &mut Row {
        let s = self.slot(idx);
        &mut self.rows[s]
    }

    /// Scroll up by one: the top row becomes scrollback, a fresh blank
    /// row appears at the bottom. O(1) thanks to the ring layout.
    pub fn scroll_up(&mut self) {
        // The slot that was logical row 0 becomes the new "bottom" row;
        // clear it so it acts as the freshly-blank line at the bottom.
        let bottom_slot = self.head;
        self.rows[bottom_slot].clear();
        self.rows[bottom_slot].resize_cols(self.cols);
        self.head = (self.head + 1) % self.rows.len();
    }

    /// Clear every visible row. Scrollback is left untouched (used by the
    /// alt screen flip, which only manages its own visible region).
    pub fn clear_visible(&mut self, style: Style) {
        let cols = self.cols;
        for i in 0..self.visible_rows {
            let row = self.row_mut(i);
            // Ensure width matches current geometry, then blank every cell.
            if row.cells.len() != cols as usize {
                row.resize_cols(cols);
            }
            row.cells.fill(Cell { ch: ' ', style });
            row.soft_wrap = false;
        }
    }

    /// Resize: change rows/cols. `Cell` content is best-effort preserved
    /// (decision #6 explicitly defers reflow); we only fix up widths.
    pub fn resize(&mut self, visible_rows: u16, cols: u16, cap: usize) {
        // TODO(reflow): walk soft-wrapped runs and re-shape per
        // decisions/scrollback.md. For M3 we resize widths and reallocate
        // the ring if `cap` changed.
        let cap = cap.max(visible_rows as usize).max(1);
        if cap == self.rows.len() {
            for r in &mut self.rows {
                r.resize_cols(cols);
            }
        } else {
            let mut v: Vec<Row> = Vec::with_capacity(cap);
            // Copy current visible region into the new ring at the top.
            let preserve = (visible_rows as usize).min(self.visible_rows as usize);
            for i in 0..preserve {
                let mut r = self.row(i as u16).clone();
                r.resize_cols(cols);
                v.push(r);
            }
            for _ in preserve..cap {
                v.push(Row::blank(cols));
            }
            self.rows = v.into_boxed_slice();
            self.head = 0;
        }
        self.cols = cols;
        self.visible_rows = visible_rows;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::{Color, Style};

    fn red_style() -> Style {
        Style {
            fg: Color::Red,
            ..Style::RESET
        }
    }

    #[test]
    fn new_row_is_all_blanks() {
        let r = Row::blank(4);
        assert_eq!(r.cells.len(), 4);
        assert!(r.cells.iter().all(|c| *c == Cell::BLANK));
        assert!(!r.soft_wrap);
    }

    #[test]
    fn row_clear_resets_cells_and_soft_wrap() {
        let mut r = Row::blank(3);
        r.cells[0] = Cell { ch: 'x', style: red_style() };
        r.soft_wrap = true;
        r.clear();
        assert!(r.cells.iter().all(|c| *c == Cell::BLANK));
        assert!(!r.soft_wrap);
    }

    #[test]
    fn row_resize_grows_and_shrinks() {
        let mut r = Row::blank(2);
        r.resize_cols(5);
        assert_eq!(r.cells.len(), 5);
        r.resize_cols(1);
        assert_eq!(r.cells.len(), 1);
        // grow back — newly added cells must be blank.
        r.resize_cols(3);
        assert_eq!(r.cells[1], Cell::BLANK);
        assert_eq!(r.cells[2], Cell::BLANK);
    }

    #[test]
    fn row_put_grows_up_to_max_cols() {
        let mut r = Row::blank(2);
        r.put(4, Cell { ch: 'z', style: Style::RESET }, 8);
        assert_eq!(r.cells.len(), 5);
        assert_eq!(r.cells[4].ch, 'z');
        // intermediate cells should be blanks
        assert_eq!(r.cells[2], Cell::BLANK);
    }

    #[test]
    fn row_put_clamped_by_max_cols() {
        let mut r = Row::blank(0);
        // max_cols = 3 so attempting to put at col 10 just grows to 3 — and
        // the write at col 10 is silently dropped.
        r.put(10, Cell { ch: 'x', style: Style::RESET }, 3);
        assert_eq!(r.cells.len(), 3);
        assert!(r.cells.iter().all(|c| *c == Cell::BLANK));
    }

    #[test]
    fn row_erase_handles_range_and_clamping() {
        let mut r = Row::blank(5);
        for (i, c) in r.cells.iter_mut().enumerate() {
            *c = Cell { ch: char::from(b'a' + i as u8), style: Style::RESET };
        }
        r.erase(1, 3, red_style());
        assert_eq!(r.cells[0].ch, 'a');
        assert_eq!(r.cells[1], Cell { ch: ' ', style: red_style() });
        assert_eq!(r.cells[2], Cell { ch: ' ', style: red_style() });
        assert_eq!(r.cells[3].ch, 'd');
        // Out-of-range end gets clamped.
        r.erase(4, 100, Style::RESET);
        assert_eq!(r.cells[4], Cell::BLANK);
        // start >= end is a no-op (no panic).
        r.erase(3, 3, red_style());
        assert_eq!(r.cells[3].ch, 'd');
    }

    #[test]
    fn grid_new_and_access() {
        let g = Grid::new(3, 4, 5);
        assert_eq!(g.cols(), 4);
        assert_eq!(g.visible_rows(), 3);
        assert_eq!(g.cap(), 5);
        for r in 0..3 {
            let row = g.row(r);
            assert_eq!(row.cells.len(), 4);
        }
    }

    // Note: `Grid::new(visible, cols, cap)` debug-asserts `cap >= visible`.
    // The runtime fallback (`cap.max(visible_rows)`) only matters in release
    // builds, so we don't have a unit test for it — exercising it would
    // require a release-only test harness.

    #[test]
    fn grid_scroll_up_rotates_head() {
        let mut g = Grid::new(2, 3, 2);
        g.row_mut(0).put(0, Cell { ch: 'a', style: Style::RESET }, 3);
        g.row_mut(1).put(0, Cell { ch: 'b', style: Style::RESET }, 3);
        g.scroll_up();
        // Now row 0 should hold what was row 1.
        assert_eq!(g.row(0).cells[0].ch, 'b');
        // Row 1 is fresh blank.
        assert_eq!(g.row(1).cells[0], Cell::BLANK);
    }

    #[test]
    fn grid_clear_visible_paints_blanks_with_style() {
        let mut g = Grid::new(2, 3, 3);
        g.row_mut(0).put(0, Cell { ch: 'x', style: Style::RESET }, 3);
        g.row_mut(0).soft_wrap = true;
        g.clear_visible(red_style());
        for r in 0..2 {
            let row = g.row(r);
            assert!(row.cells.iter().all(|c| c.ch == ' ' && c.style == red_style()));
            assert!(!row.soft_wrap);
        }
    }

    #[test]
    fn grid_resize_same_cap_only_adjusts_cols() {
        let mut g = Grid::new(3, 4, 3);
        g.row_mut(0).put(2, Cell { ch: 'a', style: Style::RESET }, 4);
        g.resize(3, 6, 3);
        assert_eq!(g.cols(), 6);
        assert_eq!(g.row(0).cells.len(), 6);
        assert_eq!(g.row(0).cells[2].ch, 'a');
    }

    #[test]
    fn grid_resize_different_cap_preserves_visible_rows() {
        let mut g = Grid::new(2, 3, 4);
        g.row_mut(0).put(0, Cell { ch: 'a', style: Style::RESET }, 3);
        g.row_mut(1).put(0, Cell { ch: 'b', style: Style::RESET }, 3);
        g.resize(3, 3, 6);
        assert_eq!(g.cap(), 6);
        assert_eq!(g.visible_rows(), 3);
        assert_eq!(g.row(0).cells[0].ch, 'a');
        assert_eq!(g.row(1).cells[0].ch, 'b');
        assert_eq!(g.row(2).cells[0], Cell::BLANK);
    }

    #[test]
    fn grid_clear_visible_after_shrink_fixes_width() {
        // If we mutate the row width through a manual path and then clear,
        // clear_visible should still align the row to current cols.
        let mut g = Grid::new(1, 3, 1);
        // Manually shrink the underlying row to simulate a partial state.
        g.row_mut(0).cells.truncate(1);
        g.clear_visible(Style::RESET);
        assert_eq!(g.row(0).cells.len(), 3);
    }
}
