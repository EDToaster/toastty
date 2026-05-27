//! Mouse text selection model.
//!
//! Selection endpoints are stored as `(line_id, col)` so they stay
//! pinned to a row across `scroll_up`/`scroll_down` and survive
//! scrollback eviction (at which point they simply stop matching any
//! retained row — `Grid::locate` returns `None`).
//!
//! Three contains-modes:
//!  - `Char`: row-major range, top-leftmost cell first.
//!  - `Word`/`Line`: the anchor and active are expanded to their
//!    granularity boundaries at the call site; from this module's
//!    perspective they behave like `Char` — the endpoints are simply
//!    pre-extended.
//!  - `Block`: rectangular `min_line..=max_line × min_col..=max_col`.
//!
//! The selection has *visual* ordering (top of the screen first), which
//! is the opposite of `line_id` ordering (older line_ids are visually
//! higher because new rows push old ones up). [`Selection::ordered`]
//! returns the pair `(first, last)` in visual order; reading `first`
//! goes top-down and left-to-right.

use std::ops::RangeInclusive;

/// A row/column position pinned to a stable line id. See module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pos {
    pub line_id: u64,
    pub col: u16,
}

impl Pos {
    #[must_use]
    pub fn new(line_id: u64, col: u16) -> Self {
        Self { line_id, col }
    }
}

/// Selection granularity. Set at construction; the binary chooses based
/// on click-count and modifier state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMode {
    /// Per-cell drag selection — row-major range.
    Char,
    /// Word selection (double-click). Endpoints are pre-extended to
    /// word boundaries; we still walk it row-major.
    Word,
    /// Line selection (triple-click). Endpoints span full row width.
    Line,
    /// Rectangular block selection (Alt+drag).
    Block,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: Pos,
    pub active: Pos,
    pub mode: SelectionMode,
}

impl Selection {
    #[must_use]
    pub fn new(anchor: Pos, mode: SelectionMode) -> Self {
        Self {
            anchor,
            active: anchor,
            mode,
        }
    }

    pub fn set_active(&mut self, active: Pos) {
        self.active = active;
    }

    /// Return endpoints in visual order: `(first, last)` where `first`
    /// is the top-leftmost cell. Visual "first" has the higher
    /// `line_id` (since older rows have lower ids and sit visually
    /// higher) — wait, the opposite. Newer rows have higher ids and
    /// sit at the *bottom*. So `first` has the *lower* line_id.
    #[must_use]
    pub fn ordered(&self) -> (Pos, Pos) {
        let (a, b) = (self.anchor, self.active);
        // Order by (line_id, col). Lower line_id = higher up on screen.
        if (a.line_id, a.col) <= (b.line_id, b.col) {
            (a, b)
        } else {
            (b, a)
        }
    }

    /// True iff `(line_id, col)` is part of the selection.
    #[must_use]
    pub fn contains(&self, line_id: u64, col: u16) -> bool {
        let (first, last) = self.ordered();
        match self.mode {
            SelectionMode::Block => {
                let (min_col, max_col) = if first.col <= last.col {
                    (first.col, last.col)
                } else {
                    (last.col, first.col)
                };
                line_id >= first.line_id
                    && line_id <= last.line_id
                    && col >= min_col
                    && col <= max_col
            }
            SelectionMode::Char | SelectionMode::Word | SelectionMode::Line => {
                if line_id < first.line_id || line_id > last.line_id {
                    return false;
                }
                if first.line_id == last.line_id {
                    col >= first.col && col <= last.col
                } else if line_id == first.line_id {
                    col >= first.col
                } else if line_id == last.line_id {
                    col <= last.col
                } else {
                    true
                }
            }
        }
    }

    /// Inclusive range of line ids the selection touches. Used for
    /// dirty-marking when the selection changes shape.
    #[must_use]
    pub fn rows_touched(&self) -> RangeInclusive<u64> {
        let (first, last) = self.ordered();
        first.line_id..=last.line_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(line: u64, col: u16) -> Pos {
        Pos::new(line, col)
    }

    #[test]
    fn ordered_returns_visual_first_last() {
        // Anchor older (lower id, visually higher), active newer (higher id, lower).
        let s = Selection {
            anchor: p(5, 3),
            active: p(8, 1),
            mode: SelectionMode::Char,
        };
        let (first, last) = s.ordered();
        assert_eq!(first, p(5, 3));
        assert_eq!(last, p(8, 1));
        // Swapping anchor/active doesn't change ordered() output.
        let s2 = Selection {
            anchor: p(8, 1),
            active: p(5, 3),
            mode: SelectionMode::Char,
        };
        assert_eq!(s2.ordered(), (p(5, 3), p(8, 1)));
    }

    #[test]
    fn ordered_same_row_uses_col() {
        let s = Selection {
            anchor: p(7, 9),
            active: p(7, 2),
            mode: SelectionMode::Char,
        };
        assert_eq!(s.ordered(), (p(7, 2), p(7, 9)));
    }

    #[test]
    fn contains_char_single_row() {
        let s = Selection {
            anchor: p(7, 3),
            active: p(7, 9),
            mode: SelectionMode::Char,
        };
        assert!(!s.contains(7, 2));
        assert!(s.contains(7, 3));
        assert!(s.contains(7, 6));
        assert!(s.contains(7, 9));
        assert!(!s.contains(7, 10));
        assert!(!s.contains(6, 5));
        assert!(!s.contains(8, 5));
    }

    #[test]
    fn contains_char_multi_row_row_major() {
        let s = Selection {
            anchor: p(5, 4),
            active: p(7, 2),
            mode: SelectionMode::Char,
        };
        // First row: from col 4 to end (no upper bound in middle/first).
        assert!(!s.contains(5, 3));
        assert!(s.contains(5, 4));
        assert!(s.contains(5, 100));
        // Middle row: every col selected.
        assert!(s.contains(6, 0));
        assert!(s.contains(6, 200));
        // Last row: up to col 2.
        assert!(s.contains(7, 0));
        assert!(s.contains(7, 2));
        assert!(!s.contains(7, 3));
        // Outside the row range.
        assert!(!s.contains(4, 4));
        assert!(!s.contains(8, 0));
    }

    #[test]
    fn contains_block_rectangle() {
        let s = Selection {
            anchor: p(5, 2),
            active: p(8, 6),
            mode: SelectionMode::Block,
        };
        for line in 5..=8 {
            for col in 2..=6 {
                assert!(s.contains(line, col), "line={line} col={col}");
            }
        }
        // Outside the rectangle (rows match, cols don't):
        assert!(!s.contains(5, 1));
        assert!(!s.contains(5, 7));
        // Outside the rectangle (cols match, rows don't):
        assert!(!s.contains(4, 4));
        assert!(!s.contains(9, 4));
    }

    #[test]
    fn contains_block_reverse_drag() {
        // Anchor in bottom-right, active in top-left — must still form
        // the same rectangle.
        let s = Selection {
            anchor: p(8, 6),
            active: p(5, 2),
            mode: SelectionMode::Block,
        };
        assert!(s.contains(6, 4));
        assert!(s.contains(5, 2));
        assert!(s.contains(8, 6));
        assert!(!s.contains(5, 1));
    }

    #[test]
    fn rows_touched_inclusive() {
        let s = Selection {
            anchor: p(5, 0),
            active: p(8, 0),
            mode: SelectionMode::Char,
        };
        let r = s.rows_touched();
        assert_eq!(*r.start(), 5);
        assert_eq!(*r.end(), 8);
    }

    #[test]
    fn set_active_updates_endpoint() {
        let mut s = Selection::new(p(5, 0), SelectionMode::Char);
        assert_eq!(s.active, p(5, 0));
        s.set_active(p(6, 4));
        assert_eq!(s.active, p(6, 4));
        assert_eq!(s.anchor, p(5, 0));
    }
}
