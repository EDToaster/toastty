//! VT compatibility pass: the CSI / ESC sequences and DEC private modes
//! that used to fall through the dispatcher (see
//! `docs/compat-checklist.md`).
//!
//! - REP (`CSI Ps b`), CNL / CPL (`CSI E` / `CSI F`), HPA / HPR / VPR
//!   (`` CSI ` `` / `CSI a` / `CSI e`)
//! - Tab stops: HTS (`ESC H`), TBC (`CSI g`), CHT (`CSI I`), CBT (`CSI Z`)
//! - SCOSC / SCORC (`CSI s` / `CSI u`), IRM (`CSI 4 h/l`), ANSI DECRQM
//!   (`CSI Ps $ p`), DECALN (`ESC # 8`), DA3 (`CSI = c`), DECSTR
//!   (`CSI ! p`)
//! - DECSLRM (`CSI Pl ; Pr s`) gated on DECLRMM (`?69`)
//! - DEC private modes 1 (DECCKM), 5 (DECSCNM), 6 (DECOM), 7 (DECAWM),
//!   12 (att610 blink), 66 (DECNKM, same flag as DECKPAM/DECKPNM),
//!   1007 (alternate scroll), 1036 / 1039 (meta/alt sends escape).
//!
//! Behaviour follows xterm (`ctlseqs`, `charproc.c`) unless noted.

use super::Term;
use crate::cell::{Cell, Style};
use crate::cursor::Cursor;

/// Tab stops sit every eight columns by default.
const TAB_WIDTH: usize = 8;

/// Mode / editing state introduced by the VT compatibility pass.
/// Independent terminal modes, hence the many bools.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub(super) struct VtModes {
    /// Last graphic character `print` wrote — the character REP repeats.
    /// `None` after RIS, or when nothing has been printed yet.
    pub(super) last_graphic: Option<char>,
    /// One flag per column; `true` = a tab stop. Reset to every eighth
    /// column on RIS, DECSTR and resize.
    pub(super) tab_stops: Vec<bool>,
    /// IRM (`CSI 4 h`): printing shifts the rest of the line right.
    pub(super) insert_mode: bool,
    /// DECCKM (`?1`): cursor keys send `SS3 A` instead of `CSI A`.
    pub(super) app_cursor_keys: bool,
    /// DECKPAM (`ESC =`) / DECNKM (`?66`): keypad sends application codes.
    pub(super) app_keypad: bool,
    /// DECAWM (`?7`): wrap at the right margin. Default on.
    pub(super) autowrap: bool,
    /// DECOM (`?6`): CUP / VPA / HPA are relative to the margins and the
    /// cursor is confined to them.
    pub(super) origin_mode: bool,
    /// DECSCNM (`?5`): reverse-video screen (renderer swaps fg/bg).
    pub(super) reverse_video: bool,
    /// `?1007`: on the alternate screen with mouse reporting off, the
    /// wheel sends cursor-key presses. Default on (toastty always did
    /// this before the mode was wired).
    pub(super) alternate_scroll: bool,
    /// `?1036` metaSendsEscape. Default on.
    pub(super) meta_sends_escape: bool,
    /// `?1039` altSendsEscape. Default on.
    pub(super) alt_sends_escape: bool,
    /// DECLRMM (`?69`): `CSI Pl ; Pr s` sets left/right margins.
    pub(super) lr_margin_mode: bool,
    /// DECSLRM left/right margins, 0-based inclusive. Only consulted
    /// while `lr_margin_mode` is set.
    pub(super) left: u16,
    pub(super) right: u16,
    /// Deferred wrap when a print lands on a right margin that isn't
    /// the screen edge (the screen edge uses the `col == cols`
    /// sentinel instead). Also set with DECAWM off so the grapheme
    /// path knows the cursor sits *on* the last written cell. Stores the
    /// cursor position the flag belongs to; any other position means
    /// the cursor has moved and the flag is stale.
    pub(super) wrap_pending_at: Option<(u16, u16)>,
    /// DECOM / DECAWM captured by DECSC (xterm saves both).
    pub(super) decsc_origin: bool,
    pub(super) decsc_autowrap: bool,
}

impl VtModes {
    pub(super) fn new(cols: u16) -> Self {
        Self {
            last_graphic: None,
            tab_stops: default_tab_stops(cols),
            insert_mode: false,
            app_cursor_keys: false,
            app_keypad: false,
            autowrap: true,
            origin_mode: false,
            reverse_video: false,
            alternate_scroll: true,
            meta_sends_escape: true,
            alt_sends_escape: true,
            lr_margin_mode: false,
            left: 0,
            right: cols.saturating_sub(1),
            wrap_pending_at: None,
            decsc_origin: false,
            decsc_autowrap: true,
        }
    }
}

fn default_tab_stops(cols: u16) -> Vec<bool> {
    (0..usize::from(cols))
        .map(|c| c > 0 && c % TAB_WIDTH == 0)
        .collect()
}

impl Term {
    // ----- public mode getters (read by the binary) -----

    /// DECCKM (`CSI ? 1 h`): cursor keys should be sent as `SS3 A..D`
    /// (and Home/End as `SS3 H` / `SS3 F`).
    #[must_use]
    pub fn app_cursor_keys(&self) -> bool {
        self.vt.app_cursor_keys
    }

    /// DECKPAM (`ESC =`) / DECNKM (`CSI ? 66 h`): the numeric keypad
    /// should send application (`SS3 p`..`SS3 y`, `SS3 M`, ...) codes.
    #[must_use]
    pub fn app_keypad(&self) -> bool {
        self.vt.app_keypad
    }

    /// DECSCNM (`CSI ? 5 h`): render the whole screen in reverse video.
    #[must_use]
    pub fn reverse_video(&self) -> bool {
        self.vt.reverse_video
    }

    /// `CSI ? 1007 h`: translate wheel motion into cursor keys on the
    /// alternate screen when mouse reporting is off.
    #[must_use]
    pub fn alternate_scroll(&self) -> bool {
        self.vt.alternate_scroll
    }

    /// Whether an Alt/Meta-modified key should be ESC-prefixed
    /// (`?1036` metaSendsEscape / `?1039` altSendsEscape; on unless the
    /// app resets both). When off the key is sent unprefixed.
    #[must_use]
    pub fn alt_sends_escape(&self) -> bool {
        self.vt.meta_sends_escape || self.vt.alt_sends_escape
    }

    /// IRM (`CSI 4 h`).
    #[must_use]
    pub fn insert_mode(&self) -> bool {
        self.vt.insert_mode
    }

    /// DECAWM (`CSI ? 7 h`).
    #[must_use]
    pub fn autowrap(&self) -> bool {
        self.vt.autowrap
    }

    /// DECOM (`CSI ? 6 h`).
    #[must_use]
    pub fn origin_mode(&self) -> bool {
        self.vt.origin_mode
    }

    /// Active DECSLRM margins (0-based, inclusive), or `None` when
    /// DECLRMM is off.
    #[must_use]
    pub fn lr_margins(&self) -> Option<(u16, u16)> {
        self.vt
            .lr_margin_mode
            .then_some((self.vt.left, self.vt.right))
    }

    /// Columns with a tab stop set (0-based). Mostly for tests.
    #[must_use]
    pub fn tab_stops(&self) -> Vec<u16> {
        self.vt
            .tab_stops
            .iter()
            .enumerate()
            .filter(|&(_, &on)| on)
            .map(|(i, _)| u16::try_from(i).unwrap_or(u16::MAX))
            .collect()
    }

    // ----- margins -----

    /// Left/right margins in effect for scrolling and editing:
    /// `(0, cols - 1)` unless DECLRMM is on.
    pub(super) fn lr_bounds(&self) -> (u16, u16) {
        if self.vt.lr_margin_mode {
            (self.vt.left, self.vt.right.min(self.cols.saturating_sub(1)))
        } else {
            (0, self.cols.saturating_sub(1))
        }
    }

    /// DECSLRM narrows the screen (something other than full width).
    pub(super) fn lr_narrowed(&self) -> bool {
        let (l, r) = self.lr_bounds();
        l > 0 || r + 1 < self.cols
    }

    /// Cursor column lies inside the left/right margins.
    pub(super) fn cursor_in_lr(&self) -> bool {
        let (l, r) = self.lr_bounds();
        let col = self.cursor.col.min(self.cols.saturating_sub(1));
        col >= l && col <= r
    }

    /// Exclusive right edge a print at the cursor wraps at: the right
    /// margin when the cursor is inside it, else the screen edge.
    pub(super) fn print_right_edge(&self) -> u16 {
        if self.vt.lr_margin_mode && self.cursor.col <= self.vt.right {
            self.vt.right.saturating_add(1).min(self.cols)
        } else {
            self.cols
        }
    }

    /// Column CR / NEL / autowrap return to: the left margin when the
    /// cursor is at or right of it, else column 0.
    pub(super) fn line_start_col(&self) -> u16 {
        if self.vt.lr_margin_mode && self.cursor.col >= self.vt.left {
            self.vt.left
        } else {
            0
        }
    }

    /// DECSLRM — `CSI Pl ; Pr s` (only while DECLRMM is set). Invalid
    /// pairs (`Pl >= Pr`) are ignored, as in xterm. Homes the cursor.
    pub(super) fn set_lr_margins(&mut self, left_1: u16, right_1: u16) {
        let last = self.cols.saturating_sub(1);
        let l = left_1.saturating_sub(1).min(last);
        let r = right_1.saturating_sub(1).min(last);
        if r <= l {
            return;
        }
        self.vt.left = l;
        self.vt.right = r;
        self.cursor_position(1, 1);
    }

    pub(super) fn reset_lr_margins(&mut self) {
        self.vt.left = 0;
        self.vt.right = self.cols.saturating_sub(1);
    }

    // ----- deferred wrap -----

    /// Whether the margin / no-autowrap deferred-wrap flag still belongs
    /// to the current cursor position.
    pub(super) fn wrap_pending(&self) -> bool {
        self.vt.wrap_pending_at == Some((self.cursor.row, self.cursor.col))
    }

    // ----- origin mode -----

    /// Absolute row for a 1-based row parameter, honouring DECOM.
    pub(super) fn origin_row(&self, row_1: u16) -> u16 {
        let r = row_1.saturating_sub(1);
        if self.vt.origin_mode {
            let bot = self.scroll_bot.min(self.rows.saturating_sub(1));
            self.scroll_top.saturating_add(r).min(bot)
        } else {
            r
        }
    }

    /// Absolute column for a 1-based column parameter, honouring DECOM
    /// (relative to the left margin when DECLRMM is on).
    pub(super) fn origin_col(&self, col_1: u16) -> u16 {
        let c = col_1.saturating_sub(1);
        if self.vt.origin_mode {
            let (l, r) = self.lr_bounds();
            l.saturating_add(c).min(r)
        } else {
            c
        }
    }

    /// DSR 6 / DECXCPR — cursor position report (relative to the margins
    /// under DECOM, like xterm).
    pub(super) fn report_cursor_position(&mut self, private: bool) {
        let mut row = self.cursor.row;
        let mut col = self.cursor.col.min(self.cols.saturating_sub(1));
        if self.vt.origin_mode {
            row = row.saturating_sub(self.scroll_top);
            col = col.saturating_sub(self.lr_bounds().0);
        }
        let q = if private { "?" } else { "" };
        let reply = format!("\x1b[{q}{};{}R", row + 1, col + 1);
        self.pty_replies.extend_from_slice(reply.as_bytes());
    }

    fn home_cursor(&mut self) {
        self.cursor_position(1, 1);
    }

    pub(super) fn set_origin_mode(&mut self, enable: bool) {
        self.vt.origin_mode = enable;
        // Setting or resetting DECOM homes the cursor (to the margin
        // origin when set).
        self.home_cursor();
    }

    // ----- DECSC / DECRC / SCOSC / SCORC / 1048 -----

    pub(super) fn save_cursor(&mut self) {
        self.decsc_saved = Some(self.cursor);
        self.vt.decsc_origin = self.vt.origin_mode;
        self.vt.decsc_autowrap = self.vt.autowrap;
    }

    /// Restore the DECSC snapshot. Without a prior save xterm homes the
    /// cursor and resets SGR (and DECOM).
    pub(super) fn restore_cursor(&mut self) {
        let old = self.cursor;
        if let Some(saved) = self.decsc_saved {
            self.cursor = saved;
            self.vt.origin_mode = self.vt.decsc_origin;
            self.vt.autowrap = self.vt.decsc_autowrap;
        } else {
            self.cursor = Cursor::default();
            self.vt.origin_mode = false;
        }
        self.vt.wrap_pending_at = None;
        self.clamp_cursor();
        self.mark_cell(old.row, old.col.min(self.cols.saturating_sub(1)));
        self.mark_cell(
            self.cursor.row,
            self.cursor.col.min(self.cols.saturating_sub(1)),
        );
    }

    // ----- tab stops -----

    pub(super) fn reset_tab_stops(&mut self) {
        self.vt.tab_stops = default_tab_stops(self.cols);
    }

    /// HTS (`ESC H`): set a tab stop at the cursor column.
    pub(super) fn set_tab_stop(&mut self) {
        let col = usize::from(self.cursor.col.min(self.cols.saturating_sub(1)));
        if let Some(slot) = self.vt.tab_stops.get_mut(col) {
            *slot = true;
        }
    }

    /// TBC (`CSI Ps g`): `0` clears the stop at the cursor, `3` clears
    /// all of them. Other values are ignored.
    pub(super) fn clear_tab_stops(&mut self, mode: u16) {
        match mode {
            0 => {
                let col = usize::from(self.cursor.col.min(self.cols.saturating_sub(1)));
                if let Some(slot) = self.vt.tab_stops.get_mut(col) {
                    *slot = false;
                }
            }
            3 => self.vt.tab_stops.iter_mut().for_each(|s| *s = false),
            _ => {}
        }
    }

    /// Next tab stop strictly right of `col`, capped at `limit`.
    fn next_tab_stop(&self, col: u16, limit: u16) -> u16 {
        let start = usize::from(col) + 1;
        let end = usize::from(limit);
        (start..=end)
            .find(|&c| self.vt.tab_stops.get(c).copied().unwrap_or(false))
            .map_or(limit, |c| u16::try_from(c).unwrap_or(limit))
    }

    /// HT / CHT: advance `n` tab stops. Stops at the right margin when
    /// the cursor is inside it, else at the last column.
    pub(super) fn tab_forward(&mut self, n: u16) {
        let row = self.cursor.row;
        let max_col = self.cols.saturating_sub(1);
        let old_col = self.cursor.col.min(max_col);
        let limit = if self.vt.lr_margin_mode && old_col <= self.vt.right {
            self.vt.right.min(max_col)
        } else {
            max_col
        };
        let from = self.cursor.col;
        let mut col = old_col;
        for _ in 0..n {
            if col >= limit {
                break;
            }
            col = self.next_tab_stop(col, limit);
        }
        self.vt.wrap_pending_at = None;
        self.cursor.col = col;
        if n == 1 {
            self.mark_tab_run(row, from, col);
        }
        self.mark_cell(row, old_col);
        self.mark_cell(row, col);
    }

    /// CBT (`CSI Ps Z`): move back `n` tab stops, stopping at the left
    /// margin (inside it) or column 0.
    pub(super) fn tab_backward(&mut self, n: u16) {
        let row = self.cursor.row;
        let max_col = self.cols.saturating_sub(1);
        let old_col = self.cursor.col.min(max_col);
        let floor = if self.vt.lr_margin_mode && old_col >= self.vt.left {
            self.vt.left
        } else {
            0
        };
        let mut col = old_col;
        for _ in 0..n {
            if col <= floor {
                break;
            }
            col = (usize::from(floor)..usize::from(col))
                .rev()
                .find(|&c| self.vt.tab_stops.get(c).copied().unwrap_or(false))
                .map_or(floor, |c| u16::try_from(c).unwrap_or(floor))
                .max(floor);
        }
        self.vt.wrap_pending_at = None;
        self.cursor.col = col;
        self.mark_cell(row, old_col);
        self.mark_cell(row, col);
    }

    // ----- REP -----

    /// REP (`CSI Ps b`): print the last graphic character `n` more times.
    /// Capped at one screenful so a huge count can't stall the parser.
    pub(super) fn repeat_last_graphic(&mut self, n: u16) {
        let Some(c) = self.vt.last_graphic else {
            return;
        };
        let cap = usize::from(self.rows) * usize::from(self.cols);
        for _ in 0..usize::from(n).min(cap) {
            self.print_char(c);
        }
    }

    // ----- line moves -----

    /// CNL / CPL: move `n` rows down (`down = true`) or up, then to the
    /// line start (left margin when inside it).
    pub(super) fn cursor_next_prev_line(&mut self, n: u16, down: bool) {
        if down {
            self.cursor_down(n);
        } else {
            self.cursor_up(n);
        }
        let col = self.line_start_col();
        self.move_cursor(self.cursor.row, col);
    }

    // ----- DECALN -----

    /// DECALN (`ESC # 8`): fill the screen with `E`, reset the margins
    /// and home the cursor (absolute, regardless of DECOM).
    pub(super) fn decaln(&mut self) {
        let cols = self.cols;
        let rows = self.rows;
        let e = Cell {
            ch: 'E',
            style: Style::RESET,
            is_continuation: false,
            wrap_pad: false,
            hyperlink_id: None,
            extra: None,
        };
        let grid = self.active_grid_mut();
        for r in 0..rows {
            let row = grid.row_mut(r);
            row.resize_cols(cols);
            row.rich = None;
            row.soft_wrap = false;
            for c in &mut row.cells {
                *c = e;
            }
        }
        self.scroll_top = 0;
        self.scroll_bot = rows.saturating_sub(1);
        self.reset_lr_margins();
        self.vt.origin_mode = false;
        self.vt.wrap_pending_at = None;
        self.cursor.row = 0;
        self.cursor.col = 0;
        self.mark_all_dirty();
    }

    // ----- DECSTR -----

    /// DECSTR (`CSI ! p`) — soft terminal reset. Resets the modes and
    /// pen state xterm's soft reset covers, without clearing the screen
    /// or moving the cursor.
    pub(super) fn soft_reset(&mut self) {
        if !self.cursor_visible {
            self.cursor_visible = true;
            let col = self.cursor.col.min(self.cols.saturating_sub(1));
            self.mark_cell(self.cursor.row, col);
        }
        self.vt.insert_mode = false;
        self.vt.origin_mode = false;
        self.vt.autowrap = true;
        self.vt.app_cursor_keys = false;
        self.vt.app_keypad = false;
        self.vt.lr_margin_mode = false;
        self.vt.wrap_pending_at = None;
        self.reset_lr_margins();
        self.scroll_top = 0;
        self.scroll_bot = self.rows.saturating_sub(1);
        self.cursor.style = Style::RESET;
        self.cursor_underline_color = None;
        self.decsc_saved = None;
        self.vt.decsc_origin = false;
        self.vt.decsc_autowrap = true;
        self.reset_tab_stops();
        // Both screens' kitty keyboard flag stacks, as kitty does.
        self.reset_kitty_keyboard_stacks();
    }

    /// Reset everything in [`VtModes`] to power-on defaults (RIS).
    pub(super) fn hard_reset_vt(&mut self) {
        if self.vt.reverse_video {
            self.mark_all_dirty();
        }
        self.vt = VtModes::new(self.cols);
    }

    // ----- DECSCNM -----

    pub(super) fn set_reverse_video(&mut self, enable: bool) {
        if self.vt.reverse_video != enable {
            self.vt.reverse_video = enable;
            // Every cell's effective colors flip.
            self.mark_all_dirty();
        }
    }

    // ----- ANSI (non-private) modes -----

    /// SM / RM (`CSI Ps h` / `CSI Ps l`). Only IRM (4) is modelled.
    pub(super) fn apply_ansi_mode(&mut self, params: &toastty_parser::Params, enable: bool) {
        for sub in params {
            if sub.first().copied() == Some(4) {
                self.vt.insert_mode = enable;
            }
        }
    }

    /// ANSI DECRQM (`CSI Ps $ p`) status: 1 set, 2 reset, 4 permanently
    /// reset, 0 unknown.
    pub(super) fn ansi_decrqm_status(&self, ps: u16) -> u16 {
        match ps {
            4 => {
                if self.vt.insert_mode {
                    1
                } else {
                    2
                }
            }
            // KAM (keyboard lock), SRM (local echo — "send/receive"
            // is set = no echo, i.e. permanently set) and LNM are not
            // supported; report them as fixed.
            2 | 20 => 4,
            12 => 3,
            _ => 0,
        }
    }

    // ----- horizontal-margin scrolling -----

    /// Scroll the rectangle `[top..=bot] x [left..=right]` up by one row.
    /// Used when DECSLRM narrows the screen; never touches scrollback.
    pub(super) fn rect_scroll_up(&mut self, top: u16, bot: u16) {
        let (l, r) = self.lr_bounds();
        let style = self.cursor.style;
        let cols = self.cols;
        let grid = self.active_grid_mut();
        for row in top..bot {
            let src: Vec<Cell> = {
                let next = grid.row_mut(row + 1);
                next.resize_cols(cols);
                next.cells[usize::from(l)..=usize::from(r)].to_vec()
            };
            let dst = grid.row_mut(row);
            dst.resize_cols(cols);
            dst.rich = None;
            dst.soft_wrap = false;
            dst.cells[usize::from(l)..=usize::from(r)].copy_from_slice(&src);
        }
        let last = grid.row_mut(bot);
        last.resize_cols(cols);
        last.soft_wrap = false;
        last.erase(l, r + 1, style);
        for row in top..=bot {
            self.mark_cells(row, l, r + 1);
        }
    }

    /// Scroll the margin rectangle down by one row (mirror of
    /// [`Self::rect_scroll_up`]).
    pub(super) fn rect_scroll_down(&mut self, top: u16, bot: u16) {
        let (l, r) = self.lr_bounds();
        let style = self.cursor.style;
        let cols = self.cols;
        let grid = self.active_grid_mut();
        for row in (top + 1..=bot).rev() {
            let src: Vec<Cell> = {
                let prev = grid.row_mut(row - 1);
                prev.resize_cols(cols);
                prev.cells[usize::from(l)..=usize::from(r)].to_vec()
            };
            let dst = grid.row_mut(row);
            dst.resize_cols(cols);
            dst.rich = None;
            dst.soft_wrap = false;
            dst.cells[usize::from(l)..=usize::from(r)].copy_from_slice(&src);
        }
        let first = grid.row_mut(top);
        first.resize_cols(cols);
        first.soft_wrap = false;
        first.erase(l, r + 1, style);
        for row in top..=bot {
            self.mark_cells(row, l, r + 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::MouseProtocol;
    use super::*;
    use toastty_parser::Parser;

    fn feed(t: &mut Term, bytes: &[u8]) {
        let mut p = Parser::new();
        p.advance(t, bytes);
    }

    fn run(rows: u16, cols: u16, bytes: &[u8]) -> Term {
        let mut t = Term::new(rows, cols, 0);
        feed(&mut t, bytes);
        t
    }

    fn row_text(t: &Term, r: u16) -> String {
        let s: String = t
            .row(r)
            .cells
            .iter()
            .filter(|c| !c.is_continuation)
            .map(|c| c.ch)
            .collect();
        s.trim_end().to_string()
    }

    fn pos(t: &Term) -> (u16, u16) {
        (t.cursor().row, t.cursor().col)
    }

    fn replies(t: &mut Term) -> String {
        String::from_utf8(t.drain_pty_replies()).unwrap()
    }

    // ----- REP -----

    #[test]
    fn rep_repeats_last_graphic_char() {
        let t = run(3, 20, b"ab\x1b[3b");
        assert_eq!(row_text(&t, 0), "abbbb");
        assert_eq!(pos(&t), (0, 5));
    }

    #[test]
    fn rep_defaults_to_one_and_wraps() {
        let t = run(3, 4, b"xyz\x1b[b\x1b[2b");
        assert_eq!(row_text(&t, 0), "xyzz");
        assert_eq!(row_text(&t, 1), "zz");
    }

    #[test]
    fn rep_without_prior_print_is_noop_and_huge_count_is_capped() {
        let t = run(2, 4, b"\x1b[5b");
        assert_eq!(row_text(&t, 0), "");
        // One screenful (8 cells) of repeats after the original `q`: the
        // ninth `q` scrolls, leaving a full row above a single `q`.
        let t = run(2, 4, b"q\x1b[65535b");
        assert_eq!(row_text(&t, 0), "qqqq");
        assert_eq!(row_text(&t, 1), "q");
    }

    #[test]
    fn rep_repeats_wide_chars() {
        let t = run(2, 10, "漢\x1b[2b".as_bytes());
        assert_eq!(row_text(&t, 0), "漢漢漢");
        assert_eq!(pos(&t), (0, 6));
    }

    // ----- CNL / CPL / HPA / HPR / VPR -----

    #[test]
    fn cnl_cpl_move_lines_and_return_to_column_zero() {
        let mut t = run(10, 20, b"\x1b[3;8H\x1b[2E");
        assert_eq!(pos(&t), (4, 0));
        feed(&mut t, b"\x1b[1;5H\x1b[5;9H\x1b[3F");
        assert_eq!(pos(&t), (1, 0));
        feed(&mut t, b"\x1b[F\x1b[F");
        assert_eq!(pos(&t), (0, 0));
    }

    #[test]
    fn hpa_hpr_vpr() {
        let mut t = run(10, 20, b"\x1b[3;4H\x1b[12`");
        assert_eq!(pos(&t), (2, 11));
        feed(&mut t, b"\x1b[3a");
        assert_eq!(pos(&t), (2, 14));
        feed(&mut t, b"\x1b[a");
        assert_eq!(pos(&t), (2, 15));
        feed(&mut t, b"\x1b[4e");
        assert_eq!(pos(&t), (6, 15));
        feed(&mut t, b"\x1b[99a\x1b[99e");
        assert_eq!(pos(&t), (9, 19));
    }

    // ----- tab stops -----

    #[test]
    fn default_tab_stops_every_eight() {
        let t = Term::new(2, 30, 0);
        assert_eq!(t.tab_stops(), vec![8, 16, 24]);
    }

    #[test]
    fn hts_tbc_custom_stops() {
        let mut t = run(2, 40, b"\x1b[3g\x1b[1;5H\x1bH\x1b[1;13H\x1bH\r");
        assert_eq!(t.tab_stops(), vec![4, 12]);
        feed(&mut t, b"\tA\tB\tC");
        // No stop after 12: the last HT runs to the final column.
        assert_eq!(row_text(&t, 0), format!("    A       B{}C", " ".repeat(26)));
        // TBC 0 clears just the stop under the cursor.
        feed(&mut t, b"\x1b[1;5H\x1b[0g");
        assert_eq!(t.tab_stops(), vec![12]);
        feed(&mut t, b"\x1b[g");
        assert_eq!(t.tab_stops(), vec![12], "TBC at a non-stop is harmless");
        feed(&mut t, b"\x1b[1;13H\x1b[g");
        assert_eq!(t.tab_stops(), Vec::<u16>::new());
        // Unknown TBC mode is ignored.
        feed(&mut t, b"\x1bH\x1b[2g");
        assert_eq!(t.tab_stops(), vec![12]);
    }

    #[test]
    fn cht_and_cbt() {
        let mut t = run(2, 40, b"\x1b[2I");
        assert_eq!(pos(&t), (0, 16));
        feed(&mut t, b"\x1b[I");
        assert_eq!(pos(&t), (0, 24));
        feed(&mut t, b"\x1b[9I");
        assert_eq!(pos(&t), (0, 39), "CHT clamps at the last column");
        feed(&mut t, b"\x1b[Z");
        assert_eq!(pos(&t), (0, 32));
        feed(&mut t, b"\x1b[1;20H\x1b[Z");
        assert_eq!(pos(&t), (0, 16));
        feed(&mut t, b"\x1b[5Z");
        assert_eq!(pos(&t), (0, 0), "CBT stops at column 0");
    }

    #[test]
    fn tab_stops_reset_on_ris_decstr_and_resize() {
        let mut t = run(2, 30, b"\x1b[3g");
        assert_eq!(t.tab_stops(), Vec::<u16>::new());
        feed(&mut t, b"\x1bc");
        assert_eq!(t.tab_stops(), vec![8, 16, 24]);
        feed(&mut t, b"\x1b[3g\x1b[!p");
        assert_eq!(t.tab_stops(), vec![8, 16, 24]);
        feed(&mut t, b"\x1b[3g");
        t.resize(2, 20);
        assert_eq!(t.tab_stops(), vec![8, 16]);
    }

    // ----- SCOSC / SCORC -----

    #[test]
    fn scosc_scorc_save_and_restore_cursor() {
        let mut t = run(10, 20, b"\x1b[4;6H\x1b[31m\x1b[s\x1b[H\x1b[0m");
        assert_eq!(pos(&t), (0, 0));
        feed(&mut t, b"\x1b[u");
        assert_eq!(pos(&t), (3, 5));
        assert_eq!(t.cursor().style.fg, crate::cell::Color::Red);
    }

    #[test]
    fn scorc_without_save_homes_cursor() {
        let t = run(10, 20, b"\x1b[4;6H\x1b[u");
        assert_eq!(pos(&t), (0, 0));
    }

    #[test]
    fn kitty_keyboard_u_forms_still_work() {
        let mut t = run(2, 10, b"\x1b[>1u");
        assert_eq!(t.kitty_flags(), 1);
        feed(&mut t, b"\x1b[?u");
        assert_eq!(replies(&mut t), "\x1b[?1u");
        feed(&mut t, b"\x1b[<u");
        assert_eq!(t.kitty_flags(), 0);
    }

    // ----- IRM -----

    #[test]
    fn irm_inserts_instead_of_overwriting() {
        let mut t = run(2, 10, b"abcdef\r\x1b[4hXY");
        assert_eq!(row_text(&t, 0), "XYabcdef");
        assert!(t.insert_mode());
        feed(&mut t, b"\x1b[4lZ");
        assert_eq!(row_text(&t, 0), "XYZbcdef");
        assert!(!t.insert_mode());
    }

    #[test]
    fn irm_pushes_text_off_the_right_edge() {
        let t = run(2, 6, b"abcdef\r\x1b[4h12");
        assert_eq!(row_text(&t, 0), "12abcd");
    }

    #[test]
    fn ansi_decrqm_reports_irm() {
        let mut t = run(2, 10, b"\x1b[4$p");
        assert_eq!(replies(&mut t), "\x1b[4;2$y");
        feed(
            &mut t,
            b"\x1b[4h\x1b[4$p\x1b[20$p\x1b[2$p\x1b[12$p\x1b[99$p",
        );
        assert_eq!(
            replies(&mut t),
            "\x1b[4;1$y\x1b[20;4$y\x1b[2;4$y\x1b[12;3$y\x1b[99;0$y"
        );
    }

    // ----- DECALN / DA3 / DECSTR -----

    #[test]
    fn decaln_fills_screen_and_resets_margins() {
        let mut t = run(3, 4, b"\x1b[2;3r\x1b[2;2Hzz\x1b#8");
        for r in 0..3 {
            assert_eq!(row_text(&t, r), "EEEE");
        }
        assert_eq!(pos(&t), (0, 0));
        // Full-screen region again: LF on the last row scrolls.
        feed(&mut t, b"\x1b[3;1H\n");
        assert_eq!(row_text(&t, 2), "");
        // Other `ESC #` finals are ignored.
        feed(&mut t, b"\x1b#3");
    }

    #[test]
    fn da3_reports_unit_id() {
        let mut t = run(2, 10, b"\x1b[=c\x1b[=0c");
        assert_eq!(
            replies(&mut t),
            "\x1bP!|00000000\x1b\\\x1bP!|00000000\x1b\\"
        );
    }

    #[test]
    fn decstr_soft_resets_modes_but_keeps_screen() {
        let mut t = run(
            5,
            10,
            b"hi\x1b[4h\x1b[?6h\x1b[?7l\x1b[?1h\x1b=\x1b[?25l\x1b[2;4r\x1b[1m\x1b7",
        );
        feed(&mut t, b"\x1b[!p");
        assert!(!t.insert_mode());
        assert!(!t.origin_mode());
        assert!(t.autowrap());
        assert!(!t.app_cursor_keys());
        assert!(!t.app_keypad());
        assert!(t.cursor_visible());
        assert_eq!(t.cursor().style, Style::RESET);
        assert_eq!(row_text(&t, 0), "hi");
        // Scroll region reset: LF at the last row scrolls the screen.
        feed(&mut t, b"\x1b[5;1H\n");
        assert_eq!(row_text(&t, 0), "");
        // Saved cursor cleared → DECRC homes.
        feed(&mut t, b"\x1b8");
        assert_eq!(pos(&t), (0, 0));
    }

    // ----- DECCKM / DECKPAM / DECNKM / 1007 / 1036 / 1039 -----

    #[test]
    fn keypad_and_cursor_key_modes() {
        let mut t = Term::new(2, 10, 0);
        assert!(!t.app_cursor_keys() && !t.app_keypad());
        feed(&mut t, b"\x1b[?1h\x1b=");
        assert!(t.app_cursor_keys() && t.app_keypad());
        feed(&mut t, b"\x1b[?1$p\x1b[?66$p");
        assert_eq!(replies(&mut t), "\x1b[?1;1$y\x1b[?66;1$y");
        feed(&mut t, b"\x1b>\x1b[?1l");
        assert!(!t.app_cursor_keys() && !t.app_keypad());
        feed(&mut t, b"\x1b[?66h");
        assert!(t.app_keypad());
        feed(&mut t, b"\x1b[?66l");
        assert!(!t.app_keypad());
    }

    #[test]
    fn alternate_scroll_and_alt_escape_modes() {
        let mut t = Term::new(2, 10, 0);
        assert!(t.alternate_scroll());
        assert!(t.alt_sends_escape());
        feed(&mut t, b"\x1b[?1007l");
        assert!(!t.alternate_scroll());
        feed(&mut t, b"\x1b[?1036l");
        assert!(t.alt_sends_escape(), "1039 still set");
        feed(&mut t, b"\x1b[?1039l");
        assert!(!t.alt_sends_escape());
        feed(&mut t, b"\x1b[?1007$p\x1b[?1036$p\x1b[?1039$p");
        assert_eq!(
            replies(&mut t),
            "\x1b[?1007;2$y\x1b[?1036;2$y\x1b[?1039;2$y"
        );
        feed(&mut t, b"\x1b[?1036h\x1b[?1007h");
        assert!(t.alt_sends_escape() && t.alternate_scroll());
        feed(&mut t, b"\x1bc");
        assert!(t.alt_sends_escape() && t.alternate_scroll());
    }

    // ----- DECSCNM / att610 -----

    #[test]
    fn reverse_video_toggles_and_dirties_screen() {
        let mut t = run(3, 10, b"abc");
        t.clear_damage();
        feed(&mut t, b"\x1b[?5h");
        assert!(t.reverse_video());
        assert!(t.damage().rows.iter().all(|r| !r.is_empty()));
        t.clear_damage();
        feed(&mut t, b"\x1b[?5h");
        assert!(
            t.damage()
                .rows
                .iter()
                .all(crate::damage::RowDamage::is_empty),
            "no-op set"
        );
        feed(&mut t, b"\x1b[?5$p\x1b[?5l\x1b[?5$p");
        assert_eq!(replies(&mut t), "\x1b[?5;1$y\x1b[?5;2$y");
        feed(&mut t, b"\x1b[?5h\x1bc");
        assert!(!t.reverse_video());
    }

    #[test]
    fn att610_toggles_cursor_blink() {
        let mut t = Term::new(2, 10, 0);
        assert!(t.cursor_blink());
        feed(&mut t, b"\x1b[?12l");
        assert!(!t.cursor_blink());
        feed(&mut t, b"\x1b[?12$p");
        assert_eq!(replies(&mut t), "\x1b[?12;2$y");
        feed(&mut t, b"\x1b[?12h");
        assert!(t.cursor_blink());
    }

    // ----- DECAWM -----

    #[test]
    fn decawm_off_overwrites_last_column() {
        let mut t = run(4, 5, b"\x1b[?7labcdefg");
        assert_eq!(row_text(&t, 0), "abcdg");
        assert_eq!(row_text(&t, 1), "");
        assert_eq!(pos(&t), (0, 4));
        feed(&mut t, b"\x1b[?7$p");
        assert_eq!(replies(&mut t), "\x1b[?7;2$y");
        // Wide char at the edge lands in the last two columns.
        feed(&mut t, "\r\n12345漢".as_bytes());
        assert_eq!(row_text(&t, 1), "123漢");
        // Re-enable: the next print wraps normally.
        feed(&mut t, b"\x1b[?7h\x1b[3;1Hvwxyz!");
        assert_eq!(row_text(&t, 2), "vwxyz");
        assert_eq!(row_text(&t, 3), "!");
    }

    #[test]
    fn decawm_off_combining_mark_attaches_to_last_column() {
        let t = run(2, 3, "\x1b[?7labc\u{301}".as_bytes());
        let cell = t.row(0).cells[2];
        assert_eq!(cell.ch, 'c');
        assert!(cell.extra.is_some(), "accent joined the last cell");
    }

    // ----- DECOM -----

    #[test]
    fn decom_makes_cup_relative_to_region() {
        let mut t = run(10, 20, b"\x1b[3;7r\x1b[?6h");
        assert!(t.origin_mode());
        assert_eq!(pos(&t), (2, 0), "DECOM homes to the region origin");
        feed(&mut t, b"\x1b[2;5H");
        assert_eq!(pos(&t), (3, 4));
        feed(&mut t, b"\x1b[99;1H");
        assert_eq!(pos(&t), (6, 0), "clamped to the bottom margin");
        feed(&mut t, b"\x1b[6n");
        assert_eq!(replies(&mut t), "\x1b[5;1R");
        feed(&mut t, b"\x1b[?6n");
        assert_eq!(replies(&mut t), "\x1b[?5;1R");
        feed(&mut t, b"\x1b[3d");
        assert_eq!(pos(&t), (4, 0));
        feed(&mut t, b"\x1b[?6l");
        assert_eq!(pos(&t), (0, 0), "resetting DECOM homes absolutely");
        feed(&mut t, b"\x1b[?6$p");
        assert_eq!(replies(&mut t), "\x1b[?6;2$y");
    }

    #[test]
    fn decsc_saves_origin_and_autowrap() {
        let mut t = run(10, 20, b"\x1b[3;7r\x1b[?6h\x1b[?7l\x1b7\x1b[?6l\x1b[?7h");
        assert!(!t.origin_mode() && t.autowrap());
        feed(&mut t, b"\x1b8");
        assert!(t.origin_mode() && !t.autowrap());
        assert_eq!(pos(&t), (2, 0));
    }

    #[test]
    fn cuu_cud_stop_at_scroll_margins() {
        let mut t = run(10, 20, b"\x1b[3;7r\x1b[5;1H\x1b[9A");
        assert_eq!(pos(&t), (2, 0));
        feed(&mut t, b"\x1b[9B");
        assert_eq!(pos(&t), (6, 0));
        // Outside the region the screen edges apply.
        feed(&mut t, b"\x1b[9;1H\x1b[9B");
        assert_eq!(pos(&t), (9, 0));
        feed(&mut t, b"\x1b[2;1H\x1b[9A");
        assert_eq!(pos(&t), (0, 0));
    }

    // ----- DECSLRM / DECLRMM -----

    #[test]
    fn decslrm_requires_declrmm() {
        let mut t = run(5, 10, b"\x1b[3;3H\x1b[2;5s");
        assert_eq!(t.lr_margins(), None, "plain CSI s is SCOSC");
        feed(&mut t, b"\x1b[?69h\x1b[2;5s");
        assert_eq!(t.lr_margins(), Some((1, 4)));
        assert_eq!(pos(&t), (0, 0));
        // Invalid pair ignored.
        feed(&mut t, b"\x1b[5;2s");
        assert_eq!(t.lr_margins(), Some((1, 4)));
        feed(&mut t, b"\x1b[?69$p");
        assert_eq!(replies(&mut t), "\x1b[?69;1$y");
        feed(&mut t, b"\x1b[?69l");
        assert_eq!(t.lr_margins(), None);
        feed(&mut t, b"\x1b[?69h");
        assert_eq!(t.lr_margins(), Some((0, 9)), "reset when the mode was left");
    }

    #[test]
    fn text_wraps_inside_lr_margins() {
        let mut t = run(4, 10, b"\x1b[?69h\x1b[3;6s\x1b[1;3Habcdefghij");
        assert_eq!(row_text(&t, 0), "  abcd");
        assert_eq!(row_text(&t, 1), "  efgh");
        assert_eq!(row_text(&t, 2), "  ij");
        assert!(!t.row(0).soft_wrap, "margin wraps are not reflow joins");
        // CR returns to the left margin.
        feed(&mut t, b"\rX");
        assert_eq!(row_text(&t, 2), "  Xj");
    }

    #[test]
    fn lr_margin_scroll_moves_only_the_rectangle() {
        let mut t = run(3, 6, b"111111\r\n222222\r\n333333");
        feed(&mut t, b"\x1b[?69h\x1b[2;4s\x1b[3;2H\n");
        assert_eq!(row_text(&t, 0), "122211");
        assert_eq!(row_text(&t, 1), "233322");
        assert_eq!(row_text(&t, 2), "3   33");
        // Reverse index at the top scrolls the rectangle down.
        feed(&mut t, b"\x1b[1;2H\x1bM");
        assert_eq!(row_text(&t, 0), "1   11");
        assert_eq!(row_text(&t, 1), "222222");
        // SU / SD.
        feed(&mut t, b"\x1b[S");
        assert_eq!(row_text(&t, 0), "122211");
        feed(&mut t, b"\x1b[T");
        assert_eq!(row_text(&t, 0), "1   11");
    }

    #[test]
    fn lf_outside_lr_margins_does_not_scroll() {
        let mut t = run(2, 6, b"aaaaaa\r\nbbbbbb");
        feed(&mut t, b"\x1b[?69h\x1b[2;4s\x1b[2;6H\n");
        assert_eq!(row_text(&t, 0), "aaaaaa");
        assert_eq!(row_text(&t, 1), "bbbbbb");
        feed(&mut t, b"\x1b[1;6H\x1bM");
        assert_eq!(row_text(&t, 0), "aaaaaa");
    }

    #[test]
    fn ich_dch_il_dl_respect_lr_margins() {
        let mut t = run(3, 8, b"abcdefgh\r\nijklmnop\r\nqrstuvwx");
        feed(&mut t, b"\x1b[?69h\x1b[2;5s\x1b[1;3H\x1b[@");
        assert_eq!(row_text(&t, 0), "ab cdfgh");
        feed(&mut t, b"\x1b[2P");
        assert_eq!(row_text(&t, 0), "abd  fgh");
        feed(&mut t, b"\x1b[2;3H\x1b[L");
        assert_eq!(row_text(&t, 1), "i    nop");
        assert_eq!(row_text(&t, 2), "qjklmvwx");
        feed(&mut t, b"\x1b[M");
        assert_eq!(row_text(&t, 1), "ijklmnop");
        assert_eq!(row_text(&t, 2), "q    vwx");
        // DL on the bottom row just clears the margin span.
        feed(&mut t, b"\x1b[3;3H\x1b[M");
        assert_eq!(row_text(&t, 2), "q    vwx");
        // Outside the margins: no-ops.
        feed(&mut t, b"\x1b[1;8H\x1b[@\x1b[P\x1b[L");
        assert_eq!(row_text(&t, 0), "abd  fgh");
    }

    #[test]
    fn cuf_cub_stop_at_lr_margins() {
        let mut t = run(2, 20, b"\x1b[?69h\x1b[5;10s\x1b[1;7H\x1b[99C");
        assert_eq!(pos(&t), (0, 9));
        feed(&mut t, b"\x1b[99D");
        assert_eq!(pos(&t), (0, 4));
        // Outside the margins the screen edges apply.
        feed(&mut t, b"\x1b[1;2H\x1b[99D");
        assert_eq!(pos(&t), (0, 0));
    }

    #[test]
    fn decom_with_lr_margins_is_relative_to_left_margin() {
        let mut t = run(5, 20, b"\x1b[?69h\x1b[5;10s\x1b[?6h");
        assert_eq!(pos(&t), (0, 4));
        feed(&mut t, b"\x1b[1;3H");
        assert_eq!(pos(&t), (0, 6));
        feed(&mut t, b"\x1b[1;99H");
        assert_eq!(pos(&t), (0, 9));
        feed(&mut t, b"\x1b[8G");
        assert_eq!(pos(&t), (0, 9));
        feed(&mut t, b"\x1b[2G\x1b[6n");
        assert_eq!(replies(&mut t), "\x1b[1;2R");
    }

    #[test]
    fn tabs_stop_at_right_margin_and_cbt_at_left() {
        let mut t = run(2, 30, b"\x1b[?69h\x1b[3;12s\x1b[1;4H\t");
        assert_eq!(pos(&t), (0, 8));
        feed(&mut t, b"\t");
        assert_eq!(pos(&t), (0, 11));
        feed(&mut t, b"\x1b[3Z");
        assert_eq!(pos(&t), (0, 2));
    }

    #[test]
    fn resize_resets_lr_margins() {
        let mut t = run(4, 20, b"\x1b[?69h\x1b[5;10s");
        t.resize(4, 30);
        assert_eq!(t.lr_margins(), Some((0, 29)));
    }

    #[test]
    fn mode_defaults_after_ris() {
        let mut t = run(
            4,
            20,
            b"x\x1b[4h\x1b[?6h\x1b[?7l\x1b[?1h\x1b=\x1b[?69h\x1b[5;10s\x1b[?1000h",
        );
        feed(&mut t, b"\x1bc\x1b[b");
        assert!(!t.insert_mode() && !t.origin_mode() && t.autowrap());
        assert!(!t.app_cursor_keys() && !t.app_keypad());
        assert_eq!(t.lr_margins(), None);
        assert_eq!(t.mouse_mode().protocol, MouseProtocol::Off);
        assert_eq!(row_text(&t, 0), "", "REP char forgotten");
    }
}
