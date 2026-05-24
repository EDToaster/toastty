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
        let blank = Cell {
            ch: ' ',
            style,
            is_continuation: false,
            hyperlink_id: None,
        };
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
///
/// Invariant for off-screen slots: each one is either (a) within the most
/// recent `history_lines` slots immediately preceding `head` (mod `cap`)
/// and holds a valid scrollback row, or (b) blank. `scroll_up` /
/// `scroll_down` are the only places that touch off-screen slots, and
/// they uphold this invariant.
#[derive(Debug)]
pub struct Grid {
    rows: Box<[Row]>,
    /// Index into `rows` corresponding to logical row 0.
    head: usize,
    cols: u16,
    visible_rows: u16,
    /// Number of scrollback rows currently retained above the visible
    /// region. Grows on `scroll_up`, capped at `cap - visible_rows`.
    /// For the alt grid (`cap == visible_rows`) this is always 0.
    history_lines: u32,
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
            history_lines: 0,
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

    /// Number of scrollback rows currently retained above the visible
    /// region (0..=`cap - visible_rows`). Use [`Grid::scrollback_row`] to
    /// read them.
    pub fn history_lines(&self) -> u32 {
        self.history_lines
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

    /// Borrow scrollback row `n` where `n == 0` is the row immediately
    /// above logical row 0 (the most recent scrollback line). Returns
    /// `None` when `n >= history_lines()`.
    pub fn scrollback_row(&self, n: u32) -> Option<&Row> {
        if n >= self.history_lines {
            return None;
        }
        // (head - 1 - n) mod cap, using usize math without underflow.
        let cap = self.rows.len();
        let offset = (n as usize) + 1;
        let slot = (self.head + cap - (offset % cap)) % cap;
        Some(&self.rows[slot])
    }

    /// Scroll up by one: the top row rotates into scrollback, a fresh
    /// blank row appears at the bottom. O(1) thanks to the ring layout.
    pub fn scroll_up(&mut self) {
        // The slot that's about to enter the visible region as the new
        // bottom (rotating around the ring) becomes the freshly-blank
        // line. Pre-blanking here preserves the off-screen-blank
        // invariant: when this slot rotates *out* again as scrollback
        // beyond the cap, the eviction is just an overwrite of blank
        // space.
        //
        // The slot at `head` (old logical row 0) is left untouched and
        // becomes scrollback row 0 after the head bump — that's the
        // retention path.
        let cap = self.rows.len();
        let new_bottom_slot = (self.head + self.visible_rows as usize) % cap;
        self.rows[new_bottom_slot].clear();
        self.rows[new_bottom_slot].resize_cols(self.cols);
        self.head = (self.head + 1) % cap;
        // Grow history, capped at the ring's scrollback budget.
        let max_history = (cap as u32).saturating_sub(u32::from(self.visible_rows));
        if self.history_lines < max_history {
            self.history_lines += 1;
        }
        // When max_history == 0 (alt grid, cap == visible) the field
        // stays at 0 — the cleared slot is just the rotated-around top.
    }

    /// Scroll down by one: a fresh blank row appears at the top, the
    /// bottom visible row falls off (xterm-style — *not* preserved as
    /// new scrollback). Used by RI / DECSTBM-less reverse scrolling.
    ///
    /// If there's existing scrollback, the most-recent scrollback row
    /// rotates back in as the new top (it occupies the slot at
    /// `head - 1`, which becomes the new logical row 0 after the head
    /// decrement). Otherwise the new top is a blank off-screen slot.
    /// Either way, `history_lines` shrinks by 1 (saturating at 0).
    pub fn scroll_down(&mut self) {
        // Clear the visible-bottom slot *before* moving the head, so the
        // row that's about to scroll off-screen goes out blank.
        let bottom_slot = self.slot(self.visible_rows.saturating_sub(1));
        self.rows[bottom_slot].clear();
        self.rows[bottom_slot].resize_cols(self.cols);
        // Decrement head with wrap-around. The slot that was at logical
        // row `-1` (the most recent scrollback row, blank if no history)
        // becomes the new logical row 0.
        self.head = if self.head == 0 {
            self.rows.len() - 1
        } else {
            self.head - 1
        };
        self.history_lines = self.history_lines.saturating_sub(1);
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
            row.cells.fill(Cell {
                ch: ' ',
                style,
                is_continuation: false,
                hyperlink_id: None,
            });
            row.soft_wrap = false;
        }
    }

    /// Resize: change rows/cols. `Cell` content is best-effort preserved
    /// (decision #6 explicitly defers reflow); we only fix up widths.
    /// Scrollback is preserved in the same-cap path (the ring stays put)
    /// but cleared when `cap` changes — reflow across a new ring is a
    /// future-work item per `decisions/scrollback.md`.
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
            // Cap changed → fresh ring; existing scrollback is dropped.
            self.history_lines = 0;
        }
        self.cols = cols;
        self.visible_rows = visible_rows;
        // Re-clamp history to the new scrollback budget. If the visible
        // region grew within the same cap, some scrollback slots may now
        // overlap the visible region.
        let max_history = (cap as u32).saturating_sub(u32::from(self.visible_rows));
        if self.history_lines > max_history {
            self.history_lines = max_history;
        }
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
        r.cells[0] = Cell {
            ch: 'x',
            style: red_style(),
            is_continuation: false,
            hyperlink_id: None,
        };
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
        r.put(
            4,
            Cell {
                ch: 'z',
                style: Style::RESET,
                is_continuation: false,
            hyperlink_id: None,
            },
            8,
        );
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
        r.put(
            10,
            Cell {
                ch: 'x',
                style: Style::RESET,
                is_continuation: false,
            hyperlink_id: None,
            },
            3,
        );
        assert_eq!(r.cells.len(), 3);
        assert!(r.cells.iter().all(|c| *c == Cell::BLANK));
    }

    #[test]
    fn row_erase_handles_range_and_clamping() {
        let mut r = Row::blank(5);
        for (i, c) in r.cells.iter_mut().enumerate() {
            *c = Cell {
                ch: char::from(b'a' + i as u8),
                style: Style::RESET,
                is_continuation: false,
            hyperlink_id: None,
            };
        }
        r.erase(1, 3, red_style());
        assert_eq!(r.cells[0].ch, 'a');
        assert_eq!(
            r.cells[1],
            Cell {
                ch: ' ',
                style: red_style(),
                is_continuation: false,
            hyperlink_id: None,
            }
        );
        assert_eq!(
            r.cells[2],
            Cell {
                ch: ' ',
                style: red_style(),
                is_continuation: false,
            hyperlink_id: None,
            }
        );
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
        g.row_mut(0).put(
            0,
            Cell {
                ch: 'a',
                style: Style::RESET,
                is_continuation: false,
            hyperlink_id: None,
            },
            3,
        );
        g.row_mut(1).put(
            0,
            Cell {
                ch: 'b',
                style: Style::RESET,
                is_continuation: false,
            hyperlink_id: None,
            },
            3,
        );
        g.scroll_up();
        // Now row 0 should hold what was row 1.
        assert_eq!(g.row(0).cells[0].ch, 'b');
        // Row 1 is fresh blank.
        assert_eq!(g.row(1).cells[0], Cell::BLANK);
    }

    #[test]
    fn grid_scroll_down_rotates_head() {
        let mut g = Grid::new(2, 3, 2);
        g.row_mut(0).put(
            0,
            Cell {
                ch: 'a',
                style: Style::RESET,
                is_continuation: false,
                hyperlink_id: None,
            },
            3,
        );
        g.row_mut(1).put(
            0,
            Cell {
                ch: 'b',
                style: Style::RESET,
                is_continuation: false,
                hyperlink_id: None,
            },
            3,
        );
        g.scroll_down();
        // Row 0 is fresh blank (the inserted line at the top).
        assert_eq!(g.row(0).cells[0], Cell::BLANK);
        // Row 1 holds what was row 0 ('a' moved down by one).
        assert_eq!(g.row(1).cells[0].ch, 'a');
    }

    #[test]
    fn grid_scroll_down_with_scrollback_blanks_off_screen_slot() {
        // cap > visible_rows: verify the slot scrolled off the visible
        // bottom is blanked so it stays clean as it rotates around the
        // ring (the invariant scroll_up also relies on).
        let mut g = Grid::new(2, 3, 4);
        g.row_mut(1).put(
            0,
            Cell {
                ch: 'z',
                style: Style::RESET,
                is_continuation: false,
                hyperlink_id: None,
            },
            3,
        );
        g.scroll_down();
        // After scrolling down twice more, the slot that held 'z' rotates
        // back into the visible region; it must still be blank.
        g.scroll_down();
        g.scroll_down();
        for r in 0..2 {
            assert_eq!(g.row(r).cells[0], Cell::BLANK);
        }
    }

    #[test]
    fn grid_clear_visible_paints_blanks_with_style() {
        let mut g = Grid::new(2, 3, 3);
        g.row_mut(0).put(
            0,
            Cell {
                ch: 'x',
                style: Style::RESET,
                is_continuation: false,
            hyperlink_id: None,
            },
            3,
        );
        g.row_mut(0).soft_wrap = true;
        g.clear_visible(red_style());
        for r in 0..2 {
            let row = g.row(r);
            assert!(
                row.cells
                    .iter()
                    .all(|c| c.ch == ' ' && c.style == red_style())
            );
            assert!(!row.soft_wrap);
        }
    }

    #[test]
    fn grid_resize_same_cap_only_adjusts_cols() {
        let mut g = Grid::new(3, 4, 3);
        g.row_mut(0).put(
            2,
            Cell {
                ch: 'a',
                style: Style::RESET,
                is_continuation: false,
            hyperlink_id: None,
            },
            4,
        );
        g.resize(3, 6, 3);
        assert_eq!(g.cols(), 6);
        assert_eq!(g.row(0).cells.len(), 6);
        assert_eq!(g.row(0).cells[2].ch, 'a');
    }

    #[test]
    fn grid_resize_different_cap_preserves_visible_rows() {
        let mut g = Grid::new(2, 3, 4);
        g.row_mut(0).put(
            0,
            Cell {
                ch: 'a',
                style: Style::RESET,
                is_continuation: false,
            hyperlink_id: None,
            },
            3,
        );
        g.row_mut(1).put(
            0,
            Cell {
                ch: 'b',
                style: Style::RESET,
                is_continuation: false,
            hyperlink_id: None,
            },
            3,
        );
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

    /// Test helper: write `ch` into column 0 of `row`.
    fn put_at(g: &mut Grid, row: u16, ch: char) {
        let cols = g.cols();
        g.row_mut(row).put(
            0,
            Cell {
                ch,
                style: Style::RESET,
                is_continuation: false,
                hyperlink_id: None,
            },
            cols,
        );
    }

    #[test]
    fn grid_scroll_up_preserves_top_row_as_scrollback() {
        // cap=4, vis=2 → up to 2 scrollback rows.
        let mut g = Grid::new(2, 3, 4);
        put_at(&mut g, 0, 'a');
        put_at(&mut g, 1, 'b');
        g.scroll_up();
        // Visible: 'b' at top, blank at bottom.
        assert_eq!(g.row(0).cells[0].ch, 'b');
        assert_eq!(g.row(1).cells[0], Cell::BLANK);
        // History: 'a' is now scrollback row 0 (most recent).
        assert_eq!(g.history_lines(), 1);
        assert_eq!(g.scrollback_row(0).unwrap().cells[0].ch, 'a');
        assert!(g.scrollback_row(1).is_none());
    }

    #[test]
    fn grid_scroll_up_accumulates_history_up_to_cap() {
        // cap=4, vis=2 → max 2 scrollback rows.
        //
        // Each iteration: write a char to the *top* visible row and then
        // scroll up, so the just-written char enters scrollback.
        let mut g = Grid::new(2, 3, 4);
        for c in ['a', 'b', 'c', 'd', 'e'] {
            put_at(&mut g, 0, c);
            g.scroll_up();
        }
        // History saturates at cap - vis = 2; older entries evicted.
        assert_eq!(g.history_lines(), 2);
        // Most recent scrollback rows are 'e' (row 0) then 'd' (row 1);
        // 'a', 'b', 'c' were evicted.
        assert_eq!(g.scrollback_row(0).unwrap().cells[0].ch, 'e');
        assert_eq!(g.scrollback_row(1).unwrap().cells[0].ch, 'd');
        assert!(g.scrollback_row(2).is_none());
    }

    #[test]
    fn grid_scrollback_row_evicts_oldest_when_full() {
        let mut g = Grid::new(1, 3, 3); // cap=3, vis=1 → 2 scrollback rows.
        put_at(&mut g, 0, '1');
        g.scroll_up();
        put_at(&mut g, 0, '2');
        g.scroll_up();
        put_at(&mut g, 0, '3');
        g.scroll_up();
        // History is full; '1' was evicted. Most recent is '3', then '2'.
        assert_eq!(g.history_lines(), 2);
        assert_eq!(g.scrollback_row(0).unwrap().cells[0].ch, '3');
        assert_eq!(g.scrollback_row(1).unwrap().cells[0].ch, '2');
    }

    #[test]
    fn grid_alt_screen_never_grows_history() {
        // cap == visible_rows → no scrollback budget.
        let mut g = Grid::new(3, 4, 3);
        for _ in 0..10 {
            g.scroll_up();
        }
        assert_eq!(g.history_lines(), 0);
        assert!(g.scrollback_row(0).is_none());
    }

    #[test]
    fn grid_scroll_down_consumes_history() {
        let mut g = Grid::new(2, 3, 4);
        put_at(&mut g, 0, 'a');
        put_at(&mut g, 1, 'b');
        g.scroll_up();
        assert_eq!(g.history_lines(), 1);
        g.scroll_down();
        // History row consumed as new top; counter decremented.
        assert_eq!(g.history_lines(), 0);
    }

    #[test]
    fn grid_scroll_down_no_history_saturates_at_zero() {
        let mut g = Grid::new(2, 3, 4);
        g.scroll_down();
        g.scroll_down();
        assert_eq!(g.history_lines(), 0);
    }

    #[test]
    fn grid_resize_smaller_cap_clears_history() {
        let mut g = Grid::new(2, 3, 4);
        put_at(&mut g, 0, 'a');
        g.scroll_up();
        assert_eq!(g.history_lines(), 1);
        g.resize(2, 3, 2);
        assert_eq!(g.history_lines(), 0);
    }

    #[test]
    fn grid_resize_grows_visible_clamps_history() {
        // cap=4, vis=2 → history budget 2. Grow vis to 4 (cap stays 4) →
        // history budget 0, existing history must be clamped.
        let mut g = Grid::new(2, 3, 4);
        put_at(&mut g, 0, 'a');
        g.scroll_up();
        put_at(&mut g, 0, 'b');
        g.scroll_up();
        assert_eq!(g.history_lines(), 2);
        g.resize(4, 3, 4);
        assert_eq!(g.history_lines(), 0);
    }
}
