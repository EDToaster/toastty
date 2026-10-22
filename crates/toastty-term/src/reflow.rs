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
    cells
        .iter()
        .rev()
        .take_while(|c| **c == Cell::BLANK)
        .count()
}

/// Build logical lines from a flat, oldest→newest slice of retained rows.
///
/// A logical line is a maximal run of rows in which every row but the last
/// has `soft_wrap == true`. The content cells are concatenated with
/// trailing-blank trimming:
///   - a non-final (soft-wrapped) row drops **at most one** trailing
///     `Cell::BLANK` (the wide-char wrap pad — see module docs);
///   - the final row drops **all** trailing `Cell::BLANK` (empty space to
///     the right of the last glyph).
///
/// `cursor_idx` / `cursor_col` locate the cursor within `rows`. Returns
/// the logical lines plus `(line_index, content_offset)` for the cursor —
/// the offset is a count of content cells (columns), computed against the
/// cursor row's length *before* final-row trimming so a cursor parked in
/// trailing whitespace maps to the line end rather than into trimmed cells.
///
/// `row_starts` receives, for every input row, the `(line_index,
/// content_offset)` its first cell lands at, so callers can follow a row
/// (e.g. an image anchor) through the rewrap.
fn build_logical_lines(
    rows: &[Row],
    cursor_idx: usize,
    cursor_col: u16,
    row_starts: &mut Vec<(usize, usize)>,
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
            let trim = if is_last {
                trailing_blank_run(&row.cells)
            } else {
                trailing_blank_run(&row.cells).min(1)
            };
            let keep = row.cells.len() - trim;
            row_starts.push((lines.len(), content.len()));

            if i == cursor_idx {
                cursor_line = lines.len();
                // Clamp to `keep`, not the full row width: a cursor parked
                // on a soft-wrapped row's trimmed pad cell must map to the
                // end of this row's surviving content, not overshoot by one
                // into the next row. A final-row cursor in trailing
                // whitespace lands at the content end (pending-wrap).
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
///
/// `out_starts` receives the content offset each output row starts at
/// (ascending; the first is always 0).
fn wrap_line(
    content: &[Cell],
    cols: u16,
    cursor_offset: Option<usize>,
    out_starts: &mut Vec<usize>,
) -> (Vec<Row>, Option<(usize, u16)>) {
    let cols_usize = cols as usize;
    let mut rows: Vec<Row> = Vec::new();
    let mut cur: Vec<Cell> = Vec::new();
    let mut cursor_pos: Option<(usize, u16)> = None;
    out_starts.clear();
    out_starts.push(0);

    let finish_row = |rows: &mut Vec<Row>, cur: &mut Vec<Cell>, soft_wrap: bool| {
        cur.resize(cols_usize, Cell::BLANK);
        rows.push(Row {
            cells: SmallVec::from_vec(std::mem::take(cur)),
            soft_wrap,
            rich: None,
        });
    };

    let mut i = 0usize;
    while i < content.len() {
        let w = cluster_width(content, i);
        let cluster_len = w as usize; // width-2 cluster spans 2 cells, width-1 spans 1.
        if cur.len() + w as usize > cols_usize {
            finish_row(&mut rows, &mut cur, true);
            out_starts.push(i);
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
#[cfg(test)]
pub(crate) fn reflow_rows(
    rows: &[Row],
    cols: u16,
    cursor_idx: usize,
    cursor_col: u16,
) -> (Vec<Row>, (usize, u16)) {
    let mut map = ReflowMap::default();
    reflow_rows_mapped(rows, cols, cursor_idx, cursor_col, &mut map)
}

/// Where a reflow put each input row, for following line-anchored state
/// (image placements) through it. Built by [`reflow_rows_mapped`], which
/// appends, so several segments can share one map.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ReflowMap {
    /// Per input row: the output row its first cell lands on (ascending).
    pub(crate) row_map: Vec<usize>,
    /// Per input row: `(logical line, content offset of its first cell)`,
    /// or `None` for a row that wasn't reflowed as text (a rich block).
    pub(crate) in_pos: Vec<Option<(u32, u32)>>,
    /// Per output text row, ascending: `(logical line, content offset it
    /// starts at, output row)`.
    pub(crate) out_pos: Vec<(u32, u32, u32)>,
    /// Logical lines numbered so far.
    pub(crate) lines: u32,
}

impl ReflowMap {
    /// Output row holding content offset `off` of logical line `line`,
    /// with the offset that row starts at.
    #[must_use]
    pub(crate) fn out_row(&self, line: u32, off: u32) -> Option<(usize, u32)> {
        let i = self
            .out_pos
            .partition_point(|&(l, s, _)| (l, s) <= (line, off));
        let &(l, s, r) = self.out_pos.get(i.checked_sub(1)?)?;
        (l == line).then_some((r as usize, s))
    }
}

/// [`reflow_rows`], also appending to `map` where each input row (and
/// each output row) sits, numbering logical lines from `map.lines` and
/// output rows from `out_base`. Image placements anchored to a row follow
/// it through the rewrap this way.
#[cfg(test)]
pub(crate) fn reflow_rows_mapped(
    rows: &[Row],
    cols: u16,
    cursor_idx: usize,
    cursor_col: u16,
    map: &mut ReflowMap,
) -> (Vec<Row>, (usize, u16)) {
    reflow_rows_mapped_at(rows, cols, cursor_idx, cursor_col, map, 0)
}

/// [`reflow_rows_mapped`] for a segment whose first output row will sit at
/// `out_base` in the caller's output.
#[allow(clippy::cast_possible_truncation)] // row / offset counts fit u32.
pub(crate) fn reflow_rows_mapped_at(
    rows: &[Row],
    cols: u16,
    cursor_idx: usize,
    cursor_col: u16,
    map: &mut ReflowMap,
    out_base: usize,
) -> (Vec<Row>, (usize, u16)) {
    let mut row_starts = Vec::with_capacity(rows.len());
    let (lines, (cursor_line, cursor_offset)) =
        build_logical_lines(rows, cursor_idx, cursor_col, &mut row_starts);
    let line_base = map.lines;
    let map_start = map.row_map.len();

    let mut out: Vec<Row> = Vec::new();
    let mut cur_idx = 0usize;
    let mut cur_col = 0u16;
    let mut out_starts = Vec::new();
    let mut starts = row_starts.iter().peekable();
    for (li, line) in lines.iter().enumerate() {
        let want = if li == cursor_line {
            Some(cursor_offset)
        } else {
            None
        };
        let (wrapped, cpos) = wrap_line(line, cols, want, &mut out_starts);
        if let Some((r, c)) = cpos {
            cur_idx = out.len() + r;
            cur_col = c;
        }
        let gl = line_base + li as u32;
        // Input rows of this line, in order: each lands on the last
        // output row that starts at or before its content offset.
        while let Some(&&(l, off)) = starts.peek() {
            if l != li {
                break;
            }
            let r = out_starts.partition_point(|&s| s <= off).saturating_sub(1);
            map.row_map.push(out_base + out.len() + r);
            map.in_pos.push(Some((gl, off as u32)));
            starts.next();
        }
        for (r, &s) in out_starts.iter().enumerate() {
            map.out_pos
                .push((gl, s as u32, (out_base + out.len() + r) as u32));
        }
        out.extend(wrapped);
    }
    map.lines = line_base + lines.len() as u32;

    if out.is_empty() {
        out.push(Row::blank(cols));
        cur_idx = 0;
        cur_col = 0;
    }
    // Defensive: every input row gets an entry.
    let want = map_start + rows.len();
    map.row_map.resize(want, out_base + out.len() - 1);
    map.in_pos.resize(want, None);

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
            extra: None,
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
            extra: None,
        }
    }

    fn row(cells: Vec<Cell>, soft_wrap: bool) -> Row {
        Row {
            cells: SmallVec::from_vec(cells),
            soft_wrap,
            rich: None,
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
        let (narrow, _) = wrap_line(&content, 4, None, &mut Vec::new());
        assert_eq!(narrow.len(), 3);
        assert!(narrow[0].soft_wrap && narrow[1].soft_wrap);
        assert!(!narrow[2].soft_wrap);
        // Rejoin the narrowed rows and widen back to 11.
        let mut rejoined: Vec<Cell> = Vec::new();
        for (idx, r) in narrow.iter().enumerate() {
            let trim = if idx + 1 < narrow.len() {
                trailing_blank_run(&r.cells).min(1)
            } else {
                trailing_blank_run(&r.cells)
            };
            rejoined.extend_from_slice(&r.cells[..r.cells.len() - trim]);
        }
        let (wide_again, _) = wrap_line(&rejoined, 11, None, &mut Vec::new());
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
        let (rows, _) = wrap_line(&content, 4, None, &mut Vec::new());
        assert_eq!(rows.len(), 2);
        assert!(rows[0].soft_wrap);
        assert_eq!(
            rows[0].cells[3],
            Cell::BLANK,
            "last col left blank, no spacer"
        );
        assert_eq!(rows[1].cells[0].ch, '世');
        assert!(rows[1].cells[1].is_continuation);
    }

    #[test]
    fn wrap_line_pending_wrap_maps_to_sentinel() {
        // Content exactly fills a width-4 row; cursor at the end (pending
        // wrap) maps to (last_row, cols).
        let content: Vec<Cell> = "abcd".chars().map(cell).collect();
        let (rows, cpos) = wrap_line(&content, 4, Some(content.len()), &mut Vec::new());
        assert_eq!(rows.len(), 1);
        assert_eq!(cpos, Some((0, 4)));
    }

    #[test]
    fn wrap_line_cursor_in_middle() {
        let content: Vec<Cell> = "hello".chars().map(cell).collect();
        // cols 3: "hel" / "lo"; cursor at offset 4 ('o') → row 1, col 1.
        let (_rows, cpos) = wrap_line(&content, 3, Some(4), &mut Vec::new());
        assert_eq!(cpos, Some((1, 1)));
    }

    #[test]
    fn build_logical_lines_trims_final_trailing_blanks_keeps_coloured() {
        // "hi" + red blank + default blanks, hard line end.
        let mut cells = vec![cell('h'), cell('i'), red_blank()];
        cells.push(Cell::BLANK);
        cells.push(Cell::BLANK);
        let rows = vec![row(cells, false)];
        let (lines, _) = build_logical_lines(&rows, 0, 0, &mut Vec::new());
        assert_eq!(lines.len(), 1);
        // Default trailing blanks dropped; the red-bg blank survives.
        assert_eq!(lines[0].len(), 3);
        assert_eq!(lines[0][2], red_blank());
    }

    #[test]
    fn build_logical_lines_soft_wrap_drops_one_pad_blank() {
        // A soft-wrapped row ending in two default blanks drops exactly one.
        let cells = vec![cell('a'), cell('b'), Cell::BLANK, Cell::BLANK];
        let rows = vec![row(cells, true), row(vec![cell('c')], false)];
        let (lines, _) = build_logical_lines(&rows, 0, 0, &mut Vec::new());
        assert_eq!(lines.len(), 1);
        // "ab" + one surviving blank (the second was dropped) + "c" = 4 cells.
        assert_eq!(lines[0].len(), 4);
        assert_eq!(lines[0][2], Cell::BLANK);
        assert_eq!(lines[0][3].ch, 'c');
    }

    #[test]
    fn build_logical_lines_joins_soft_wrap_chain() {
        let rows = vec![
            row(vec![cell('a'), cell('b')], true),
            row(vec![cell('c'), cell('d')], true),
            row(vec![cell('e')], false),
        ];
        let (lines, _) = build_logical_lines(&rows, 0, 0, &mut Vec::new());
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

    #[test]
    fn row_map_follows_rows_through_a_rewrap() {
        // "AAAAAA" (one 6-col row), a blank row, then "B": at 3 cols the
        // A line takes two rows, so the blank row moves to 2 and B to 3.
        let rows = vec![
            row((0..6).map(|_| cell('A')).collect(), false),
            row(vec![Cell::BLANK; 6], false),
            row(vec![cell('B')], false),
        ];
        let mut map = ReflowMap::default();
        let (out, _) = reflow_rows_mapped(&rows, 3, 0, 0, &mut map);
        assert_eq!(out.len(), 4);
        assert_eq!(map.row_map, vec![0, 2, 3]);
        assert_eq!(map.in_pos, vec![Some((0, 0)), Some((1, 0)), Some((2, 0))]);
        assert_eq!(
            map.out_pos,
            vec![(0, 0, 0), (0, 3, 1), (1, 0, 2), (2, 0, 3)]
        );
        // Widening back maps the rewrapped rows onto the first again.
        let mut map = ReflowMap::default();
        let (wide, _) = reflow_rows_mapped(&out, 6, 0, 0, &mut map);
        assert_eq!(wide.len(), 3);
        assert_eq!(map.row_map, vec![0, 0, 1, 2]);
        // The second A row started at offset 3: it lands in row 0.
        assert_eq!(map.in_pos[1], Some((0, 3)));
        assert_eq!(map.out_row(0, 3), Some((0, 0)));
    }

    #[test]
    fn row_map_tracks_soft_wrapped_continuation_rows() {
        // "abcd" soft-wrapped onto "ef": at 2 cols the second input row
        // ("ef", content offset 4) lands on output row 2.
        let rows = vec![
            row(vec![cell('a'), cell('b'), cell('c'), cell('d')], true),
            row(vec![cell('e'), cell('f'), Cell::BLANK, Cell::BLANK], false),
        ];
        let mut map = ReflowMap::default();
        let (out, _) = reflow_rows_mapped(&rows, 2, 0, 0, &mut map);
        assert_eq!(out.len(), 3);
        assert_eq!(map.row_map, vec![0, 2]);
        assert_eq!(map.in_pos, vec![Some((0, 0)), Some((0, 4))]);
    }
}
