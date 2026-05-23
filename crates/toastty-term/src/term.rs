//! `Term`: top-level terminal state.
//!
//! Owns the primary and alternate grids, a cursor, and the SGR state in
//! effect. Implements `toastty_parser::Perform` so the parser can drive it
//! directly.

use crate::cell::{Cell, Color, Style};
use crate::cursor::Cursor;
use crate::grid::Grid;
use toastty_parser::{Params, Perform};

/// Width of a hard tab. Eight is the canonical default; once we expose
/// tab-stop manipulation (HTS/TBC) this becomes per-column state.
const TAB_WIDTH: u16 = 8;

/// Top-level terminal state object.
#[derive(Debug)]
pub struct Term {
    primary: Grid,
    alt: Grid,
    cursor: Cursor,
    /// Cursor snapshot captured on the most recent `1049` enter; restored
    /// on `1049` exit.
    saved_cursor: Cursor,
    alt_active: bool,
    rows: u16,
    cols: u16,
    /// Primary-grid scrollback capacity (visible rows + history).
    scrollback: u16,
}

impl Term {
    /// Construct a fresh terminal `rows` rows by `cols` cols, with
    /// `scrollback` additional rows of history available behind the
    /// primary screen. The alt screen uses no scrollback (decision #6).
    #[must_use]
    pub fn new(rows: u16, cols: u16, scrollback: u16) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        let primary_cap = rows as usize + scrollback as usize;
        let primary = Grid::new(rows, cols, primary_cap);
        let alt = Grid::new(rows, cols, rows as usize);
        Self {
            primary,
            alt,
            cursor: Cursor::default(),
            saved_cursor: Cursor::default(),
            alt_active: false,
            rows,
            cols,
            scrollback,
        }
    }

    /// Visible (rows, cols).
    pub fn size(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }

    /// Current cursor (row/col + active SGR style).
    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    /// Borrow visible row `idx` from whichever grid is active.
    pub fn row(&self, idx: u16) -> &crate::grid::Row {
        self.active_grid().row(idx)
    }

    /// True when the alternate screen is currently displayed.
    pub fn is_alt_active(&self) -> bool {
        self.alt_active
    }

    /// Resize the visible viewport. **Does not reflow** — that's a
    /// decision #6 / scrollback.md follow-up. The cursor is clamped to the
    /// new dimensions.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        // TODO(reflow): walk soft-wrap runs and reshape per
        // docs/decisions/scrollback.md. M3 only fixes geometry + cursor.
        let rows = rows.max(1);
        let cols = cols.max(1);
        let primary_cap = rows as usize + self.scrollback as usize;
        self.primary.resize(rows, cols, primary_cap);
        self.alt.resize(rows, cols, rows as usize);
        self.rows = rows;
        self.cols = cols;
        self.clamp_cursor();
    }

    fn active_grid(&self) -> &Grid {
        if self.alt_active {
            &self.alt
        } else {
            &self.primary
        }
    }

    fn active_grid_mut(&mut self) -> &mut Grid {
        if self.alt_active {
            &mut self.alt
        } else {
            &mut self.primary
        }
    }

    fn clamp_cursor(&mut self) {
        if self.cursor.row >= self.rows {
            self.cursor.row = self.rows - 1;
        }
        // `col == cols` is the pending-wrap sentinel (the next print wraps).
        // We allow it on resize / alt restore so we don't silently lose
        // wrap state. Anything strictly past that gets clamped.
        if self.cursor.col > self.cols {
            self.cursor.col = self.cols;
        }
    }

    fn linefeed(&mut self) {
        if self.cursor.row + 1 >= self.rows {
            // At bottom: scroll up by one and stay on the last row.
            self.active_grid_mut().scroll_up();
        } else {
            self.cursor.row += 1;
        }
    }

    fn print_char(&mut self, c: char) {
        // Wrap before printing if the cursor is past the last column.
        if self.cursor.col >= self.cols {
            // Mark the row we're leaving as soft-wrapped (decision #6).
            let leaving = self.cursor.row;
            self.active_grid_mut().row_mut(leaving).soft_wrap = true;
            self.cursor.col = 0;
            self.linefeed();
        }
        let cell = Cell {
            ch: c,
            style: self.cursor.style,
        };
        let col = self.cursor.col;
        let row = self.cursor.row;
        let max_cols = self.cols;
        self.active_grid_mut().row_mut(row).put(col, cell, max_cols);
        self.cursor.col += 1;
    }

    fn handle_csi(&mut self, params: &Params, intermediates: &[u8], action: char) {
        let priv_marker = intermediates.first().copied();
        match action {
            'A' => self.cursor_up(first_param(params, 1).max(1)),
            'B' => self.cursor_down(first_param(params, 1).max(1)),
            'C' => self.cursor_forward(first_param(params, 1).max(1)),
            'D' => self.cursor_back(first_param(params, 1).max(1)),
            'H' | 'f' => {
                let r = first_param(params, 1).max(1);
                let c = nth_param(params, 1, 1).max(1);
                self.cursor_position(r, c);
            }
            'J' => self.erase_display(first_param(params, 0)),
            'K' => self.erase_line(first_param(params, 0)),
            'm' => self.apply_sgr(params),
            'h' if priv_marker == Some(b'?') => self.apply_decset(params, true),
            'l' if priv_marker == Some(b'?') => self.apply_decset(params, false),
            _ => {}
        }
    }

    fn cursor_up(&mut self, n: u16) {
        self.cursor.row = self.cursor.row.saturating_sub(n);
    }

    fn cursor_down(&mut self, n: u16) {
        let r = self.cursor.row.saturating_add(n);
        self.cursor.row = r.min(self.rows.saturating_sub(1));
    }

    fn cursor_forward(&mut self, n: u16) {
        let c = self.cursor.col.saturating_add(n);
        self.cursor.col = c.min(self.cols.saturating_sub(1));
    }

    fn cursor_back(&mut self, n: u16) {
        self.cursor.col = self.cursor.col.saturating_sub(n);
    }

    fn cursor_position(&mut self, row_1based: u16, col_1based: u16) {
        self.cursor.row = row_1based.saturating_sub(1).min(self.rows - 1);
        self.cursor.col = col_1based.saturating_sub(1).min(self.cols - 1);
    }

    fn erase_display(&mut self, mode: u16) {
        let cur_row = self.cursor.row;
        let cur_col = self.cursor.col;
        let cols = self.cols;
        let rows = self.rows;
        let style = self.cursor.style;
        let grid = self.active_grid_mut();
        match mode {
            // 0: cursor to end of screen.
            0 => {
                grid.row_mut(cur_row).erase(cur_col, cols, style);
                for r in (cur_row + 1)..rows {
                    let row = grid.row_mut(r);
                    row.erase(0, cols, style);
                    row.soft_wrap = false;
                }
            }
            // 1: beginning of screen to cursor (inclusive).
            1 => {
                for r in 0..cur_row {
                    let row = grid.row_mut(r);
                    row.erase(0, cols, style);
                    row.soft_wrap = false;
                }
                grid.row_mut(cur_row)
                    .erase(0, cur_col.saturating_add(1), style);
            }
            // 2/3: entire screen (3 = also scrollback, which we treat the same in M3).
            _ => {
                grid.clear_visible(style);
            }
        }
    }

    fn erase_line(&mut self, mode: u16) {
        let cur_row = self.cursor.row;
        let cur_col = self.cursor.col;
        let cols = self.cols;
        let style = self.cursor.style;
        let row = self.active_grid_mut().row_mut(cur_row);
        match mode {
            0 => row.erase(cur_col, cols, style),
            1 => row.erase(0, cur_col.saturating_add(1), style),
            _ => row.erase(0, cols, style),
        }
    }

    fn apply_sgr(&mut self, params: &Params) {
        // `CSI m` (no params) and `CSI 0 m` both reset. `vte 0.15.0` always
        // pushes at least one numeric param even when none was written, but
        // we keep the defensive empty-params branch for direct callers.
        if params.is_empty() {
            self.cursor.style = Style::RESET;
            return;
        }

        // Walk the top-level params one slice at a time. The multi-param
        // SGR introducers (38/48/58) consume one or more *following*
        // top-level params on the legacy semicolon form
        // (`CSI 38;5;N m` → slices `[[38],[5],[N]]`), but read their
        // sub-params from the same slice on the ITU-T T.416 colon form
        // (`CSI 38:5:N m` → slice `[[38,5,N]]`). Both must be supported —
        // virtually every modern app uses the semicolon form, but some
        // (especially with underline color, mode 58) emit the colon form.
        //
        // Critical: the consumed params must NOT also be re-interpreted as
        // standalone SGR codes. The old implementation iterated each
        // top-level slice and called `apply_sgr_param(slice[0])`, which
        // meant a truecolor `\x1b[38;2;200;32;100m` sequence accidentally
        // ran `apply_sgr_param(32)` and set fg green. That's the leak this
        // function exists to fix.
        let mut iter = params.iter();
        while let Some(slice) = iter.next() {
            // Empty top-level params shouldn't occur from vte but treat them
            // as the implicit 0 (reset) the spec requires.
            let head = slice.first().copied().unwrap_or(0);
            match head {
                38 if slice.len() >= 2 => self.cursor.style.fg = parse_extended_color_from_slice(&slice[1..]).unwrap_or(self.cursor.style.fg),
                48 if slice.len() >= 2 => self.cursor.style.bg = parse_extended_color_from_slice(&slice[1..]).unwrap_or(self.cursor.style.bg),
                58 if slice.len() >= 2 => {
                    // Underline color is parsed but not yet stored — we don't
                    // have anywhere to put it. We MUST still consume the
                    // sub-params so they don't leak. The colon form keeps
                    // them in the same slice, so there's nothing to do.
                    let _ = parse_extended_color_from_slice(&slice[1..]);
                }
                38 => {
                    // Semicolon form: consume from the outer iterator.
                    let color = parse_extended_color_from_iter(&mut iter);
                    if let Some(c) = color {
                        self.cursor.style.fg = c;
                    }
                }
                48 => {
                    let color = parse_extended_color_from_iter(&mut iter);
                    if let Some(c) = color {
                        self.cursor.style.bg = c;
                    }
                }
                58 => {
                    // Parse and discard for now — see comment above.
                    let _ = parse_extended_color_from_iter(&mut iter);
                }
                v => self.apply_sgr_param(v),
            }
        }
    }

    fn apply_sgr_param(&mut self, v: u16) {
        let style = &mut self.cursor.style;
        match v {
            0 => *style = Style::RESET,
            1 => style.flags.bold = true,
            3 => style.flags.italic = true,
            4 => style.flags.underline = true,
            7 => style.flags.reverse = true,
            22 => style.flags.bold = false,
            23 => style.flags.italic = false,
            24 => style.flags.underline = false,
            27 => style.flags.reverse = false,
            30..=37 => style.fg = ansi_color(v - 30, false),
            39 => style.fg = Color::Default,
            40..=47 => style.bg = ansi_color(v - 40, false),
            49 => style.bg = Color::Default,
            // 59 (default underline color) is also handled by the wildcard
            // for now — we don't store underline color yet.
            90..=97 => style.fg = ansi_color(v - 90, true),
            100..=107 => style.bg = ansi_color(v - 100, true),
            _ => {}
        }
    }

    fn apply_decset(&mut self, params: &Params, enable: bool) {
        for sub in params {
            if let Some(&code) = sub.first()
                && code == 1049
            {
                if enable {
                    self.enter_alt_screen();
                } else {
                    self.exit_alt_screen();
                }
            }
            // TODO(modes): 1, 7, 12, 25, 1000-series mouse, 2004, 2026,
            // 2027, 2048, etc. live in toastty-protocols.
        }
    }

    fn enter_alt_screen(&mut self) {
        if self.alt_active {
            return;
        }
        self.saved_cursor = self.cursor;
        self.alt_active = true;
        self.alt.clear_visible(Style::RESET);
        // Reset cursor to home and clear style for the alt screen.
        self.cursor = Cursor::default();
    }

    fn exit_alt_screen(&mut self) {
        if !self.alt_active {
            return;
        }
        self.alt_active = false;
        self.cursor = self.saved_cursor;
        self.clamp_cursor();
    }
}

fn first_param(params: &Params, default: u16) -> u16 {
    nth_param(params, 0, default)
}

fn nth_param(params: &Params, n: usize, default: u16) -> u16 {
    params
        .iter()
        .nth(n)
        .and_then(|sub| sub.first().copied())
        .filter(|&v| v != 0)
        .unwrap_or(default)
}

/// Parse a `38/48/58` extended-color introducer's sub-parameters from a
/// single sub-param slice — i.e. the ITU-T T.416 colon form like
/// `CSI 38:5:42m` (which `vte 0.15` exposes as one slice `[38, 5, 42]`).
/// Caller passes the slice *after* the leading 38/48/58, i.e. `[5, 42]`
/// or `[2, R, G, B]` (or the 5-element `[2, Pi, R, G, B]` with the T.416
/// color-space identifier we ignore).
///
/// Returns `None` for malformed input (insufficient sub-params, unknown
/// kind). The caller has already consumed the sub-params either way, so
/// nothing leaks back into the SGR stream.
fn parse_extended_color_from_slice(rest: &[u16]) -> Option<Color> {
    match rest.first().copied()? {
        // Indexed: next sub-param is the 0..256 palette index.
        5 => rest.get(1).map(|n| Color::Indexed256(clamp_u8(*n))),
        // Truecolor. The canonical T.416 form is `[2, Pi, R, G, B]` with a
        // color-space identifier; the widely-deployed shortcut form omits
        // it (`[2, R, G, B]`). xterm and alacritty both accept both. We
        // mirror that: 5+ sub-params → skip the identifier; 4 sub-params →
        // treat the first as R.
        2 => {
            // `rest.len() >= N` below guarantees the indexing is in-bounds,
            // so we use direct slice access — `rest.get(..).copied()?`
            // would produce unreachable short-circuit branches that clippy
            // and coverage both flag.
            let (r, g, b) = if rest.len() >= 5 {
                (rest[2], rest[3], rest[4])
            } else if rest.len() >= 4 {
                (rest[1], rest[2], rest[3])
            } else {
                return None;
            };
            Some(Color::Rgb(clamp_u8(r), clamp_u8(g), clamp_u8(b)))
        }
        _ => None,
    }
}

/// Parse a `38/48/58` extended-color from the legacy semicolon form
/// (`CSI 38;5;42m` → slices `[[38],[5],[42]]`). The caller has already
/// consumed the leading `38/48/58` slice from `iter`. We read the kind
/// (5 or 2) and the appropriate number of *following* top-level params,
/// returning `None` on malformed input. Crucially, **we always consume
/// the expected number of params** so they cannot leak back into the
/// outer SGR walker.
fn parse_extended_color_from_iter<'a, I>(iter: &mut I) -> Option<Color>
where
    I: Iterator<Item = &'a [u16]>,
{
    let kind = iter.next().and_then(|s| s.first().copied())?;
    match kind {
        5 => {
            let idx = iter.next().and_then(|s| s.first().copied())?;
            Some(Color::Indexed256(clamp_u8(idx)))
        }
        2 => {
            // Semicolon form is always 3-component RGB. The T.416
            // color-space identifier is only meaningful in the colon
            // form and not transmitted via `;`-separated params.
            let r = iter.next().and_then(|s| s.first().copied())?;
            let g = iter.next().and_then(|s| s.first().copied())?;
            let b = iter.next().and_then(|s| s.first().copied())?;
            Some(Color::Rgb(clamp_u8(r), clamp_u8(g), clamp_u8(b)))
        }
        _ => None,
    }
}

/// Saturating cast — SGR params arrive as `u16`; valid values are 0..256.
fn clamp_u8(v: u16) -> u8 {
    u8::try_from(v).unwrap_or(u8::MAX)
}

fn ansi_color(idx: u16, bright: bool) -> Color {
    match (idx, bright) {
        (0, false) => Color::Black,
        (1, false) => Color::Red,
        (2, false) => Color::Green,
        (3, false) => Color::Yellow,
        (4, false) => Color::Blue,
        (5, false) => Color::Magenta,
        (6, false) => Color::Cyan,
        (7, false) => Color::White,
        (0, true) => Color::BrightBlack,
        (1, true) => Color::BrightRed,
        (2, true) => Color::BrightGreen,
        (3, true) => Color::BrightYellow,
        (4, true) => Color::BrightBlue,
        (5, true) => Color::BrightMagenta,
        (6, true) => Color::BrightCyan,
        (7, true) => Color::BrightWhite,
        _ => Color::Default,
    }
}

impl Perform for Term {
    fn print(&mut self, c: char) {
        self.print_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\r' => self.cursor.col = 0,
            b'\n' | 0x0B | 0x0C => self.linefeed(),
            0x08 => {
                // BS: move cursor left one, no wrap.
                if self.cursor.col > 0 {
                    self.cursor.col -= 1;
                }
            }
            b'\t' => {
                // HT: advance to next multiple of TAB_WIDTH, clamped.
                let next = (self.cursor.col / TAB_WIDTH + 1) * TAB_WIDTH;
                self.cursor.col = next.min(self.cols.saturating_sub(1));
            }
            // BEL and everything else are no-ops for M3.
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        self.handle_csi(params, intermediates, action);
    }

    // OSC / DCS / APC / hyperlinks / kitty keyboard / mode 2026 etc. all
    // deferred. TODOs live in lib-level docs.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::{Color, StyleFlags};
    use toastty_parser::Parser;

    /// Feed `bytes` through a fresh parser into `t`.
    fn feed(t: &mut Term, bytes: &[u8]) {
        let mut p = Parser::new();
        p.advance(t, bytes);
    }

    /// Build a `Term`, feed it bytes, return it.
    fn run(rows: u16, cols: u16, bytes: &[u8]) -> Term {
        let mut t = Term::new(rows, cols, 0);
        feed(&mut t, bytes);
        t
    }

    /// Stringify a row, trimming trailing blanks.
    fn row_text(t: &Term, r: u16) -> String {
        let mut s: String = t.row(r).cells.iter().map(|c| c.ch).collect();
        while s.ends_with(' ') {
            s.pop();
        }
        s
    }

    #[test]
    fn new_initialises_blank_grid_and_cursor() {
        let t = Term::new(3, 4, 8);
        assert_eq!(t.size(), (3, 4));
        assert_eq!(t.cursor(), Cursor::default());
        assert!(!t.is_alt_active());
        for r in 0..3 {
            assert_eq!(row_text(&t, r), "");
        }
    }

    #[test]
    fn new_clamps_zero_dimensions_to_one() {
        // The renderer can't display zero rows or columns; the constructor
        // should round up rather than build an unusable grid.
        let t = Term::new(0, 0, 0);
        assert_eq!(t.size(), (1, 1));
    }

    #[test]
    fn plain_text_lands_in_row_zero() {
        let t = run(3, 8, b"hello");
        assert_eq!(row_text(&t, 0), "hello");
        assert_eq!(t.cursor().col, 5);
        assert_eq!(t.cursor().row, 0);
    }

    #[test]
    fn cr_returns_to_col_zero() {
        let t = run(3, 8, b"abc\rx");
        assert_eq!(row_text(&t, 0), "xbc");
    }

    #[test]
    fn lf_moves_to_next_row() {
        let t = run(3, 8, b"a\nb");
        assert_eq!(row_text(&t, 0), "a");
        // LF does not move to col 0 — that's CR's job. Cursor stayed at col 1.
        assert_eq!(t.cursor().col, 2);
        assert_eq!(t.cursor().row, 1);
        assert_eq!(t.row(1).cells[1].ch, 'b');
    }

    #[test]
    fn crlf_starts_fresh_line() {
        let t = run(3, 8, b"a\r\nb");
        assert_eq!(row_text(&t, 0), "a");
        assert_eq!(row_text(&t, 1), "b");
    }

    #[test]
    fn lf_at_bottom_scrolls() {
        let mut t = Term::new(2, 4, 4);
        feed(&mut t, b"a\r\nb\r\nc");
        // After LF on the bottom row, "a" should have scrolled into
        // history; visible is now "b" then "c".
        assert_eq!(row_text(&t, 0), "b");
        assert_eq!(row_text(&t, 1), "c");
    }

    #[test]
    fn vertical_tab_and_form_feed_act_like_lf() {
        // Real terminals treat 0x0B and 0x0C as LF for index motion.
        let t = run(3, 4, b"a\x0bb\x0cc");
        assert_eq!(row_text(&t, 0), "a");
        assert_eq!(row_text(&t, 1).trim_end(), " b");
        assert_eq!(t.row(2).cells[2].ch, 'c');
    }

    #[test]
    fn backspace_moves_cursor_left_but_does_not_wrap() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"ab\x08");
        assert_eq!(t.cursor().col, 1);
        // Backspace at column 0 is a no-op (must not underflow).
        feed(&mut t, b"\x08\x08\x08\x08");
        assert_eq!(t.cursor().col, 0);
    }

    #[test]
    fn tab_advances_to_next_tab_stop() {
        // cols=24 so we have multiple stops without clamping.
        let mut t = Term::new(2, 24, 0);
        feed(&mut t, b"\t");
        assert_eq!(t.cursor().col, 8);
        feed(&mut t, b"a\t");
        assert_eq!(t.cursor().col, 16);
        feed(&mut t, b"\t");
        // Already at multiple of 8 — tab advances to the next one.
        assert_eq!(t.cursor().col, 24 - 1); // clamps to last col since cols=24
    }

    #[test]
    fn tab_clamps_at_last_column() {
        let mut t = Term::new(2, 10, 0);
        // Print enough to push past the next tab stop; tab should clamp.
        feed(&mut t, b"abcdef\t");
        assert_eq!(t.cursor().col, 8);
        feed(&mut t, b"\t");
        // Next tab stop would be 16, but cols=10, so clamp to last column = 9.
        assert_eq!(t.cursor().col, 9);
    }

    #[test]
    fn bel_is_a_noop() {
        let t = run(2, 4, b"a\x07b");
        assert_eq!(row_text(&t, 0), "ab");
    }

    #[test]
    fn print_wraps_at_end_of_line_and_marks_soft_wrap() {
        let t = run(3, 4, b"hello");
        assert_eq!(row_text(&t, 0), "hell");
        assert_eq!(row_text(&t, 1), "o");
        assert!(t.row(0).soft_wrap);
        assert!(!t.row(1).soft_wrap);
    }

    #[test]
    fn cursor_moves_table_driven() {
        // (initial_seq, op_seq, expected_row, expected_col)
        let cases: &[(&[u8], &[u8], u16, u16)] = &[
            // CUU — up
            (b"\r\n\r\n\r\n", b"\x1b[2A", 1, 0),
            // CUD — down
            (b"", b"\x1b[2B", 2, 0),
            // CUF — forward
            (b"", b"\x1b[3C", 0, 3),
            // CUB — back (after some text)
            (b"abcd", b"\x1b[2D", 0, 2),
            // CUP — absolute position 2;3 (1-based)
            (b"", b"\x1b[2;3H", 1, 2),
            // CUP with implicit defaults goes home
            (b"abcd\n\rxy", b"\x1b[H", 0, 0),
            // 'f' is an alias for CUP
            (b"", b"\x1b[3;2f", 2, 1),
            // Movement with zero param treated as 1
            (b"", b"\x1b[0C", 0, 1),
            // Movement clamps to grid edges
            (b"", b"\x1b[99C", 0, 7),
            (b"", b"\x1b[99B", 4, 0),
        ];
        for (init, op, want_r, want_c) in cases.iter().copied() {
            let mut t = Term::new(5, 8, 0);
            feed(&mut t, init);
            feed(&mut t, op);
            let cur = t.cursor();
            assert_eq!(
                (cur.row, cur.col),
                (want_r, want_c),
                "init={init:?} op={op:?}",
            );
        }
    }

    #[test]
    fn erase_display_modes() {
        // mode 0 — cursor to end
        let mut t = Term::new(3, 5, 0);
        feed(&mut t, b"aaaaa\r\nbbbbb\r\nccccc\x1b[1;3H\x1b[0J");
        assert_eq!(row_text(&t, 0), "aa");
        assert_eq!(row_text(&t, 1), "");
        assert_eq!(row_text(&t, 2), "");

        // mode 1 — start to cursor
        let mut t = Term::new(3, 5, 0);
        feed(&mut t, b"aaaaa\r\nbbbbb\r\nccccc\x1b[2;3H\x1b[1J");
        assert_eq!(row_text(&t, 0), "");
        assert_eq!(row_text(&t, 1).trim_end(), "   bb");
        assert_eq!(row_text(&t, 2), "ccccc");

        // mode 2 — everything
        let mut t = Term::new(3, 5, 0);
        feed(&mut t, b"aaaaa\r\nbbbbb\r\nccccc\x1b[2J");
        assert_eq!(row_text(&t, 0), "");
        assert_eq!(row_text(&t, 1), "");
        assert_eq!(row_text(&t, 2), "");

        // mode 3 — also clears scrollback; in M3 same as mode 2.
        let mut t = Term::new(2, 4, 4);
        feed(&mut t, b"abcd\r\nefgh\x1b[3J");
        assert_eq!(row_text(&t, 0), "");
        assert_eq!(row_text(&t, 1), "");
    }

    #[test]
    fn erase_line_modes() {
        // Use CUP (`H`) to reposition to row 1, col 3 — `abcdef` then jump
        // back to col 3 (1-based) = idx 2. EL covers the rest.
        let mut t = Term::new(1, 6, 0);
        feed(&mut t, b"abcdef\x1b[1;3H\x1b[0K");
        assert_eq!(row_text(&t, 0), "ab");
        let mut t = Term::new(1, 6, 0);
        feed(&mut t, b"abcdef\x1b[1;3H\x1b[1K");
        assert_eq!(row_text(&t, 0).trim_end(), "   def");
        let mut t = Term::new(1, 6, 0);
        feed(&mut t, b"abcdef\x1b[1;3H\x1b[2K");
        assert_eq!(row_text(&t, 0), "");
    }

    #[test]
    fn sgr_single_param_sets_fg_color() {
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[31mr");
        assert_eq!(t.row(0).cells[0].style.fg, Color::Red);
    }

    #[test]
    fn sgr_multi_param_applies_in_order() {
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[1;3;31;44mx");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.fg, Color::Red);
        assert_eq!(s.bg, Color::Blue);
        assert!(s.flags.bold);
        assert!(s.flags.italic);
    }

    #[test]
    fn sgr_reset_clears_everything() {
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[1;31mA\x1b[0mB");
        assert!(t.row(0).cells[0].style.flags.bold);
        assert_eq!(t.row(0).cells[1].style, Style::RESET);
    }

    #[test]
    fn sgr_empty_means_reset() {
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[1;31mA\x1b[mB");
        assert_eq!(t.row(0).cells[1].style, Style::RESET);
    }

    #[test]
    fn sgr_attribute_reset_codes() {
        let mut t = Term::new(1, 8, 0);
        feed(&mut t, b"\x1b[1;3;4;7mA\x1b[22;23;24;27mB");
        let a = t.row(0).cells[0].style.flags;
        assert_eq!(
            a,
            StyleFlags {
                bold: true,
                italic: true,
                underline: true,
                reverse: true
            }
        );
        let b = t.row(0).cells[1].style.flags;
        assert_eq!(b, StyleFlags::default());
    }

    #[test]
    fn sgr_bright_and_default_colors() {
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[91;104mA");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.fg, Color::BrightRed);
        assert_eq!(s.bg, Color::BrightBlue);
        feed(&mut t, b"\x1b[39;49mB");
        let s = t.row(0).cells[1].style;
        assert_eq!(s.fg, Color::Default);
        assert_eq!(s.bg, Color::Default);
    }

    #[test]
    fn sgr_full_color_table() {
        // Every standard + bright slot maps to the expected variant.
        let pairs: &[(u16, Color)] = &[
            (30, Color::Black),
            (31, Color::Red),
            (32, Color::Green),
            (33, Color::Yellow),
            (34, Color::Blue),
            (35, Color::Magenta),
            (36, Color::Cyan),
            (37, Color::White),
            (90, Color::BrightBlack),
            (91, Color::BrightRed),
            (92, Color::BrightGreen),
            (93, Color::BrightYellow),
            (94, Color::BrightBlue),
            (95, Color::BrightMagenta),
            (96, Color::BrightCyan),
            (97, Color::BrightWhite),
        ];
        for (code, want) in pairs.iter().copied() {
            let mut t = Term::new(1, 1, 0);
            feed(&mut t, format!("\x1b[{code}mX").as_bytes());
            assert_eq!(t.row(0).cells[0].style.fg, want, "fg code {code}");
            let bg_code = code + 10;
            let mut t = Term::new(1, 1, 0);
            feed(&mut t, format!("\x1b[{bg_code}mX").as_bytes());
            assert_eq!(t.row(0).cells[0].style.bg, want, "bg code {bg_code}");
        }
    }

    #[test]
    fn sgr_unknown_param_is_ignored() {
        // 256-color introducer is unhandled in M3 — it must not panic, and
        // it must not silently apply garbage to the style.
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[123mX");
        assert_eq!(t.row(0).cells[0].style, Style::RESET);
    }

    #[test]
    fn unknown_csi_action_ignored() {
        // 'Z' (CBT) is unhandled — it must not panic and the cursor must
        // not move.
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b[2Z");
        assert_eq!(t.cursor(), Cursor::default());
    }

    #[test]
    fn alt_screen_enter_and_exit_round_trip() {
        let mut t = Term::new(3, 4, 4);
        feed(&mut t, b"abcd\r\nefgh");
        let saved_before = t.cursor();
        feed(&mut t, b"\x1b[?1049h"); // enter alt
        assert!(t.is_alt_active());
        assert_eq!(t.cursor(), Cursor::default());
        // Alt grid is blank.
        for r in 0..3 {
            assert_eq!(row_text(&t, r), "");
        }
        feed(&mut t, b"XYZ");
        // Now exit — primary state should be intact, alt content gone.
        feed(&mut t, b"\x1b[?1049l");
        assert!(!t.is_alt_active());
        assert_eq!(t.cursor(), saved_before);
        assert_eq!(row_text(&t, 0), "abcd");
        assert_eq!(row_text(&t, 1).trim_end(), "efgh");
    }

    #[test]
    fn alt_screen_double_enter_is_idempotent() {
        let mut t = Term::new(2, 4, 4);
        feed(&mut t, b"hi");
        let saved = t.cursor();
        feed(&mut t, b"\x1b[?1049h\x1b[?1049h");
        feed(&mut t, b"\x1b[?1049l\x1b[?1049l");
        assert_eq!(t.cursor(), saved);
        assert_eq!(row_text(&t, 0), "hi");
    }

    #[test]
    fn unknown_decset_param_is_a_noop() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b[?25hX");
        assert_eq!(t.row(0).cells[0].ch, 'X');
    }

    #[test]
    fn resize_clamps_cursor_and_changes_size() {
        let mut t = Term::new(3, 4, 4);
        feed(&mut t, b"abcd\r\nefgh\r\nijkl");
        // After "ijkl" the cursor sits at col=4 (pending wrap), row=2.
        assert_eq!(t.cursor().row, 2);
        t.resize(2, 3);
        assert_eq!(t.size(), (2, 3));
        let c = t.cursor();
        // Row must be inside the new viewport; col may equal cols
        // (pending-wrap sentinel) but must not exceed it.
        assert!(c.row < 2);
        assert!(c.col <= 3);
    }

    #[test]
    fn resize_to_larger_keeps_visible_content() {
        let mut t = Term::new(2, 4, 4);
        feed(&mut t, b"ab\r\ncd");
        t.resize(4, 6);
        assert_eq!(t.size(), (4, 6));
        assert_eq!(row_text(&t, 0), "ab");
        assert_eq!(row_text(&t, 1).trim_end(), "cd");
    }

    #[test]
    fn resize_zero_dims_clamped_to_one() {
        let mut t = Term::new(2, 2, 0);
        t.resize(0, 0);
        assert_eq!(t.size(), (1, 1));
    }

    #[test]
    fn print_after_scroll_keeps_writing_on_last_row() {
        let mut t = Term::new(2, 4, 4);
        feed(&mut t, b"aaaa\r\nbbbb\r\ncccc");
        // After the second LF the cursor is on the last row; "cccc" lands
        // entirely on row 1; row 0 should now be "bbbb".
        assert_eq!(row_text(&t, 0), "bbbb");
        assert_eq!(row_text(&t, 1), "cccc");
    }

    #[test]
    fn print_advances_cursor_one_past_last_column_until_next_print() {
        let mut t = Term::new(2, 3, 0);
        feed(&mut t, b"abc");
        // After writing the final column, cursor sits one past the end
        // (pending-wrap behaviour). It must not immediately wrap.
        assert_eq!(t.cursor().row, 0);
        assert_eq!(t.cursor().col, 3);
        feed(&mut t, b"d");
        assert!(t.row(0).soft_wrap);
        assert_eq!(t.row(1).cells[0].ch, 'd');
    }

    #[test]
    fn cursor_back_underflow_protected() {
        let mut t = Term::new(2, 4, 0);
        // CUB at col 0 must clamp, not panic.
        feed(&mut t, b"\x1b[10D");
        assert_eq!(t.cursor().col, 0);
        feed(&mut t, b"\x1b[10A");
        assert_eq!(t.cursor().row, 0);
    }

    #[test]
    fn apply_sgr_with_empty_params_resets_style() {
        // vte never produces empty params from a real CSI, but the
        // defensive branch in `apply_sgr` should still be exercised.
        let mut t = Term::new(1, 4, 0);
        t.cursor.style = Style {
            fg: Color::Red,
            bg: Color::Default,
            flags: StyleFlags {
                bold: true,
                ..StyleFlags::default()
            },
        };
        t.apply_sgr(&Params::default());
        assert_eq!(t.cursor.style, Style::RESET);
    }

    #[test]
    fn ansi_color_unknown_index_falls_back_to_default() {
        // The internal `ansi_color` helper has a defensive catch-all; we
        // exercise it here since callers normally bound the index 0..=7.
        assert_eq!(super::ansi_color(99, false), Color::Default);
        assert_eq!(super::ansi_color(99, true), Color::Default);
    }

    #[test]
    fn nth_param_uses_default_for_missing_or_zero() {
        // `CSI ;5 H` — row default (=1), col=5.
        let mut t = Term::new(5, 5, 0);
        feed(&mut t, b"\x1b[;5H");
        assert_eq!(t.cursor().row, 0);
        assert_eq!(t.cursor().col, 4);
    }

    // ----- Extended-color SGR (38/48 + 5/2) leak-fix tests ---------------
    //
    // These cover the bug where the old SGR walker re-interpreted the
    // sub-params of a truecolor / 256-color introducer as standalone SGR
    // codes — e.g. `CSI 38;2;200;32;100m` accidentally setting fg green
    // because 32 lands in the 30..=37 named-foreground range.

    #[test]
    fn sgr_256_fg_does_not_leak_into_bg_for_index_in_named_range() {
        // `42` is a 256-color index but ALSO the value of `bg=Green` in the
        // legacy SGR table. The fix must not re-apply `42` as a standalone
        // SGR after consuming it as the palette index.
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[38;5;42mX");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.fg, Color::Indexed256(42));
        // Critical: bg must still be Default. Under the broken parser bg
        // would be `Color::Green` because `42 - 40 = 2` triggers the 40..=47
        // arm.
        assert_eq!(s.bg, Color::Default);
        assert_eq!(s.flags, StyleFlags::default());
    }

    #[test]
    fn sgr_truecolor_fg_does_not_leak_inner_byte_as_fg() {
        // `\x1b[38;2;200;32;100m` sets fg to RGB(200, 32, 100). The middle
        // byte (G=32) lies in 30..=37 and would set fg=Green under the
        // broken parser; the third byte (B=100) lies in 100..=107 and
        // would also set bg=BrightBlack. Neither must happen.
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[38;2;200;32;100mX");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.fg, Color::Rgb(200, 32, 100));
        assert_eq!(s.bg, Color::Default);
        // Sanity: even if a future bug made these flags persist, this
        // asserts no flag side effect.
        assert_eq!(s.flags, StyleFlags::default());
    }

    #[test]
    fn sgr_256_bg_basic() {
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[48;5;1mX");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.bg, Color::Indexed256(1));
        assert_eq!(s.fg, Color::Default);
    }

    #[test]
    fn sgr_truecolor_bg_basic() {
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[48;2;10;20;30mX");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.bg, Color::Rgb(10, 20, 30));
        assert_eq!(s.fg, Color::Default);
    }

    #[test]
    fn sgr_256_incomplete_is_ignored_cleanly() {
        // `\x1b[38;5m` is missing the index — the parser must consume the
        // `5` so it can't leak as a standalone SGR, and must NOT change
        // the fg.
        let mut t = Term::new(1, 4, 0);
        // Start with a known fg so we can detect an accidental change.
        feed(&mut t, b"\x1b[31m\x1b[38;5mX");
        let s = t.row(0).cells[0].style;
        // fg is whatever the introducer left it as — either kept Red (good)
        // or reset to Default. Under the broken parser, `5` would land in
        // `apply_sgr_param(5)` (a no-op currently) but a future bug could
        // map 5 to BlinkSlow. We assert the introducer was at least
        // consumed: fg should be Red (untouched), NOT something else.
        assert_eq!(s.fg, Color::Red);
        // And no flag side effects from consuming `5`.
        assert!(!s.flags.bold);
    }

    #[test]
    fn sgr_truecolor_incomplete_does_not_leak_components() {
        // Missing one component: `\x1b[38;2;200;32m` (only R and G). The
        // 32 must NOT leak as a standalone SGR setting fg=Green.
        let mut t = Term::new(1, 4, 0);
        // Establish a baseline: bold + red.
        feed(&mut t, b"\x1b[1;31m");
        feed(&mut t, b"\x1b[38;2;200;32mX");
        let s = t.row(0).cells[0].style;
        // Fg was Red; the truecolor sequence is incomplete — fg either
        // stays Red or becomes whatever the partial parse returned. In
        // our implementation it stays Red (Option::unwrap_or current fg).
        // The CRITICAL assertion: fg is NOT Green (which would be set by
        // `32` leaking through).
        assert_ne!(s.fg, Color::Green);
        // And bold must still be on (we didn't accidentally consume the 1).
        assert!(s.flags.bold);
    }

    #[test]
    fn sgr_mixed_named_then_extended_then_attr() {
        // Real-world style sequence: red fg, override with 256-color fg,
        // turn on bold. Final state: fg = Indexed256(42), bold = true,
        // bg = Default.
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[31;38;5;42;1mX");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.fg, Color::Indexed256(42));
        assert_eq!(s.bg, Color::Default);
        assert!(s.flags.bold);
    }

    #[test]
    fn sgr_colon_form_256_color() {
        // ITU-T T.416 colon form. `vte 0.15` reports this as one slice
        // `[38, 5, 42]` rather than three slices `[[38],[5],[42]]`. The
        // parser must handle both.
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[38:5:42mX");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.fg, Color::Indexed256(42));
        assert_eq!(s.bg, Color::Default);
    }

    #[test]
    fn sgr_colon_form_truecolor_short() {
        // Colon-form 4-arg truecolor: `[38, 2, R, G, B]`. The G byte (32)
        // must not leak.
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[38:2:200:32:100mX");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.fg, Color::Rgb(200, 32, 100));
        assert_eq!(s.bg, Color::Default);
    }

    #[test]
    fn sgr_colon_form_truecolor_with_color_space_id() {
        // Canonical T.416 truecolor: `[38, 2, Pi, R, G, B]`. The Pi
        // color-space identifier (here `1` — sRGB) must be skipped.
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[38:2:1:200:32:100mX");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.fg, Color::Rgb(200, 32, 100));
        assert_eq!(s.bg, Color::Default);
    }

    #[test]
    fn sgr_unknown_extended_kind_is_consumed_without_leak() {
        // 38 followed by neither 5 nor 2: malformed. The introducer must
        // still consume the `9` so it doesn't reapply as a standalone
        // SGR. Currently 9 is unhandled but we test the principle.
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[1;38;9;31mX");
        let s = t.row(0).cells[0].style;
        // Bold from before the introducer should stick.
        assert!(s.flags.bold);
        // The trailing `31` (red fg) must apply — the parser must have
        // bailed cleanly out of the malformed 38;9 sequence.
        assert_eq!(s.fg, Color::Red);
    }

    #[test]
    fn sgr_default_fg_only_clears_fg() {
        // `\x1b[39m` resets fg but leaves bg/flags alone.
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[1;31;44mA\x1b[39mB");
        let s = t.row(0).cells[1].style;
        assert_eq!(s.fg, Color::Default);
        assert_eq!(s.bg, Color::Blue);
        assert!(s.flags.bold);
    }

    #[test]
    fn sgr_underline_color_is_consumed_and_ignored() {
        // We don't store underline color yet, but the introducer must
        // consume its sub-params so nothing leaks. Mode 58 with 256-color
        // index 42 should NOT end up setting bg=Green or anything else.
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[58;5;42mX");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.fg, Color::Default);
        assert_eq!(s.bg, Color::Default);
        assert_eq!(s.flags, StyleFlags::default());
    }

    #[test]
    fn sgr_underline_color_truecolor_is_consumed_and_ignored() {
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[58;2;200;32;100mX");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.fg, Color::Default);
        assert_eq!(s.bg, Color::Default);
        assert_eq!(s.flags, StyleFlags::default());
    }

    #[test]
    fn sgr_underline_color_default_is_a_noop() {
        // Mode 59 = default underline color. Must not panic / leak.
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[31;59mX");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.fg, Color::Red);
    }

    #[test]
    fn sgr_helix_simulated_sidebar_does_not_leak_past_reset() {
        // Reproduces the helix sidebar leak shape: paint a few cells with
        // a 256-color fg, then reset, then paint plain text. The plain
        // text cells must come out with default fg — no green leak.
        let mut t = Term::new(1, 16, 0);
        feed(&mut t, b"\x1b[38;5;42m 1 \x1b[0m text");
        // Cells 0..3 (the painted " 1 ") have fg=Indexed256(42); cells
        // 3..8 (the " text" after reset) have fg=Default.
        for (i, want_fg) in [
            (0, Color::Indexed256(42)),
            (1, Color::Indexed256(42)),
            (2, Color::Indexed256(42)),
            (3, Color::Default),
            (4, Color::Default),
            (5, Color::Default),
            (6, Color::Default),
            (7, Color::Default),
        ] {
            assert_eq!(
                t.row(0).cells[i].style.fg,
                want_fg,
                "cell {i} fg differs (helix leak regression)",
            );
            // bg must always be Default in this scenario.
            assert_eq!(
                t.row(0).cells[i].style.bg,
                Color::Default,
                "cell {i} bg accidentally non-default",
            );
        }
    }

    #[test]
    fn sgr_truecolor_then_named_fg_overrides_correctly() {
        // After truecolor, a plain named SGR must still work (no stale
        // state in the iterator).
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[38;2;1;2;3m\x1b[33mX");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.fg, Color::Yellow);
    }

    #[test]
    fn parse_extended_color_from_slice_rejects_unknown_kind() {
        // Direct unit-test of the slice helper — input `[9, ...]` is
        // neither 5 nor 2 so must return None.
        assert!(super::parse_extended_color_from_slice(&[9, 1, 2, 3]).is_none());
        // Empty slice → None.
        assert!(super::parse_extended_color_from_slice(&[]).is_none());
        // 5 with no index → None.
        assert!(super::parse_extended_color_from_slice(&[5]).is_none());
        // 2 with fewer than 3 RGB components → None.
        assert!(super::parse_extended_color_from_slice(&[2, 1, 2]).is_none());
    }

    #[test]
    fn parse_extended_color_from_iter_returns_none_for_unknown_kind() {
        // Direct unit-test of the iterator helper.
        let slices: Vec<&[u16]> = vec![&[9], &[1], &[2]];
        let mut it = slices.into_iter();
        assert!(super::parse_extended_color_from_iter(&mut it).is_none());
    }

    #[test]
    fn clamp_u8_saturates_values_above_255() {
        assert_eq!(super::clamp_u8(0), 0);
        assert_eq!(super::clamp_u8(255), 255);
        assert_eq!(super::clamp_u8(256), 255);
        assert_eq!(super::clamp_u8(u16::MAX), 255);
    }

    #[test]
    fn sgr_colon_form_bg_256_color() {
        // Cover the colon-form `48` branch (top-level `bg = ...`).
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[48:5:1mX");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.bg, Color::Indexed256(1));
        assert_eq!(s.fg, Color::Default);
    }

    #[test]
    fn sgr_colon_form_underline_color_consumed_and_ignored() {
        // Cover the colon-form `58` branch (top-level discard).
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[58:5:42mX");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.fg, Color::Default);
        assert_eq!(s.bg, Color::Default);
    }

    #[test]
    fn sgr_semicolon_form_bg_incomplete_leaves_bg_alone() {
        // Cover the `None` branch of the bg `if let Some(c)` in the
        // semicolon-form 48 handler. We pre-set a known bg (Blue), then
        // feed a malformed `\x1b[48;9m` (unknown kind 9). The bg must
        // stay Blue — the partial parse must NOT leak any of those
        // codes back into the SGR walker.
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[44m\x1b[48;9mX");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.bg, Color::Blue);
    }

    #[test]
    fn parse_extended_color_from_slice_truecolor_with_color_space_id_form() {
        // Cover the rest.len() >= 5 branch directly.
        let c = super::parse_extended_color_from_slice(&[2, 1, 100, 150, 200]);
        assert_eq!(c, Some(Color::Rgb(100, 150, 200)));
    }

    #[test]
    fn parse_extended_color_from_slice_truecolor_short_form() {
        // Cover the 4-arg shortcut.
        let c = super::parse_extended_color_from_slice(&[2, 100, 150, 200]);
        assert_eq!(c, Some(Color::Rgb(100, 150, 200)));
    }

    #[test]
    fn parse_extended_color_from_iter_truecolor_runs_out_of_components() {
        // Cover the `?` branches inside the iter helper. Each missing
        // component path returns None.
        // Missing G.
        let slices: Vec<&[u16]> = vec![&[2], &[100]];
        let mut it = slices.into_iter();
        assert!(super::parse_extended_color_from_iter(&mut it).is_none());
        // Missing B.
        let slices: Vec<&[u16]> = vec![&[2], &[100], &[150]];
        let mut it = slices.into_iter();
        assert!(super::parse_extended_color_from_iter(&mut it).is_none());
        // Missing R.
        let slices: Vec<&[u16]> = vec![&[2]];
        let mut it = slices.into_iter();
        assert!(super::parse_extended_color_from_iter(&mut it).is_none());
        // Missing 256 index.
        let slices: Vec<&[u16]> = vec![&[5]];
        let mut it = slices.into_iter();
        assert!(super::parse_extended_color_from_iter(&mut it).is_none());
        // Empty iterator -> None on first read.
        let slices: Vec<&[u16]> = vec![];
        let mut it = slices.into_iter();
        assert!(super::parse_extended_color_from_iter(&mut it).is_none());
    }
}
