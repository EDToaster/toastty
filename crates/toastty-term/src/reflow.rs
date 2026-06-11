//! Reflow: re-wrap soft-wrapped logical lines when the grid resizes
//! (the decision #6 / `scrollback.md` follow-up).
//!
//! These are pure functions over `Row`/`Cell` slices — no `Grid` state —
//! so they can be unit-tested in isolation. [`Grid::resize_reflow`] is the
//! orchestrator that pulls retained rows out of the ring, runs them
//! through here, and rebuilds the ring.
//!
//! The wrap rules here mirror `Term::print_char` exactly, NOT the
//! `scrollback.md` prototype's `wrap_line`:
//!   - a width-2 cluster that would land on the last column starts a fresh
//!     row, leaving the last column blank — there is **no explicit spacer
//!     cell** (toastty never emits one);
//!   - every produced row except the last of a logical line gets
//!     `soft_wrap = true`.

use crate::cell::Cell;
use crate::grid::Row;
use smallvec::SmallVec;

/// Display width of the cluster starting at `cells[i]`: 2 when the next
/// cell is the continuation half of a width-2 cluster, else 1.
///
/// Width is derived from the continuation marker (the grid's source of
/// truth for the renderer), not from `unicode-width` — so a lone wide
/// primary whose continuation was lost counts as width 1 and never
/// straddles a wrap boundary.
fn cluster_width(cells: &[Cell], i: usize) -> u16 {
    if cells.get(i + 1).is_some_and(|c| c.is_continuation) {
        2
    } else {
        1
    }
}

/// Number of trailing cells equal to `Cell::BLANK`. Only exact-default
/// blanks count, so a cell erased to a coloured background (non-default
/// style) or a hyperlinked space is preserved.
fn trailing_blank_run(cells: &[Cell]) -> usize {
    cells.iter().rev().take_while(|c| **c == Cell::BLANK).count()
}

/// Build logical lines from a flat, oldest→newest slice of retained rows.
///
/// A logical line is a maximal run of rows in which every row but the last
/// has `soft_wrap == true`. The content cells are concatenated:
///   - a non-final (soft-wrapped) row keeps **all** its cells — a trailing
///     blank there is a real space at the wrap point, not padding (see the
///     trim comment in the body);
///   - the final row drops **all** trailing `Cell::BLANK` (empty space to
///     the right of the last glyph).
///
/// `cursor_idx` / `cursor_col` locate the cursor within `rows`. Returns
/// the logical lines plus `(line_index, content_offset)` for the cursor —
/// the offset is a count of content cells (columns), computed against the
/// cursor row's length *before* final-row trimming so a cursor parked in
/// trailing whitespace maps to the line end rather than into trimmed cells.
fn build_logical_lines(
    rows: &[Row],
    cursor_idx: usize,
    cursor_col: u16,
) -> (Vec<Vec<Cell>>, (usize, usize)) {
    let mut lines: Vec<Vec<Cell>> = Vec::new();
    let mut cursor_line = 0usize;
    let mut cursor_offset = 0usize;

    let mut i = 0usize;
    while i < rows.len() {
        let mut content: Vec<Cell> = Vec::new();
        loop {
            let row = &rows[i];
            let is_last = !row.soft_wrap;
            // A soft-wrapped (non-final) row keeps ALL its cells. A trailing
            // blank there is almost always a real space sitting at the wrap
            // point ("foo bar" wrapping right after the space) — dropping it
            // would merge words into "foobar". The only case it isn't real
            // is a width-2 cluster bumped off the last column, which leaves
            // an unwritten blank there; that's indistinguishable from a real
            // space without a spacer flag, so we keep it. The cost is a rare
            // cosmetic extra space before a margin-wrapped wide char on
            // widen — never data loss. The final (hard-end) row drops all
            // trailing blanks (empty space to the right of the last glyph).
            let trim = if is_last {
                trailing_blank_run(&row.cells)
            } else {
                0
            };
            let keep = row.cells.len() - trim;

            if i == cursor_idx {
                cursor_line = lines.len();
                // Clamp to `keep`: a final-row cursor parked in trailing
                // whitespace maps to the content end (pending-wrap) rather
                // than into trimmed-away cells.
                cursor_offset = content.len() + (cursor_col as usize).min(keep);
            }

            content.extend_from_slice(&row.cells[..keep]);

            // `soft_wrap` on the final retained row (its continuation was
            // truncated away as blank padding) terminates the line here.
            if is_last || i + 1 >= rows.len() {
                i += 1;
                break;
            }
            i += 1;
        }
        lines.push(content);
    }

    (lines, (cursor_line, cursor_offset))
}

/// Re-wrap one logical line's content cells into physical [`Row`]s at
/// `cols`. Always yields at least one row; every row but the last has
/// `soft_wrap = true`.
///
/// When `cursor_offset` is `Some`, returns the `(row_within_output, col)`
/// the cursor maps to. An offset at or past `content.len()` returns the
/// pending-wrap form — `col` may equal `cols` (the sentinel
/// `Term::clamp_cursor` accepts) when the last row is exactly full.
fn wrap_line(
    content: &[Cell],
    cols: u16,
    cursor_offset: Option<usize>,
) -> (Vec<Row>, Option<(usize, u16)>) {
    let cols_usize = cols as usize;
    let mut rows: Vec<Row> = Vec::new();
    let mut cur: Vec<Cell> = Vec::new();
    let mut cursor_pos: Option<(usize, u16)> = None;

    let finish_row = |rows: &mut Vec<Row>, cur: &mut Vec<Cell>, soft_wrap: bool| {
        cur.resize(cols_usize, Cell::BLANK);
        rows.push(Row {
            cells: SmallVec::from_vec(std::mem::take(cur)),
            soft_wrap,
        });
    };

    let mut i = 0usize;
    while i < content.len() {
        let w = cluster_width(content, i);
        let cluster_len = w as usize; // width-2 cluster spans 2 cells, width-1 spans 1.
        if cur.len() + w as usize > cols_usize {
            finish_row(&mut rows, &mut cur, true);
        }
        if let Some(off) = cursor_offset
            && cursor_pos.is_none()
            && off >= i
            && off < i + cluster_len
        {
            #[allow(clippy::cast_possible_truncation)]
            let col = cur.len() as u16;
            cursor_pos = Some((rows.len(), col));
        }
        for k in 0..cluster_len {
            if let Some(c) = content.get(i + k) {
                cur.push(*c);
            }
        }
        i += cluster_len;
    }

    // Cursor at/after the end of content → pending-wrap / end position on
    // the in-progress (soon to be last) row.
    if let Some(off) = cursor_offset
        && cursor_pos.is_none()
        && off >= content.len()
    {
        #[allow(clippy::cast_possible_truncation)]
        let col = cur.len() as u16;
        cursor_pos = Some((rows.len(), col));
    }

    finish_row(&mut rows, &mut cur, false);
    (rows, cursor_pos)
}

/// Reflow a flat, oldest→newest slice of retained rows to `cols`.
///
/// Returns the rewrapped physical rows (oldest→newest) plus the cursor's
/// `(row_in_output, col)`. `cursor_idx` / `cursor_col` locate the cursor
/// within `rows`. The output is never empty (a degenerate all-blank input
/// yields a single blank row with the cursor at `(0, 0)`).
pub(crate) fn reflow_rows(
    rows: &[Row],
    cols: u16,
    cursor_idx: usize,
    cursor_col: u16,
) -> (Vec<Row>, (usize, u16)) {
    let (lines, (cursor_line, cursor_offset)) = build_logical_lines(rows, cursor_idx, cursor_col);

    let mut out: Vec<Row> = Vec::new();
    let mut cur_idx = 0usize;
    let mut cur_col = 0u16;
    for (li, line) in lines.iter().enumerate() {
        let want = if li == cursor_line {
            Some(cursor_offset)
        } else {
            None
        };
        let (wrapped, cpos) = wrap_line(line, cols, want);
        if let Some((r, c)) = cpos {
            cur_idx = out.len() + r;
            cur_col = c;
        }
        out.extend(wrapped);
    }

    if out.is_empty() {
        out.push(Row::blank(cols));
        cur_idx = 0;
        cur_col = 0;
    }

    (out, (cur_idx, cur_col))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::{Color, Style};

    fn cell(ch: char) -> Cell {
        Cell {
            ch,
            style: Style::RESET,
            is_continuation: false,
            hyperlink_id: None,
        }
    }

    /// A width-2 cluster: primary `ch` followed by a continuation cell.
    fn wide(ch: char) -> [Cell; 2] {
        let cont = Cell {
            is_continuation: true,
            ..cell('\0')
        };
        [cell(ch), cont]
    }

    fn red_blank() -> Cell {
        Cell {
            ch: ' ',
            style: Style {
                bg: Color::Red,
                ..Style::RESET
            },
            is_continuation: false,
            hyperlink_id: None,
        }
    }

    fn row(cells: Vec<Cell>, soft_wrap: bool) -> Row {
        Row {
            cells: SmallVec::from_vec(cells),
            soft_wrap,
        }
    }

    fn text(cells: &[Cell]) -> String {
        cells
            .iter()
            .filter(|c| !c.is_continuation)
            .map(|c| c.ch)
            .collect()
    }

    #[test]
    fn cluster_width_reads_continuation_marker() {
        let cells = vec![cell('a'), wide('世')[0], wide('世')[1], cell('b')];
        assert_eq!(cluster_width(&cells, 0), 1); // 'a'
        assert_eq!(cluster_width(&cells, 1), 2); // '世' primary, next is cont
        assert_eq!(cluster_width(&cells, 3), 1); // 'b'
    }

    #[test]
    fn wrap_line_narrow_then_widen_roundtrip() {
        let content: Vec<Cell> = "hello world".chars().map(cell).collect();
        // Narrow to 4: "hell" / "o wo" / "rld".
        let (narrow, _) = wrap_line(&content, 4, None);
        assert_eq!(narrow.len(), 3);
        assert!(narrow[0].soft_wrap && narrow[1].soft_wrap);
        assert!(!narrow[2].soft_wrap);
        // Rejoin the narrowed rows (soft rows keep all cells; final row
        // drops trailing blanks) and widen back to 11.
        let mut rejoined: Vec<Cell> = Vec::new();
        for (idx, r) in narrow.iter().enumerate() {
            let trim = if idx + 1 < narrow.len() {
                0
            } else {
                trailing_blank_run(&r.cells)
            };
            rejoined.extend_from_slice(&r.cells[..r.cells.len() - trim]);
        }
        let (wide_again, _) = wrap_line(&rejoined, 11, None);
        assert_eq!(wide_again.len(), 1);
        assert_eq!(text(&wide_again[0].cells).trim_end(), "hello world");
    }

    #[test]
    fn wrap_line_wide_char_never_straddles_boundary() {
        // 3 ASCII then a wide char, wrapped at 4: the wide char would land
        // on the last column, so it starts a fresh row and col 3 of row 0
        // is left blank.
        let mut content = vec![cell('a'), cell('b'), cell('c')];
        content.extend_from_slice(&wide('世'));
        let (rows, _) = wrap_line(&content, 4, None);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].soft_wrap);
        assert_eq!(rows[0].cells[3], Cell::BLANK, "last col left blank, no spacer");
        assert_eq!(rows[1].cells[0].ch, '世');
        assert!(rows[1].cells[1].is_continuation);
    }

    #[test]
    fn wrap_line_pending_wrap_maps_to_sentinel() {
        // Content exactly fills a width-4 row; cursor at the end (pending
        // wrap) maps to (last_row, cols).
        let content: Vec<Cell> = "abcd".chars().map(cell).collect();
        let (rows, cpos) = wrap_line(&content, 4, Some(content.len()));
        assert_eq!(rows.len(), 1);
        assert_eq!(cpos, Some((0, 4)));
    }

    #[test]
    fn wrap_line_cursor_in_middle() {
        let content: Vec<Cell> = "hello".chars().map(cell).collect();
        // cols 3: "hel" / "lo"; cursor at offset 4 ('o') → row 1, col 1.
        let (_rows, cpos) = wrap_line(&content, 3, Some(4));
        assert_eq!(cpos, Some((1, 1)));
    }

    #[test]
    fn build_logical_lines_trims_final_trailing_blanks_keeps_coloured() {
        // "hi" + red blank + default blanks, hard line end.
        let mut cells = vec![cell('h'), cell('i'), red_blank()];
        cells.push(Cell::BLANK);
        cells.push(Cell::BLANK);
        let rows = vec![row(cells, false)];
        let (lines, _) = build_logical_lines(&rows, 0, 0);
        assert_eq!(lines.len(), 1);
        // Default trailing blanks dropped; the red-bg blank survives.
        assert_eq!(lines[0].len(), 3);
        assert_eq!(lines[0][2], red_blank());
    }

    #[test]
    fn build_logical_lines_soft_wrap_keeps_trailing_space() {
        // A soft-wrapped row's trailing blank is a real space at the wrap
        // point — it must survive the join, not be eaten. ("foo bar"
        // wrapping after the space must NOT rejoin to "foobar".)
        let cells = vec![cell('f'), cell('o'), cell('o'), cell(' ')];
        let rows = vec![row(cells, true), row(vec![cell('b'), cell('a'), cell('r')], false)];
        let (lines, _) = build_logical_lines(&rows, 0, 0);
        assert_eq!(lines.len(), 1);
        assert_eq!(text(&lines[0]), "foo bar");
    }

    #[test]
    fn reflow_rows_foo_bar_narrow_then_widen_keeps_space() {
        // End-to-end regression: "foo bar" narrowed (so it wraps after the
        // space) then widened must come back as "foo bar", not "foobar".
        let content: Vec<Cell> = "foo bar".chars().map(cell).collect();
        let rows = vec![row(content, false)];
        let (narrow, _) = reflow_rows(&rows, 4, 0, 0);
        // "foo " (soft) / "bar".
        assert!(narrow[0].soft_wrap);
        let (widened, _) = reflow_rows(&narrow, 20, 0, 0);
        assert_eq!(widened.len(), 1);
        assert_eq!(text(&widened[0].cells).trim_end(), "foo bar");
    }

    #[test]
    fn build_logical_lines_joins_soft_wrap_chain() {
        let rows = vec![
            row(vec![cell('a'), cell('b')], true),
            row(vec![cell('c'), cell('d')], true),
            row(vec![cell('e')], false),
        ];
        let (lines, _) = build_logical_lines(&rows, 0, 0);
        assert_eq!(lines.len(), 1);
        assert_eq!(text(&lines[0]), "abcde");
    }

    #[test]
    fn reflow_rows_empty_input_yields_one_blank_row() {
        let rows = vec![row(vec![Cell::BLANK, Cell::BLANK], false)];
        let (out, cursor) = reflow_rows(&rows, 4, 0, 0);
        assert_eq!(out.len(), 1);
        assert!(out[0].cells.iter().all(|c| *c == Cell::BLANK));
        assert_eq!(cursor, (0, 0));
    }

    #[test]
    fn reflow_rows_narrow_then_widen_restores_glyphs() {
        // One logical line "hello world" on a wide row.
        let content: Vec<Cell> = "hello world".chars().map(cell).collect();
        let rows = vec![row(content, false)];
        let (narrow, _) = reflow_rows(&rows, 4, 0, 0);
        assert!(narrow.len() >= 3);
        let (widened, _) = reflow_rows(&narrow, 11, 0, 0);
        assert_eq!(widened.len(), 1);
        assert_eq!(text(&widened[0].cells).trim_end(), "hello world");
    }
}
