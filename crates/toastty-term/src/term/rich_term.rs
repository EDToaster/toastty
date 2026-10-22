//! `Term` side of rich blocks (OSC 7700): protocol handling, writing the
//! tagged rows, re-layout on reflow / zoom / palette change, and GC.
//! Types and the store live in [`crate::rich`].

use std::collections::HashSet;
use std::sync::Arc;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::{RichLayoutSlot, Term};
use crate::cell::{Cell, HyperlinkId};
use crate::extra::{CellExtra, ExtraTable, MAX_GRAPHEME_EXTRA_BYTES};
use crate::grid::Row;
use crate::rich::{
    RICH_MAX_ROWS, RichBlockId, RichBlockLayout, RichBlockStore, RichEnv, RichLayout,
    RichLayoutRequest, RichPalette, RichParams, RichRow, parse_osc_7700,
};

/// Intern `url` into an OSC 8 hyperlink table (shared with
/// [`Term::intern_hyperlink`]). `None` when the table is full.
pub(super) fn intern_url(table: &mut Vec<String>, url: &str) -> Option<HyperlinkId> {
    if let Some(pos) = table.iter().position(|u| u == url) {
        return u16::try_from(pos + 1).ok().and_then(HyperlinkId::new);
    }
    if table.len() >= (u16::MAX as usize) {
        return None;
    }
    table.push(url.to_string());
    u16::try_from(table.len()).ok().and_then(HyperlinkId::new)
}

/// Cells a grapheme cluster of block text takes: its Unicode width
/// capped at 2 (so `❤️` and ZWJ emoji sequences are one 2-cell cluster,
/// as under DECSET 2027), and 0 for controls and zero-width clusters,
/// which are dropped. The layout engine places the block's glyphs by the
/// same rule (`grapheme_cells` in toastty-markdown `layout/grid.rs`), so
/// the cells line up with the image. Both sides test the same case table
/// (`block_cluster_cells_matches_the_layout_engine` here,
/// `grapheme_cell_widths` there); change them together.
fn block_cluster_cells(g: &str) -> usize {
    if g.starts_with(char::is_control) {
        return 0;
    }
    UnicodeWidthStr::width(g).min(2)
}

/// The longest prefix of `s` at most `max` bytes long that ends on a
/// char boundary.
fn clip_bytes(s: &str, max: usize) -> &str {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Build the grid rows for a laid-out block: blank rows tagged
/// `RichRow { block, index }`, the text runs written into the cells one
/// grapheme cluster per cell (clipped to `cols`; wide clusters as
/// primary + continuation; a cluster's trailing codepoints interned into
/// `extras` like printed graphemes) and links stamped as hyperlink ids.
pub(crate) fn build_block_rows(
    out: &RichLayout,
    block: RichBlockId,
    cols: u16,
    links: &mut Vec<String>,
    extras: &mut ExtraTable,
) -> Vec<Row> {
    let width = cols as usize;
    let mut rows: Vec<Row> = (0..out.rows)
        .map(|index| {
            let mut r = Row::blank(cols);
            r.rich = Some(RichRow { block, index });
            r
        })
        .collect();
    for (row, runs) in rows.iter_mut().zip(&out.text) {
        for run in runs {
            let mut col = run.col as usize;
            for g in run.text.graphemes(true) {
                let w = block_cluster_cells(g);
                if w == 0 {
                    // Controls and zero-width clusters take no cell.
                    continue;
                }
                let mut chars = g.chars();
                let Some(ch) = chars.next() else {
                    continue;
                };
                if col + w > width {
                    break;
                }
                let tail = chars.as_str();
                let extra = if tail.is_empty() {
                    None
                } else {
                    extras.intern(CellExtra::Grapheme(
                        clip_bytes(tail, MAX_GRAPHEME_EXTRA_BYTES).into(),
                    ))
                };
                // Don't leave half of a wide char behind.
                if row.cells[col].is_continuation && col > 0 {
                    row.cells[col - 1] = Cell::BLANK;
                }
                if w == 1 && row.cells.get(col + 1).is_some_and(|c| c.is_continuation) {
                    row.cells[col + 1] = Cell::BLANK;
                }
                row.cells[col] = Cell {
                    ch,
                    extra,
                    ..Cell::BLANK
                };
                if w == 2 {
                    row.cells[col + 1] = Cell {
                        ch: '\0',
                        is_continuation: true,
                        ..Cell::BLANK
                    };
                }
                col += w;
            }
        }
    }
    for link in &out.links {
        let Some(row) = rows.get_mut(link.row as usize) else {
            continue;
        };
        if link.url.is_empty() {
            continue;
        }
        let Some(id) = intern_url(links, &link.url) else {
            continue;
        };
        let end = (link.cols.end as usize).min(width);
        for c in (link.cols.start as usize).min(end)..end {
            row.cells[c].hyperlink_id = Some(id);
        }
    }
    rows
}

/// Run the layout for `source` with `params`; clamps the row count to
/// `max_rows` and drops empty layouts.
fn run_layout(
    layout: &mut dyn RichBlockLayout,
    kind: &str,
    source: &str,
    base_dir: Option<&std::path::Path>,
    params: &RichParams,
) -> Option<RichLayout> {
    let req = RichLayoutRequest {
        kind,
        source,
        cols: params.cols,
        cell_w: params.cell_w,
        cell_h: params.cell_h,
        font_px: params.font_px,
        mono_family: params.mono_family.as_deref(),
        palette: params.palette,
        allow_local_files: params.allow_local_files,
        base_dir: if params.allow_local_files {
            base_dir
        } else {
            None
        },
        max_rows: RICH_MAX_ROWS,
    };
    let mut out = layout.layout(&req)?;
    out.rows = out.rows.min(RICH_MAX_ROWS);
    if out.rows == 0 {
        return None;
    }
    out.text.truncate(out.rows as usize);
    // The image must cover exactly the block's rows.
    let want_h = u32::from(out.rows) * params.cell_h;
    if let Some(img) = out.image.as_mut() {
        if img.width == 0 {
            out.image = None;
        } else if img.height != want_h {
            *img = img.with_height(want_h);
        }
    }
    Some(out)
}

impl Term {
    /// Install the rich-block layout engine (the binary passes an
    /// adapter over `toastty_markdown::Renderer`). Without one, OSC 7700
    /// is ignored.
    pub fn set_rich_layout(&mut self, layout: Box<dyn RichBlockLayout>) {
        self.rich_layout = Some(RichLayoutSlot(layout));
    }

    /// Whether a layout engine is installed.
    #[must_use]
    pub fn has_rich_layout(&self) -> bool {
        self.rich_layout.is_some()
    }

    /// Font size, font family and theme palette used for rich blocks. A change marks
    /// blocks for re-layout on the next [`Term::sync_rich_blocks`].
    pub fn set_rich_env(&mut self, env: RichEnv) {
        if env != self.rich_env {
            self.rich_env = env;
            self.rich_dirty = true;
        }
    }

    #[must_use]
    pub fn rich_env(&self) -> RichEnv {
        self.rich_env.clone()
    }

    /// Stored rich blocks (images, sources). The renderer queries
    /// [`RichBlockStore::image`] per tagged row and re-uploads when
    /// [`RichBlockStore::block_revision`] changes.
    #[must_use]
    pub fn rich_blocks(&self) -> &RichBlockStore {
        &self.rich_store
    }

    /// Store-wide revision (any block added, removed or re-imaged).
    #[must_use]
    pub fn rich_revision(&self) -> u32 {
        self.rich_store.revision()
    }

    /// Byte budget for block images (default 256 MiB). May evict.
    pub fn set_rich_budget(&mut self, bytes: u64) {
        let before = self.rich_store.revision();
        self.rich_store.set_budget(bytes);
        if before != self.rich_store.revision() {
            self.mark_all_dirty();
        }
    }

    /// Source of the block, for "Copy Markdown".
    #[must_use]
    pub fn rich_block_source(&self, block: RichBlockId) -> Option<&str> {
        self.rich_store.source(block)
    }

    /// First and last viewport row (as indexed by [`Term::view_row`])
    /// showing `block` (the extra partial top row counts while the view
    /// has a sub-row pixel offset).
    #[must_use]
    pub fn rich_block_view_span(&self, block: RichBlockId) -> Option<(u16, u16)> {
        let mut span: Option<(u16, u16)> = None;
        let extra = u16::from(!self.alt_active && self.viewport.current_pixel > 0.0);
        for r in 0..self.rows + extra {
            if self.view_row(r).rich.is_some_and(|t| t.block == block) {
                span = Some(span.map_or((r, r), |(a, _)| (a, r)));
            }
        }
        span
    }

    /// Effective palette: the theme with OSC 4 overrides for 0–15.
    fn rich_palette(&self) -> RichPalette {
        let mut p = self.rich_env.palette;
        for (i, slot) in p.ansi.iter_mut().enumerate() {
            if let Some(rgb) = self.palette_overrides[i] {
                *slot = rgb;
            }
        }
        p
    }

    /// Parameters a block laid out now at `cols` would get.
    fn rich_params(&self, cols: u16) -> RichParams {
        RichParams {
            cols,
            cell_w: u32::from(self.cell_pixel_size.0),
            cell_h: u32::from(self.cell_pixel_size.1),
            font_px: self.rich_env.font_px,
            mono_family: self.rich_env.mono_family.clone(),
            palette: self.rich_palette(),
            allow_local_files: self.security.markdown_local_files,
        }
    }

    /// `OSC 7700 ; md ; <keys> ; <base64> ST`.
    pub(super) fn osc_rich_block(&mut self, params: &[&[u8]]) {
        if !self.security.markdown || self.alt_active || self.rich_layout.is_none() {
            return;
        }
        let Some(osc) = parse_osc_7700(params) else {
            tracing::debug!("osc 7700: malformed sequence ignored");
            return;
        };
        let rp = self.rich_params(self.cols);
        let id = self.rich_store.alloc_id();
        let Some(slot) = self.rich_layout.as_mut() else {
            return;
        };
        let Some(out) = run_layout(
            slot.0.as_mut(),
            &osc.kind,
            &osc.source,
            osc.cwd.as_deref(),
            &rp,
        ) else {
            return;
        };
        let rows = build_block_rows(&out, id, self.cols, &mut self.hyperlinks, &mut self.extras);
        // The block starts at column 0 of a fresh line.
        if self.cursor.col != 0 {
            let r = self.cursor.row;
            let c = self.cursor.col.min(self.cols.saturating_sub(1));
            self.mark_cell(r, c);
            self.cursor.col = 0;
            self.linefeed();
        }
        self.rich_store.insert(
            id,
            Arc::from(osc.kind.as_str()),
            Arc::from(osc.source.as_str()),
            osc.cwd,
            rp,
            out.rows,
            out.image,
        );
        for row in rows {
            let r = self.cursor.row;
            *self.primary.row_mut(r) = row;
            self.mark_row(r);
            self.linefeed();
        }
        self.cursor.col = 0;
        self.mark_cell(self.cursor.row, 0);
        // Older blocks may have been overwritten / scrolled out.
        self.gc_rich_blocks();
    }

    /// Reflow the primary grid, re-laying out every intact block whose
    /// geometry differs from the current parameters. Returns the new
    /// cursor and how the retained rows were renumbered.
    pub(super) fn reflow_primary(
        &mut self,
        rows: u16,
        cols: u16,
        cap: usize,
        cursor: (u16, u16),
    ) -> ((u16, u16), crate::grid::LineRemap) {
        let params = self.rich_params(cols);
        let store = &mut self.rich_store;
        let layout = &mut self.rich_layout;
        let links = &mut self.hyperlinks;
        let extras = &mut self.extras;
        let mut relayout = |block: RichBlockId, run: &[Row]| -> Option<Vec<Row>> {
            let b = store.get(block)?;
            if b.frozen {
                return None;
            }
            let intact = run.len() == b.rows as usize
                && run
                    .iter()
                    .enumerate()
                    .all(|(i, r)| r.rich.is_some_and(|t| t.index as usize == i));
            if !intact {
                // Partly overwritten or partly evicted: keep what's left
                // as plain rows; never re-lay it out.
                store.get_mut(block)?.frozen = true;
                return None;
            }
            if b.params.same_geometry(&params)
                && b.params.allow_local_files == params.allow_local_files
            {
                return None;
            }
            let (kind, source, base) = (b.kind.clone(), b.source.clone(), b.base_dir.clone());
            let slot = layout.as_mut()?;
            let out = run_layout(slot.0.as_mut(), &kind, &source, base.as_deref(), &params)?;
            let new_rows = build_block_rows(&out, block, cols, links, extras);
            let bm = store.get_mut(block)?;
            bm.params = params.clone();
            bm.rows = out.rows;
            store.set_image(block, out.image);
            Some(new_rows)
        };
        self.primary
            .resize_reflow_mapped(rows, cols, cap, cursor, &mut relayout)
    }

    /// Free blocks with no tagged rows left in the primary grid or its
    /// scrollback.
    pub(super) fn gc_rich_blocks(&mut self) {
        if self.rich_store.is_empty() {
            return;
        }
        let mut live: HashSet<RichBlockId> = HashSet::new();
        for n in 0..self.primary.history_lines() {
            if let Some(t) = self.primary.scrollback_row(n).and_then(|r| r.rich) {
                live.insert(t.block);
            }
        }
        for i in 0..self.primary.visible_rows() {
            if let Some(t) = self.primary.row(i).rich {
                live.insert(t.block);
            }
        }
        if self.rich_store.retain_live(&live) > 0 {
            self.mark_all_dirty();
        }
    }

    /// Apply pending re-layouts and GC. Call once per frame before
    /// rendering (cheap when nothing changed). Geometry changes (cell
    /// pixel size, font size) re-lay blocks out and replace their rows;
    /// palette-only changes re-render the image with the same row
    /// count. Returns `true` when anything changed.
    pub fn sync_rich_blocks(&mut self) -> bool {
        let rev = self.rich_store.revision();
        if self.rich_dirty && !self.rich_store.is_empty() {
            self.rich_dirty = false;
            let params = self.rich_params(self.cols);
            let mut need_relayout = false;
            let mut recolor: Vec<RichBlockId> = Vec::new();
            for id in self.rich_store.ids() {
                let b = self.rich_store.get(id).expect("listed id");
                if b.frozen {
                    continue;
                }
                if !b.params.same_geometry(&params)
                    || b.params.allow_local_files != params.allow_local_files
                {
                    need_relayout = true;
                } else if b.params.palette != params.palette {
                    recolor.push(id);
                }
            }
            if need_relayout {
                self.relayout_rich_in_place();
            }
            for id in recolor {
                self.recolor_block(id, &params);
            }
        } else {
            self.rich_dirty = false;
        }
        self.gc_rich_blocks();
        let changed = rev != self.rich_store.revision();
        if changed {
            self.mark_all_dirty();
        }
        changed
    }

    /// Re-render `id` with new colours, keeping its row count.
    fn recolor_block(&mut self, id: RichBlockId, params: &RichParams) {
        let Some(b) = self.rich_store.get(id) else {
            return;
        };
        let (kind, source, base, rows) =
            (b.kind.clone(), b.source.clone(), b.base_dir.clone(), b.rows);
        let Some(slot) = self.rich_layout.as_mut() else {
            return;
        };
        let Some(out) = run_layout(slot.0.as_mut(), &kind, &source, base.as_deref(), params) else {
            return;
        };
        let image = out
            .image
            .map(|img| img.with_height(u32::from(rows) * params.cell_h));
        if let Some(b) = self.rich_store.get_mut(id) {
            b.params = params.clone();
        }
        self.rich_store.set_image(id, image);
    }

    /// Re-lay out blocks at the current size (font / DPI change) without
    /// the other side effects of [`Term::resize`].
    fn relayout_rich_in_place(&mut self) {
        let cap = self.rows as usize + self.scrollback as usize;
        let cursor = if self.alt_active {
            (self.saved_cursor.row, self.saved_cursor.col)
        } else {
            (self.cursor.row, self.cursor.col)
        };
        let ((r, c), remap) = self.reflow_primary(self.rows, self.cols, cap, cursor);
        if self.remap_primary_images(&remap) {
            self.image_revision = self.image_revision.wrapping_add(1);
        }
        if self.alt_active {
            self.saved_cursor.row = r;
            self.saved_cursor.col = c;
        } else {
            self.cursor.row = r;
            self.cursor.col = c;
            self.clamp_cursor();
        }
        self.viewport.clamp_to(self.primary.history_lines());
        self.drop_selection();
        self.mark_all_dirty();
    }
}

#[cfg(test)]
#[path = "rich_tests.rs"]
mod tests;
