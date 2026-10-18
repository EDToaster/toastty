//! Link hit-testing and the hover span for Ctrl/Cmd+click link opening.
//!
//! Two kinds of link live in the grid:
//!
//! - **OSC 8 hyperlinks**: cells carry a [`crate::HyperlinkId`]; the span
//!   is the contiguous run of cells with that id.
//! - **Detected URLs**: plain text recognised by
//!   [`toastty_protocols::url_detect`]. The scanner runs over the
//!   *logical* line around the hovered cell (rows joined by
//!   `row.soft_wrap`), so a URL broken by the right margin is one link.
//!
//! OSC 8 wins when a cell has both, because the application said
//! explicitly where it points. Both are addressed by stable
//! `(line_id, col)` [`Pos`]itions, so hit-testing works the same on the
//! live screen and in scrollback while the view is scrolled back.
//!
//! The hover span ([`Term::set_link_hover`]) is display state only: the
//! renderer underlines the cells it covers. Setting or clearing it marks
//! the affected rows dirty so the partial-redraw path repaints them and
//! no stale underline survives.

use std::ops::RangeInclusive;

use toastty_protocols::url_detect;

use crate::selection::Pos;
use crate::term::Term;

/// How many rows on either side of the hovered row are joined into the
/// scanned text. Bounds the cost of a hover over a huge soft-wrapped line
/// (minified JSON, `cat` of a binary); a URL longer than `2 * 16 + 1`
/// rows is not worth a quadratic scan.
pub const MAX_SCAN_ROWS: u64 = 16;

/// Inclusive, row-major range of cells covered by one link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkSpan {
    /// First cell (top-left in reading order).
    pub start: Pos,
    /// Last cell, inclusive. For a wide character this is its primary
    /// (left) cell; the renderer widens the underline over its partner.
    pub end: Pos,
}

impl LinkSpan {
    /// True when `(line_id, col)` lies inside the span.
    #[must_use]
    pub fn contains(&self, line_id: u64, col: u16) -> bool {
        if line_id < self.start.line_id || line_id > self.end.line_id {
            return false;
        }
        if line_id == self.start.line_id && col < self.start.col {
            return false;
        }
        !(line_id == self.end.line_id && col > self.end.col)
    }

    /// Line ids of every row the span touches.
    #[must_use]
    pub fn rows(&self) -> RangeInclusive<u64> {
        self.start.line_id..=self.end.line_id
    }
}

/// Where a [`LinkHit`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkSource {
    /// An OSC 8 hyperlink.
    Hyperlink,
    /// A URL detected in plain text.
    Detected,
}

/// A link under a cell: its extent on screen and its target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkHit {
    pub span: LinkSpan,
    pub url: String,
    pub source: LinkSource,
}

impl Term {
    /// The link covering `at`, if any. OSC 8 hyperlinks take precedence
    /// over URLs detected in the text.
    #[must_use]
    pub fn link_at(&self, at: Pos) -> Option<LinkHit> {
        self.hyperlink_at(at).or_else(|| self.detected_url_at(at))
    }

    /// Normalise `at` onto a cell that carries content: the right half of
    /// a wide character maps to its primary (left) cell. `None` when the
    /// row is evicted or `at` is past the row's end.
    fn primary_pos(&self, at: Pos) -> Option<Pos> {
        let row = self.row_by_line_id(at.line_id)?;
        let cell = row.cells.get(usize::from(at.col))?;
        if cell.is_continuation && at.col > 0 {
            return Some(Pos::new(at.line_id, at.col - 1));
        }
        Some(at)
    }

    /// The OSC 8 hyperlink under `at`: the maximal run of cells sharing
    /// its hyperlink id, continuing across soft wraps (continuation cells
    /// of wide characters ride along with their primary).
    #[must_use]
    pub fn hyperlink_at(&self, at: Pos) -> Option<LinkHit> {
        let at = self.primary_pos(at)?;
        let id = self.row_by_line_id(at.line_id)?.cells[usize::from(at.col)].hyperlink_id?;
        let url = self.hyperlink_url(id)?.to_string();
        let (_, cols) = self.size();
        let last_col = cols.saturating_sub(1);
        let same = |p: Pos| -> bool {
            self.row_by_line_id(p.line_id)
                .and_then(|r| r.cells.get(usize::from(p.col)))
                .is_some_and(|c| c.hyperlink_id == Some(id) || c.is_continuation)
        };
        let prev = |p: Pos| -> Option<Pos> {
            if p.col > 0 {
                return Some(Pos::new(p.line_id, p.col - 1));
            }
            let id = p.line_id.checked_sub(1)?;
            self.row_by_line_id(id)
                .filter(|r| r.soft_wrap)
                .map(|_| Pos::new(id, last_col))
        };
        let next = |p: Pos| -> Option<Pos> {
            if p.col < last_col {
                return Some(Pos::new(p.line_id, p.col + 1));
            }
            self.row_by_line_id(p.line_id).filter(|r| r.soft_wrap)?;
            self.row_by_line_id(p.line_id + 1)?;
            Some(Pos::new(p.line_id + 1, 0))
        };
        let mut start = at;
        while let Some(p) = prev(start)
            && same(p)
        {
            start = p;
        }
        // A run can't begin on a continuation cell; step back onto it.
        start = self.primary_pos(start).unwrap_or(start);
        let mut end = at;
        while let Some(p) = next(end)
            && same(p)
        {
            end = p;
        }
        // Report a wide char's primary as the end, like detected URLs.
        end = self.primary_pos(end).unwrap_or(end);
        Some(LinkHit {
            span: LinkSpan { start, end },
            url,
            source: LinkSource::Hyperlink,
        })
    }

    /// The URL detected in plain text under `at`.
    ///
    /// Builds the text of the logical line around `at` (at most
    /// [`MAX_SCAN_ROWS`] rows each way) with a byte → cell map, runs
    /// [`url_detect::url_at`] on it and maps the byte range back to cells.
    /// Graphemes contribute all their codepoints to their one cell; the
    /// blank pad a wide char leaves at a soft-wrapped row's end is
    /// skipped (like copy does) so it doesn't split the URL; unwritten
    /// (NUL) cells read as spaces and so end a URL.
    #[must_use]
    pub fn detected_url_at(&self, at: Pos) -> Option<LinkHit> {
        let at = self.primary_pos(at)?;
        let (lo, hi) = self.logical_line_bounds(at.line_id)?;
        let lo = lo.max(at.line_id.saturating_sub(MAX_SCAN_ROWS));
        let hi = hi.min(at.line_id.saturating_add(MAX_SCAN_ROWS));
        let mut text = String::new();
        // (byte offset where the cell's text starts, cell position)
        let mut map: Vec<(usize, Pos)> = Vec::new();
        for line_id in lo..=hi {
            let row = self.row_by_line_id(line_id)?;
            let pad_col = if line_id < hi
                && row.soft_wrap
                && self
                    .row_by_line_id(line_id + 1)
                    .is_some_and(|next| next.cells.get(1).is_some_and(|c| c.is_continuation))
            {
                row.cells
                    .len()
                    .checked_sub(1)
                    .filter(|&i| matches!(row.cells[i].ch, ' ' | '\0'))
            } else {
                None
            };
            for (i, cell) in row.cells.iter().enumerate() {
                if cell.is_continuation || Some(i) == pad_col {
                    continue;
                }
                let col = u16::try_from(i).unwrap_or(u16::MAX);
                map.push((text.len(), Pos::new(line_id, col)));
                text.push(if cell.ch == '\0' { ' ' } else { cell.ch });
                text.push_str(self.grapheme_tail(cell));
            }
        }
        let idx = map.iter().position(|&(_, p)| p == at)?;
        let range = url_detect::url_at(&text, map[idx].0)?;
        let cell_of = |byte: usize| map[map.partition_point(|&(b, _)| b <= byte) - 1].1;
        Some(LinkHit {
            span: LinkSpan {
                start: cell_of(range.start),
                end: cell_of(range.end - 1),
            },
            url: text[range].to_string(),
            source: LinkSource::Detected,
        })
    }

    /// The span currently drawn as hovered, if any.
    #[must_use]
    pub fn link_hover(&self) -> Option<LinkSpan> {
        self.link_hover
    }

    /// True iff `(line_id, col)` is inside the hovered link span. Cheap
    /// enough for the renderer's per-cell loop.
    #[must_use]
    pub fn is_link_hovered(&self, line_id: u64, col: u16) -> bool {
        self.link_hover.is_some_and(|s| s.contains(line_id, col))
    }

    /// Replace the hovered link span, marking the rows of both the old
    /// and the new span dirty so the underline appears / disappears on
    /// the next (possibly partial) frame. Returns `true` when it changed.
    pub fn set_link_hover(&mut self, span: Option<LinkSpan>) -> bool {
        if span == self.link_hover {
            return false;
        }
        if let Some(old) = self.link_hover {
            self.dirty_line_ids(old.rows());
        }
        if let Some(new) = span {
            self.dirty_line_ids(new.rows());
        }
        self.link_hover = span;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use toastty_parser::Parser;

    fn feed(t: &mut Term, bytes: &[u8]) {
        let mut p = Parser::new();
        p.advance(t, bytes);
    }

    fn term(rows: u16, cols: u16, scrollback: u16, bytes: &[u8]) -> Term {
        let mut t = Term::new(rows, cols, scrollback);
        feed(&mut t, bytes);
        t
    }

    /// Position of visible row `r`, column `c` on the live screen.
    fn vis(t: &Term, r: u16, c: u16) -> Pos {
        let (rows, _) = t.size();
        Pos::new(t.bottom_id() - u64::from(rows - 1 - r), c)
    }

    fn dirty_rows(t: &Term) -> Vec<u16> {
        t.damage().iter_rows().map(|(r, _)| r).collect()
    }

    fn url_at(t: &Term, r: u16, c: u16) -> Option<String> {
        t.link_at(vis(t, r, c)).map(|h| h.url)
    }

    #[test]
    fn span_contains_single_and_multi_row() {
        let s = LinkSpan {
            start: Pos::new(5, 3),
            end: Pos::new(5, 7),
        };
        assert!(!s.contains(5, 2));
        assert!(s.contains(5, 3));
        assert!(s.contains(5, 7));
        assert!(!s.contains(5, 8));
        assert!(!s.contains(4, 5));
        assert!(!s.contains(6, 5));
        let m = LinkSpan {
            start: Pos::new(1, 6),
            end: Pos::new(3, 2),
        };
        assert!(!m.contains(1, 5));
        assert!(m.contains(1, 6));
        assert!(m.contains(1, 79));
        assert!(m.contains(2, 0));
        assert!(m.contains(3, 2));
        assert!(!m.contains(3, 3));
        assert_eq!(m.rows(), 1..=3);
    }

    #[test]
    fn detects_url_on_every_cell_of_it() {
        let t = term(3, 40, 0, b"see https://a.b/c now");
        for c in 4..=16 {
            assert_eq!(
                url_at(&t, 0, c).as_deref(),
                Some("https://a.b/c"),
                "col {c}"
            );
        }
        assert_eq!(url_at(&t, 0, 3), None);
        assert_eq!(url_at(&t, 0, 17), None);
        let hit = t.link_at(vis(&t, 0, 10)).unwrap();
        assert_eq!(hit.source, LinkSource::Detected);
        assert_eq!(hit.span.start, vis(&t, 0, 4));
        assert_eq!(hit.span.end, vis(&t, 0, 16));
    }

    #[test]
    fn trailing_punctuation_not_in_span() {
        let t = term(2, 40, 0, b"(https://a.b/x).");
        let hit = t.link_at(vis(&t, 0, 1)).unwrap();
        assert_eq!(hit.url, "https://a.b/x");
        assert_eq!((hit.span.start.col, hit.span.end.col), (1, 13));
        assert!(t.link_at(vis(&t, 0, 14)).is_none());
        assert!(t.link_at(vis(&t, 0, 15)).is_none());
    }

    #[test]
    fn multiple_urls_on_one_line() {
        let t = term(2, 40, 0, b"http://a.bc x http://d.ef");
        assert_eq!(url_at(&t, 0, 0).as_deref(), Some("http://a.bc"));
        assert_eq!(url_at(&t, 0, 12), None);
        assert_eq!(url_at(&t, 0, 14).as_deref(), Some("http://d.ef"));
    }

    #[test]
    fn url_at_grid_edge() {
        // Ends exactly in the last column, no wrap pending yet.
        let t = term(2, 10, 0, b"http://a.b");
        let hit = t.link_at(vis(&t, 0, 9)).unwrap();
        assert_eq!(hit.url, "http://a.b");
        assert_eq!(hit.span.end, vis(&t, 0, 9));
    }

    #[test]
    fn url_wrapped_across_soft_wrapped_rows() {
        // 10 columns: "go http://" | "example.co" | "m/path x"
        let t = term(4, 10, 0, b"go http://example.com/path x");
        for (r, c) in [(0, 3), (0, 9), (1, 0), (1, 9), (2, 0), (2, 5)] {
            assert_eq!(
                url_at(&t, r, c).as_deref(),
                Some("http://example.com/path"),
                "({r},{c})"
            );
        }
        let hit = t.link_at(vis(&t, 1, 4)).unwrap();
        assert_eq!(hit.span.start, vis(&t, 0, 3));
        assert_eq!(hit.span.end, vis(&t, 2, 5));
        assert!(hit.span.contains(vis(&t, 1, 0).line_id, 0));
        assert_eq!(url_at(&t, 2, 7), None);
    }

    #[test]
    fn hard_newline_ends_url() {
        let t = term(3, 20, 0, b"http://a.b/c\r\ndef");
        assert_eq!(url_at(&t, 0, 0).as_deref(), Some("http://a.b/c"));
        assert_eq!(url_at(&t, 1, 0), None);
    }

    #[test]
    fn wide_chars_and_wrap_pad() {
        // 8 cols: "http://" + 例 doesn't fit in col 7 (needs 2) → pad blank
        // at col 7, 例 wraps to row 1 cols 0-1.
        let t = term(3, 8, 0, "http://例.jp z".as_bytes());
        let want = Some("http://例.jp".to_string());
        assert_eq!(url_at(&t, 0, 0), want);
        assert_eq!(url_at(&t, 1, 0), want);
        // Right half of the wide char maps to its primary.
        assert_eq!(url_at(&t, 1, 1), want);
        assert_eq!(url_at(&t, 1, 4), want);
        let hit = t.link_at(vis(&t, 1, 1)).unwrap();
        assert_eq!(hit.span.end, vis(&t, 1, 4));
        // The pad cell itself is not part of any link.
        assert_eq!(url_at(&t, 0, 7), None);
    }

    #[test]
    fn wide_char_as_last_url_cell() {
        let t = term(2, 20, 0, "https://a.b/例 x".as_bytes());
        let hit = t.link_at(vis(&t, 0, 13)).unwrap();
        assert_eq!(hit.url, "https://a.b/例");
        // End is the wide char's primary column.
        assert_eq!(hit.span.end.col, 12);
    }

    #[test]
    fn grapheme_cells_keep_their_tail() {
        // e + combining acute is one cell holding a grapheme tail.
        let t = term(2, 30, 0, "https://a.b/e\u{301}x y".as_bytes());
        let hit = t.link_at(vis(&t, 0, 13)).unwrap();
        assert_eq!(hit.url, "https://a.b/e\u{301}x");
        assert_eq!((hit.span.start.col, hit.span.end.col), (0, 13));
    }

    #[test]
    fn unwritten_cells_end_the_url() {
        // Cursor jump leaves NUL/blank cells between the URL and more text.
        let t = term(2, 30, 0, b"http://a.b\x1b[1;20Hzzz");
        let hit = t.link_at(vis(&t, 0, 0)).unwrap();
        assert_eq!(hit.url, "http://a.b");
        assert_eq!(url_at(&t, 0, 15), None);
    }

    #[test]
    fn out_of_range_and_evicted_positions() {
        let t = term(2, 10, 0, b"http://a.b");
        assert!(t.link_at(Pos::new(9999, 0)).is_none());
        assert!(t.link_at(Pos::new(t.bottom_id(), 50)).is_none());
    }

    #[test]
    fn works_in_scrollback() {
        let mut t = term(3, 30, 10, b"old http://x.y/z\r\n");
        let url_line = t.bottom_id() - 2;
        feed(&mut t, b"1\r\n2\r\n3\r\n4\r\n");
        // The URL row is now in scrollback; its line id still resolves.
        let hit = t.link_at(Pos::new(url_line, 6)).unwrap();
        assert_eq!(hit.url, "http://x.y/z");
        assert_eq!(hit.span.start, Pos::new(url_line, 4));
    }

    #[test]
    fn long_logical_line_is_windowed() {
        // 60 rows of 'a' then a URL: only rows near the hovered one are
        // joined, but the URL near the hover still resolves.
        let cols = 10u16;
        let mut bytes = vec![b'a'; usize::from(cols) * 60];
        bytes.extend_from_slice(b" http://q.r/s");
        let t = term(70, cols, 0, &bytes);
        assert_eq!(url_at(&t, 60, 3).as_deref(), Some("http://q.r/s"));
        assert_eq!(url_at(&t, 2, 3), None);
    }

    #[test]
    fn osc8_hyperlink_span_and_precedence() {
        // OSC 8 over text that itself looks like a URL: the hyperlink wins.
        let t = term(
            2,
            40,
            0,
            b"x \x1b]8;;https://target.example/\x07http://shown.example\x1b]8;;\x07 y",
        );
        let hit = t.link_at(vis(&t, 0, 5)).unwrap();
        assert_eq!(hit.source, LinkSource::Hyperlink);
        assert_eq!(hit.url, "https://target.example/");
        assert_eq!(hit.span.start, vis(&t, 0, 2));
        assert_eq!(hit.span.end, vis(&t, 0, 21));
        assert!(t.link_at(vis(&t, 0, 22)).is_none());
        assert!(t.hyperlink_at(vis(&t, 0, 0)).is_none());
    }

    #[test]
    fn osc8_hyperlink_across_wrap_with_wide_chars() {
        // 6 cols; link text "ab例cd例ef" wraps.
        let t = term(
            3,
            6,
            0,
            "\x1b]8;;http://w/\x07ab例cd例ef\x1b]8;;\x07!".as_bytes(),
        );
        let hit = t.link_at(vis(&t, 1, 0)).unwrap();
        assert_eq!(hit.url, "http://w/");
        assert_eq!(hit.span.start, vis(&t, 0, 0));
        // Row 0: a b 例 例' c d | row 1: 例 例' e f !
        assert_eq!(hit.span.end, vis(&t, 1, 3));
        // Hovering the continuation half resolves too.
        assert_eq!(t.link_at(vis(&t, 0, 3)).unwrap().span, hit.span);
        // A link starting on a wide char reports its primary as start.
        let t = term(2, 10, 0, "\x1b]8;;http://w/\x07例x\x1b]8;;\x07".as_bytes());
        let hit = t.link_at(vis(&t, 0, 1)).unwrap();
        assert_eq!(hit.span.start.col, 0);
        assert_eq!(hit.span.end.col, 2);
    }

    #[test]
    fn hover_set_clear_and_dirty() {
        let mut t = term(3, 30, 0, b"\r\nhttp://a.b/c");
        let span = t.link_at(vis(&t, 1, 2)).unwrap().span;
        t.clear_damage();
        assert!(t.link_hover().is_none());
        assert!(t.set_link_hover(Some(span)));
        assert!(!t.set_link_hover(Some(span)));
        assert_eq!(t.link_hover(), Some(span));
        assert!(t.is_link_hovered(span.start.line_id, 0));
        assert!(!t.is_link_hovered(span.start.line_id, 20));
        assert!(dirty_rows(&t).contains(&1));
        assert!(!dirty_rows(&t).contains(&0));
        t.clear_damage();
        assert!(t.set_link_hover(None));
        assert!(dirty_rows(&t).contains(&1));
        assert!(!t.is_link_hovered(span.start.line_id, 0));
        assert!(!t.set_link_hover(None));
    }

    #[test]
    fn hover_dropped_on_screen_switch_and_resize() {
        let mut t = term(3, 30, 0, b"http://a.b/c");
        let span = t.link_at(vis(&t, 0, 0)).unwrap().span;
        t.set_link_hover(Some(span));
        feed(&mut t, b"\x1b[?1049h");
        assert!(t.link_hover().is_none());
        t.set_link_hover(Some(span));
        t.resize(4, 20);
        assert!(t.link_hover().is_none());
    }
}
