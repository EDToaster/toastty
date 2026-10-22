//! `Term` side of kitty Unicode placeholders.
//!
//! A placeholder image is text. Each `U+10EEEE` cell is stored in the
//! grid like any other character: its foreground colour carries the low
//! bits of the image id, and a [`PlaceholderExtra`] carries the rest
//! (row / column / id-MSB diacritics and the placement id taken from the
//! SGR 58 underline colour). Nothing is decided when the cell is
//! printed. Every frame the renderer asks for the runs of placeholder
//! cells in the rows it is about to draw ([`Term::placeholder_runs`]),
//! looks up each run's virtual placement ([`Term::resolve_placeholder`])
//! and draws the run's slice of the fitted image
//! ([`toastty_graphics::placeholder_slice`]).
//!
//! Because the image is derived from the cells, everything that moves or
//! erases text moves or erases the image too: overwrite, EL / ECH / DCH
//! / ICH, scrolling, scroll regions, scrollback, reflow on resize, tmux
//! redraws. Placeholders printed before the image or its virtual
//! placement arrive show up as soon as they do.
//!
//! Virtual placements live in each screen's images (`ScreenImages`, #42),
//! one table per screen like kitty's, and swap with the screen. Only the id-based deletes (`d=i`/`n`/`r` and
//! capitals) and re-transmitting the image remove them; ED 2, RIS and
//! the positional deletes don't (kitty 0.49.2).

use super::Term;
use crate::cell::{Cell, Color};
use crate::extra::{CellExtra, CellExtraId, ExtraTable, PlaceholderExtra};
use crate::grid::Row;
use toastty_graphics::{VirtualPlacement, VirtualPlacements};

/// A horizontal run of placeholder cells that shows consecutive image
/// cells of one row of one virtual placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaceholderRun {
    /// First grid column of the run.
    pub col: u16,
    /// Number of cells.
    pub len: u16,
    /// Full image id: `(id_msb << 24) | foreground bits`.
    pub image_id: u32,
    /// Placement id from the underline colour; `0` = any.
    pub placement_id: u32,
    /// Image row (0-based) the run shows.
    pub img_row: u32,
    /// Image column (0-based) of the run's first cell.
    pub img_col: u32,
}

/// The id a placeholder cell's colour encodes (kitty `color_to_id`): a
/// palette index for the 16 named and 256 indexed colours, the 24-bit
/// value for truecolour, and `0` for the default colour.
pub(crate) fn color_to_id(c: Color) -> u32 {
    match c {
        Color::Default | Color::Black => 0,
        Color::Red => 1,
        Color::Green => 2,
        Color::Yellow => 3,
        Color::Blue => 4,
        Color::Magenta => 5,
        Color::Cyan => 6,
        Color::White => 7,
        Color::BrightBlack => 8,
        Color::BrightRed => 9,
        Color::BrightGreen => 10,
        Color::BrightYellow => 11,
        Color::BrightBlue => 12,
        Color::BrightMagenta => 13,
        Color::BrightCyan => 14,
        Color::BrightWhite => 15,
        Color::Indexed256(i) => u32::from(i),
        Color::Rgb(r, g, b) => (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b),
    }
}

/// One placeholder cell, decoded. Diacritic values are 1-based, `0` =
/// not given (see [`PlaceholderExtra`]).
#[derive(Debug, Clone, Copy)]
struct Decoded {
    id_low: u32,
    placement_id: u32,
    row: u16,
    col: u16,
    id_msb: u16,
}

fn decode(cell: &Cell, extras: &ExtraTable) -> Option<Decoded> {
    if cell.ch != crate::PLACEHOLDER || cell.is_continuation {
        return None;
    }
    let ph = match cell.extra.and_then(|id| extras.get(id)) {
        Some(CellExtra::Placeholder(p)) => *p,
        _ => PlaceholderExtra::default(),
    };
    Some(Decoded {
        id_low: color_to_id(cell.style.fg),
        placement_id: ph.placement_id,
        row: ph.row,
        col: ph.col,
        id_msb: ph.id_msb,
    })
}

/// Run being accumulated by [`resolve_runs`]. Values are 0-based;
/// `row: None` is a run whose first cell gave no row diacritic.
struct Open {
    col: u16,
    len: u16,
    id_low: u32,
    placement_id: u32,
    row: Option<u32>,
    img_col: u32,
    id_msb: u32,
}

impl Open {
    fn start(col: u16, d: Decoded) -> Self {
        Self {
            col,
            len: 1,
            id_low: d.id_low,
            placement_id: d.placement_id,
            row: d.row.checked_sub(1).map(u32::from),
            img_col: u32::from(d.col.saturating_sub(1)),
            id_msb: u32::from(d.id_msb.saturating_sub(1)),
        }
    }

    /// The spec's inheritance rules: a cell continues the run when its
    /// colours match and every diacritic it does give agrees with what
    /// it would inherit from the cell to its left.
    fn continues(&self, d: Decoded) -> bool {
        d.id_low == self.id_low
            && d.placement_id == self.placement_id
            && (d.row == 0 || self.row == Some(u32::from(d.row - 1)))
            && (d.col == 0 || u32::from(d.col - 1) == self.img_col + u32::from(self.len))
            && (d.id_msb == 0 || u32::from(d.id_msb - 1) == self.id_msb)
    }

    fn finish(self, out: &mut Vec<PlaceholderRun>) {
        let image_id = (self.id_msb << 24) | self.id_low;
        // A run that never got a row (bare cells) or names image 0 shows
        // nothing, as in kitty.
        if let Some(img_row) = self.row
            && image_id != 0
        {
            out.push(PlaceholderRun {
                col: self.col,
                len: self.len,
                image_id,
                placement_id: self.placement_id,
                img_row,
                img_col: self.img_col,
            });
        }
    }
}

/// Split `cells` into placeholder runs, left to right, following kitty
/// (`screen_render_line_graphics`): a run starts at a placeholder cell
/// that can't continue the one to its left; missing diacritics on a
/// run's first cell default to column 0 and id-MSB 0, but a missing row
/// makes the whole run blank.
pub(crate) fn resolve_runs(cells: &[Cell], extras: &ExtraTable, out: &mut Vec<PlaceholderRun>) {
    let mut open: Option<Open> = None;
    for (i, cell) in cells.iter().enumerate() {
        let d = decode(cell, extras);
        if let (Some(d), Some(run)) = (d, open.as_mut())
            && run.continues(d)
        {
            run.len += 1;
            continue;
        }
        if let Some(run) = open.take() {
            run.finish(out);
        }
        if let Some(d) = d {
            open = Some(Open::start(u16::try_from(i).unwrap_or(u16::MAX), d));
        }
    }
    if let Some(run) = open {
        run.finish(out);
    }
}

impl Term {
    /// Extra for a placeholder cell about to be printed: the placement
    /// id from the current underline colour. `None` (a bare cell) when
    /// there is none.
    pub(super) fn placeholder_cell_extra(&mut self) -> Option<CellExtraId> {
        let placement_id = self.cursor_underline_color.map_or(0, color_to_id);
        if placement_id == 0 {
            return None;
        }
        self.intern_extra(CellExtra::Placeholder(PlaceholderExtra {
            placement_id,
            ..PlaceholderExtra::default()
        }))
    }

    /// Attach diacritic `c` to the placeholder cell just left of the
    /// cursor, filling its row, column and id-MSB slots in that order.
    /// Returns `false` (print `c` normally) when that cell isn't a
    /// placeholder or `c` isn't one of kitty's diacritics.
    pub(super) fn attach_placeholder_diacritic(&mut self, c: char) -> bool {
        let Some(idx) = toastty_graphics::diacritic_to_index(c) else {
            return false;
        };
        let row = self.cursor.row;
        // With a deferred wrap the cursor sits on the cell just written.
        let cur = if self.wrap_pending() {
            self.cursor.col + 1
        } else {
            self.cursor.col.min(self.cols)
        };
        let Some(col) = cur.checked_sub(1) else {
            return false;
        };
        let Some(&cell) = self.active_grid().row(row).cells.get(usize::from(col)) else {
            return false;
        };
        if cell.ch != crate::PLACEHOLDER || cell.is_continuation {
            return false;
        }
        let mut ph = match cell.extra.and_then(|id| self.extras.get(id)) {
            Some(CellExtra::Placeholder(p)) => *p,
            _ => PlaceholderExtra::default(),
        };
        // A fourth diacritic has no meaning; swallow it (it's
        // zero-width either way). Same when the extras table is full.
        if !ph.push_diacritic(idx) {
            return true;
        }
        let Some(id) = self.intern_extra(CellExtra::Placeholder(ph)) else {
            return true;
        };
        let max_cols = self.cols;
        self.active_grid_mut().row_mut(row).put(
            col,
            Cell {
                extra: Some(id),
                ..cell
            },
            max_cols,
        );
        self.mark_cell(row, col);
        true
    }

    /// Keep at most `target` distinct placeholder entries referenced by
    /// dropping the placeholder data (diacritics, placement id) of the
    /// oldest scrollback rows, then reclaim. The visible screen and the
    /// alt grid are never touched. Stripped cells stay `U+10EEEE` text
    /// but draw nothing, like an image evicted by quota. Returns the
    /// number of entries freed.
    pub(super) fn drop_old_placeholder_extras(&mut self, target: usize) -> usize {
        let extras = &self.extras;
        let is_placeholder =
            |id: CellExtraId| matches!(extras.get(id), Some(CellExtra::Placeholder(_)));
        let mut seen = vec![false; crate::extra::MAX_ENTRIES + 1];
        let mut count = 0usize;
        let mut note = |row: &Row, count: &mut usize| {
            for c in &row.cells {
                if let Some(id) = c.extra
                    && !seen[usize::from(id.get())]
                    && is_placeholder(id)
                {
                    seen[usize::from(id.get())] = true;
                    *count += 1;
                }
            }
        };
        for r in 0..self.primary.visible_rows() {
            note(self.primary.row(r), &mut count);
        }
        for row in self.alt.ring_rows() {
            note(row, &mut count);
        }
        // Newest scrollback row first: find the first one that takes the
        // count past `target`; it and everything older is stripped.
        let hist = self.primary.history_lines();
        let mut cut = hist;
        for n in 0..hist {
            if let Some(row) = self.primary.scrollback_row(n) {
                note(row, &mut count);
            }
            if count > target {
                cut = n;
                break;
            }
        }
        if cut == hist {
            return 0;
        }
        for n in cut..hist {
            let Some(row) = self.primary.scrollback_row_mut(n) else {
                continue;
            };
            for c in &mut row.cells {
                if c.extra.is_some_and(|id| {
                    matches!(self.extras.get(id), Some(CellExtra::Placeholder(_)))
                }) {
                    c.extra = None;
                }
            }
        }
        tracing::debug!(target: "term", rows = hist - cut, "dropped placeholder data of old scrollback");
        self.mark_all_dirty();
        self.collect_extras()
    }

    /// Drop image `id`'s virtual placements. Called whenever the image's
    /// bytes are replaced or freed (re-transmit, byte-freeing delete).
    /// Active screen only: each screen has its own store (#42), so the
    /// other screen's `i=id` is a different image.
    pub(super) fn forget_virtual_placements(&mut self, id: u32) {
        let dropped = self.images.virtual_placements.remove_image(id);
        if dropped {
            self.image_revision = self.image_revision.wrapping_add(1);
        }
    }

    /// Drop the virtual placements whose image left this screen's store
    /// (quota eviction).
    pub(super) fn prune_virtual_placements(&mut self) {
        if self.images.prune_virtual() {
            self.image_revision = self.image_revision.wrapping_add(1);
        }
    }

    /// The active screen's virtual (`U=1`) placements.
    #[must_use]
    pub fn virtual_placements(&self) -> &VirtualPlacements {
        &self.images.virtual_placements
    }

    /// True when the active screen has any virtual placement, i.e. when
    /// placeholder cells can draw anything at all. The renderer skips
    /// the per-row scan otherwise.
    #[must_use]
    pub fn has_virtual_placements(&self) -> bool {
        !self.images.virtual_placements.is_empty()
    }

    /// Append the placeholder runs of `row` (any row: live grid,
    /// scrollback, or [`Term::view_row`]'s result) to `out`.
    pub fn placeholder_runs(&self, row: &Row, out: &mut Vec<PlaceholderRun>) {
        resolve_runs(&row.cells, &self.extras, out);
    }

    /// The virtual placement `run` draws, if it exists right now.
    #[must_use]
    pub fn resolve_placeholder(&self, run: &PlaceholderRun) -> Option<&VirtualPlacement> {
        self.images.virtual_placements.find(run.image_id, run.placement_id)
    }

    /// Top-left `(row, col)` of the placeholder cells on the visible
    /// screen that draw virtual placement `(image_id, placement_id)`:
    /// the minimum row and the minimum column over all of them,
    /// independently, as the spec defines a virtual parent's position
    /// for relative placements. `None` when no cell refers to it.
    #[must_use]
    pub fn placeholder_origin(&self, image_id: u32, placement_id: u32) -> Option<(u16, u16)> {
        if !self.images.virtual_placements.contains_image(image_id) {
            return None;
        }
        let mut runs = Vec::new();
        let mut origin: Option<(u16, u16)> = None;
        for r in 0..self.rows {
            runs.clear();
            self.placeholder_runs(self.active_grid().row(r), &mut runs);
            for run in &runs {
                let Some(vp) = self.resolve_placeholder(run) else {
                    continue;
                };
                if vp.image_id != image_id || vp.placement_id != placement_id {
                    continue;
                }
                origin = Some(match origin {
                    None => (r, run.col),
                    Some((or, oc)) => (or.min(r), oc.min(run.col)),
                });
            }
        }
        origin
    }
}

#[cfg(test)]
#[path = "placeholder_tests.rs"]
mod tests;
