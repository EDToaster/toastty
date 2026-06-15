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
    /// Monotonic id of the current live bottom row. Incremented on
    /// `scroll_up`, saturating-decremented on `scroll_down`. Each
    /// retained row (visible or scrollback) has a stable id derived
    /// from this counter, so callers (e.g. mouse selection) can pin
    /// to a row across scrolls and eviction. The absolute value of
    /// `bottom_id` is unimportant — only the deltas between retained
    /// rows are.
    bottom_id: u64,
}

/// Where a given `line_id` currently sits in the grid, if it's still
/// retained. Returned by [`Grid::locate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowLocation {
    /// Row is on-screen at visible index `0..visible_rows` (0 = top).
    Visible(u16),
    /// Row is in scrollback at index `n` (0 = most recent above visible).
    Scrollback(u32),
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
            // Live bottom row's id at construction. We start non-zero
            // so the visible region's top row (id `bottom_id -
            // (visible_rows - 1)`) doesn't underflow — callers do
            // straight subtraction to translate a visible-row index
            // into a `line_id`.
            bottom_id: u64::from(visible_rows.saturating_sub(1)),
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

    /// Monotonic id of the live bottom row. See [`Grid::bottom_id`] docs
    /// on the field.
    pub fn bottom_id(&self) -> u64 {
        self.bottom_id
    }

    /// id of the oldest row still retained (top of scrollback if any,
    /// else the visible top). All retained `line_id`s fall in
    /// `oldest_retained_id..=bottom_id`.
    pub fn oldest_retained_id(&self) -> u64 {
        let above_bottom =
            u64::from(self.history_lines) + u64::from(self.visible_rows.saturating_sub(1));
        self.bottom_id.saturating_sub(above_bottom)
    }

    /// Map a `line_id` to where the row currently sits, if it's still
    /// retained. Returns `None` when the row has scrolled out of the
    /// retained window (newer than the live bottom or older than the
    /// oldest scrollback slot).
    pub fn locate(&self, line_id: u64) -> Option<RowLocation> {
        if line_id > self.bottom_id {
            return None;
        }
        let delta = self.bottom_id - line_id;
        let vis = u64::from(self.visible_rows);
        if delta < vis {
            // The live bottom is at row `visible_rows - 1`; delta=0
            // maps there, delta=1 to one row above, …
            #[allow(clippy::cast_possible_truncation)]
            let row = (self.visible_rows - 1) - (delta as u16);
            Some(RowLocation::Visible(row))
        } else {
            let n = delta - vis;
            if n < u64::from(self.history_lines) {
                #[allow(clippy::cast_possible_truncation)]
                Some(RowLocation::Scrollback(n as u32))
            } else {
                None
            }
        }
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
        self.bottom_id = self.bottom_id.wrapping_add(1);
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
        self.bottom_id = self.bottom_id.saturating_sub(1);
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
        let old_visible_rows = self.visible_rows;
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
        // Keep `bottom_id` consistent with the new visible region. The
        // invariant is that visible row `i` has line_id
        // `bottom_id - (visible_rows - 1 - i)`. We want preserved rows
        // (which stay at the top of the new ring under both same-cap and
        // different-cap paths) to keep the same line_id; that requires
        // `bottom_id += new_visible_rows - old_visible_rows`. When growing
        // it bumps `bottom_id` so the freshly-exposed blank bottom rows
        // get fresh ids; when shrinking it pulls `bottom_id` down to track
        // the new (smaller) bottom. Without this, after a window grow the
        // top viewport rows would compute a negative line_id and saturate
        // to 0 — selection above the original top would all collapse onto
        // the same row.
        let delta = i64::from(visible_rows) - i64::from(old_visible_rows);
        if delta >= 0 {
            #[allow(clippy::cast_sign_loss)]
            let d = delta as u64;
            self.bottom_id = self.bottom_id.wrapping_add(d);
        } else {
            #[allow(clippy::cast_sign_loss)]
            let d = (-delta) as u64;
            self.bottom_id = self.bottom_id.saturating_sub(d);
        }
    }

    /// Reflow-aware resize for the **primary** grid. Rebuilds the ring at
    /// the new `(visible_rows, cols, cap)`, re-wrapping soft-wrapped
    /// logical lines to the new width and *preserving* scrollback (unlike
    /// [`Grid::resize`], which drops it on a cap change). On a width change
    /// this is what makes narrowing rewrap instead of truncating glyphs; on
    /// a height change it keeps history, revealing more of it at the top
    /// when the window grows and pushing rows into scrollback when it
    /// shrinks (xterm/Alacritty-style anchoring of the live bottom).
    ///
    /// `cursor` is the live-grid `(row, col)` on entry; the returned
    /// `(row, col)` is where that cursor lands in the new visible region
    /// (`col` may equal the new `cols` — the pending-wrap sentinel).
    ///
    /// The alt grid keeps the geometry-only [`Grid::resize`] (full-screen
    /// apps redraw on SIGWINCH and carry no scrollback).
    ///
    /// Inherent limitation: the ring is sized in *physical* rows, so heavy
    /// narrowing can evict the oldest scrollback that widening cannot then
    /// resurrect — matching Alacritty's behaviour.
    #[must_use]
    pub fn resize_reflow(
        &mut self,
        visible_rows: u16,
        cols: u16,
        cap: usize,
        cursor: (u16, u16),
    ) -> (u16, u16) {
        let cap = cap.max(visible_rows as usize).max(1);

        // Collect retained rows oldest→newest: scrollback (oldest first)
        // then the visible region.
        let mut retained: Vec<Row> =
            Vec::with_capacity(self.history_lines as usize + self.visible_rows as usize);
        for n in (0..self.history_lines).rev() {
            retained.push(self.scrollback_row(n).expect("n < history_lines").clone());
        }
        for i in 0..self.visible_rows {
            retained.push(self.row(i).clone());
        }
        let cursor_idx = self.history_lines as usize + cursor.0 as usize;

        // Trim trailing blank padding below the cursor — but never above
        // the cursor's own row, so a prompt on an otherwise-blank screen
        // still yields a line for the cursor.
        let last_nonblank = retained
            .iter()
            .rposition(|r| r.cells.iter().any(|c| *c != Cell::BLANK))
            .unwrap_or(0);
        let end = cursor_idx
            .max(last_nonblank)
            .min(retained.len().saturating_sub(1));
        retained.truncate(end + 1);

        let (flat, (cur_idx, cur_col)) =
            crate::reflow::reflow_rows(&retained, cols, cursor_idx, cursor.1);

        // Lay the rewrapped rows into a fresh ring with `head = 0`, so
        // logical row `i` sits at slot `i` and scrollback row `n` at slot
        // `cap - 1 - n` (the wrap-around region just before the head).
        let vis = visible_rows as usize;
        let budget = cap.saturating_sub(vis);
        let n = flat.len();
        let mut ring: Vec<Row> = (0..cap).map(|_| Row::blank(cols)).collect();
        let mut flat = flat;

        let (cursor_row, new_history) = if n <= vis {
            // Everything fits on screen; anchor content to the top, blank
            // padding below, no scrollback.
            for (i, row) in flat.into_iter().enumerate() {
                ring[i] = row;
            }
            #[allow(clippy::cast_possible_truncation)]
            (cur_idx as u16, 0u32)
        } else {
            // Content overflows: the live bottom is `flat[n-1]`. The last
            // `vis` rows are visible; rows above become scrollback, capped
            // to the ring's budget (oldest evicted).
            for vi in 0..vis {
                ring[vi] = std::mem::take(&mut flat[n - vis + vi]);
            }
            let scrollback_count = n - vis;
            let keep = scrollback_count.min(budget);
            for sn in 0..keep {
                let src = n - vis - 1 - sn;
                ring[cap - 1 - sn] = std::mem::take(&mut flat[src]);
            }
            #[allow(clippy::cast_possible_truncation)]
            let row = (cur_idx.saturating_sub(n - vis)).min(vis - 1) as u16;
            #[allow(clippy::cast_possible_truncation)]
            (row, keep as u32)
        };

        self.rows = ring.into_boxed_slice();
        self.head = 0;
        self.cols = cols;
        self.visible_rows = visible_rows;
        self.history_lines = new_history;
        // Reflow changes physical row counts, so the old per-line id
        // mapping can't survive a rewrap — cross-reflow `line_id` pinning is
        // best-effort and the only external pin (selection) is cleared on
        // resize. But `bottom_id` must still bound the retained id window:
        // raise it to cover the new (history + visible) span so
        // `oldest_retained_id` doesn't saturate to 0 (which would collapse
        // the id space when reflow grows scrollback past a small
        // `bottom_id`). Never decreases.
        let span = u64::from(new_history) + u64::from(visible_rows.saturating_sub(1));
        self.bottom_id = self.bottom_id.max(span);

        (cursor_row, cur_col.min(cols))
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

    // ---------- bottom_id / locate ----------

    #[test]
    fn bottom_id_increments_on_scroll_up() {
        let mut g = Grid::new(2, 3, 4);
        let start = g.bottom_id();
        g.scroll_up();
        g.scroll_up();
        g.scroll_up();
        assert_eq!(g.bottom_id(), start + 3);
    }

    #[test]
    fn bottom_id_decrements_on_scroll_down_saturating() {
        let mut g = Grid::new(2, 3, 4);
        g.scroll_up();
        g.scroll_up();
        let mid = g.bottom_id();
        g.scroll_down();
        assert_eq!(g.bottom_id(), mid - 1);
        // Walk past 0 — must saturate, not underflow.
        for _ in 0..20 {
            g.scroll_down();
        }
        assert_eq!(g.bottom_id(), 0);
    }

    #[test]
    fn locate_maps_visible_and_scrollback() {
        // vis=2, cap=4 → 2 rows of history budget.
        let mut g = Grid::new(2, 3, 4);
        // Scroll enough times to give us a non-zero bottom_id with
        // headroom on either side for the assertions below.
        for _ in 0..10 {
            g.scroll_up();
        }
        let b = g.bottom_id();
        // bottom row (delta 0) = visible row 1
        assert_eq!(g.locate(b), Some(RowLocation::Visible(1)));
        // one above bottom (delta 1) = visible row 0
        assert_eq!(g.locate(b - 1), Some(RowLocation::Visible(0)));
        // two above (delta 2) = scrollback 0 (most recent)
        assert_eq!(g.locate(b - 2), Some(RowLocation::Scrollback(0)));
        // three above (delta 3) = scrollback 1
        assert_eq!(g.locate(b - 3), Some(RowLocation::Scrollback(1)));
        // older than retained → None
        assert_eq!(g.locate(b - 4), None);
        // newer than bottom → None
        assert_eq!(g.locate(b + 1), None);
    }

    #[test]
    fn locate_after_eviction_returns_none_for_evicted() {
        // cap=3, vis=1 → 2 rows of history budget.
        let mut g = Grid::new(1, 3, 3);
        let first_bottom = g.bottom_id();
        // Scroll up four times — three rows get assigned ids
        // first_bottom..=first_bottom+3, but only the most recent two
        // can be retained as scrollback (history budget = 2).
        g.scroll_up();
        g.scroll_up();
        g.scroll_up();
        g.scroll_up();
        let b = g.bottom_id();
        assert_eq!(b, first_bottom + 4);
        // The very oldest scrollback we can reach:
        assert_eq!(g.oldest_retained_id(), b - 2);
        // Visible + 2 scrollback rows are accessible:
        assert!(g.locate(b).is_some());
        assert!(g.locate(b - 1).is_some());
        assert!(g.locate(b - 2).is_some());
        // Anything older has been evicted from the ring.
        assert_eq!(g.locate(b - 3), None);
        assert_eq!(g.locate(first_bottom), None);
    }

    #[test]
    fn resize_keeps_top_row_line_id_stable() {
        // Regression: before the bottom_id fixup in `resize`, a window
        // grow from 24 → 31 rows left `bottom_id` at the old value
        // (23). Under the new (larger) visible_rows, the top rows'
        // computed line_ids underflowed, saturated to 0, and every
        // click above row 8 collapsed to the same line — selection
        // appeared frozen on the old top-row boundary.
        let mut g = Grid::new(24, 80, 24 + 100);
        let initial_top_id = g.bottom_id() - 23; // visible row 0
        // Grow visible region (same-cap path).
        g.resize(31, 80, 24 + 100);
        // Visible row 0 must still have the same line_id; row 30 (the
        // new live bottom) gets a fresh id beyond the old bottom.
        let new_bottom = g.bottom_id();
        let new_top = new_bottom - 30;
        assert_eq!(new_top, initial_top_id, "top row line_id drifted on grow");
        // Shrink back; live bottom moves down accordingly.
        g.resize(20, 80, 24 + 100);
        let after_shrink_top = g.bottom_id() - 19;
        assert_eq!(
            after_shrink_top, initial_top_id,
            "top row line_id drifted on shrink"
        );
    }

    #[test]
    fn oldest_retained_id_with_empty_history() {
        let g = Grid::new(3, 3, 5);
        // history_lines == 0, visible == 3 → oldest retained is 2 above bottom.
        assert_eq!(g.oldest_retained_id(), 0); // bottom_id starts at 0; saturates.
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

    // ---------- resize_reflow (primary grid) ----------

    #[test]
    fn resize_reflow_preserves_scrollback_across_cap_change() {
        // vis=2, cap=12 → budget 10. Build 6 scrollback rows a..f.
        let mut g = Grid::new(2, 4, 12);
        for ch in ['a', 'b', 'c', 'd', 'e', 'f'] {
            put_at(&mut g, 0, ch);
            g.scroll_up();
        }
        assert_eq!(g.history_lines(), 6);
        // Grow the visible region 2→4 (cap 12→14). The old realloc path
        // dropped all history here; reflow preserves it, revealing some at
        // the top of the now-taller viewport.
        let (crow, _ccol) = g.resize_reflow(4, 4, 14, (0, 0));
        assert_eq!(g.cols(), 4);
        assert_eq!(g.visible_rows(), 4);
        // 3 rows revealed into the viewport; 3 remain in scrollback.
        assert_eq!(g.history_lines(), 3);
        assert_eq!(g.row(0).cells[0].ch, 'd');
        assert_eq!(g.row(2).cells[0].ch, 'f');
        // Oldest scrollback ('a') still retained, not evicted.
        assert_eq!(g.scrollback_row(2).unwrap().cells[0].ch, 'a');
        assert!(crow < 4, "cursor row must stay in the new viewport");
    }

    #[test]
    fn resize_reflow_keeps_locate_self_consistent() {
        // After a reflow the live bottom must still resolve to the last
        // visible row and the retained id window must agree with the new
        // (visible_rows, history_lines). (Replaces the old top-row line_id
        // stability test — a full rewrap can't preserve per-line ids.)
        let mut g = Grid::new(4, 8, 4 + 20);
        for ch in ['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h'] {
            put_at(&mut g, 0, ch);
            g.scroll_up();
        }
        let before = g.bottom_id();
        let _ = g.resize_reflow(6, 8, 6 + 20, (0, 0));
        assert!(g.bottom_id() >= before, "bottom_id must not decrease");
        assert_eq!(
            g.locate(g.bottom_id()),
            Some(RowLocation::Visible(g.visible_rows() - 1)),
            "live bottom resolves to the last visible row"
        );
        let oldest = g.oldest_retained_id();
        let span = u64::from(g.history_lines()) + u64::from(g.visible_rows() - 1);
        assert_eq!(g.bottom_id() - oldest, span);
    }

    #[test]
    fn resize_reflow_bumps_bottom_id_when_history_grows() {
        // Regression: a reflow that grows scrollback from a small
        // `bottom_id` (no prior scrolling) must raise `bottom_id` so
        // `oldest_retained_id` doesn't saturate to 0 and collapse the id
        // space. Build a soft-wrapped logical line "abcdefgh" across the 2
        // visible rows of a fresh grid (bottom_id == 1).
        let mut g = Grid::new(2, 4, 2 + 50);
        let write = |g: &mut Grid, row: u16, s: &str| {
            for (col, ch) in s.chars().enumerate() {
                #[allow(clippy::cast_possible_truncation)]
                g.row_mut(row).put(
                    col as u16,
                    Cell {
                        ch,
                        style: Style::RESET,
                        is_continuation: false,
                        hyperlink_id: None,
                    },
                    4,
                );
            }
        };
        write(&mut g, 0, "abcd");
        g.row_mut(0).soft_wrap = true;
        write(&mut g, 1, "efgh");
        assert_eq!(g.bottom_id(), 1);
        // Narrow to 2 cols, 1 visible row → "abcdefgh" rewraps to 4 rows,
        // 3 of which become scrollback.
        let _ = g.resize_reflow(1, 2, 1 + 50, (0, 0));
        assert_eq!(g.history_lines(), 3);
        let span = u64::from(g.history_lines()) + u64::from(g.visible_rows() - 1);
        // The retained id window must bound all 4 retained rows. Before the
        // fix this was 1 (bottom_id stayed 1, oldest saturated to 0).
        assert_eq!(
            g.bottom_id() - g.oldest_retained_id(),
            span,
            "retained id window must bound every retained row"
        );
        assert_eq!(g.locate(g.bottom_id()), Some(RowLocation::Visible(0)));
    }
}
