//! Per-row shaping for `Renderer::render_term`: which rendered rows to
//! (re-)shape this frame, the text fed to the shaper for each, and the
//! row-shape cache the instance builders read glyph slots from.
//!
//! Lives outside `lib.rs` so the `render_term` criterion bench drives the
//! exact code the renderer runs.

use std::sync::Arc;

use toastty_term::Term;
use wgpu::Queue;

use crate::text::glyph_rasterizer::{GlyphRasterizer, LineGlyphs};
use crate::text::instance::{GlyphSlot, row_has_blink};

/// Row-shape cache plus the per-frame shaping loop.
#[derive(Debug, Default)]
pub struct RowShaper {
    /// `line_cache[r]` holds the shaped glyphs for rendered row `r`;
    /// `None` slots are re-shaped on the next frame. Shared with the
    /// rasterizer's row-text memo (see
    /// [`GlyphRasterizer::shape_line_memo`]), so a row costs one
    /// allocation however many places hold it.
    ///
    /// This is the minimum-viable subset of decision #7 / M9 needed to
    /// kill the per-keystroke render cost — shape only dirty rows, reuse
    /// cached glyphs for the rest.
    line_cache: Vec<Option<Arc<LineGlyphs>>>,
    /// `row_blink[r]` is true when rendered row `r` holds a visible
    /// SGR-blink cell. Parallel to `line_cache` and refreshed for the
    /// same (dirty) rows, so finding blinking text costs nothing on
    /// clean frames. Drives the renderer's text-blink timer.
    row_blink: Vec<bool>,
    /// Reusable per-row text buffer, cleared and refilled for each dirty
    /// row instead of allocating a fresh `String` (which showed up in
    /// profiles as per-row alloc churn).
    text: String,
}

impl RowShaper {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop every cached row (font or size change: the slots point into
    /// an atlas that no longer exists).
    pub fn invalidate(&mut self) {
        self.line_cache.clear();
    }

    /// Re-shape the dirty rows of the `rows_rendered` rows drawn this
    /// frame (and any row with no cached shape); clean rows keep their
    /// cached glyphs. The atlas never shrinks, so a clean row's slots
    /// stay valid across frames. Returns how many rows were shaped.
    ///
    /// `suppress[r]` marks rendered rows showing a rich-block image
    /// slice: those draw the image instead of their text glyphs (the
    /// cells keep the text for selection / copy / the evicted fallback),
    /// so they shape as blanks.
    pub fn shape_rows(
        &mut self,
        rasterizer: &mut GlyphRasterizer,
        queue: &Queue,
        term: &Term,
        rows_rendered: u16,
        suppress: &[bool],
    ) -> u32 {
        // Two extra slots beyond `rows`: one for the partial *top* row
        // during sub-pixel scrolling, one for the partial *bottom* row
        // when the content area isn't a whole number of cells tall.
        // Sizing for the worst case avoids reallocation churn as the
        // scroll offset crosses cell boundaries. Growth is dirty (new
        // entries are `None`); shrinking just drops old slots.
        let cache_len = usize::from(term.size().0) + 2;
        if self.line_cache.len() != cache_len {
            self.line_cache.resize(cache_len, None);
        }
        self.row_blink.resize(cache_len, false);

        let damage = term.damage();
        let mode_2027 = term.grapheme_cluster_mode();
        let mut shaped = 0u32;
        for r in 0..rows_rendered {
            let ri = usize::from(r);
            let is_dirty = damage.all
                || damage.rows.get(ri).is_some_and(|rd| !rd.is_empty())
                || self.line_cache[ri].is_none();
            if !is_dirty {
                continue;
            }
            let row = term.view_row(r);
            self.row_blink[ri] = fill_row_text(
                &mut self.text,
                term,
                row,
                suppress.get(ri).copied().unwrap_or(false),
            );
            self.line_cache[ri] = Some(rasterizer.shape_line_memo(queue, &self.text, mode_2027));
            shaped += 1;
        }
        // Keep about a screenful of recently seen rows beyond the ones on
        // screen: enough to scroll back and forth without re-shaping.
        rasterizer.trim_line_memo(usize::from(rows_rendered));
        shaped
    }

    /// Glyph slot for `(row, col)` holding `ch`, from the cached shapes.
    #[inline]
    #[must_use]
    pub fn glyph(&self, row: u16, col: u16, ch: char) -> Option<GlyphSlot> {
        self.line_cache
            .get(usize::from(row))?
            .as_ref()?
            .get(col, ch)
    }

    /// Per-rendered-row "has visible blinking text" flags.
    #[must_use]
    pub fn row_blink(&self) -> &[bool] {
        &self.row_blink
    }
}

/// Fill `out` with the text the shaper sees for `row`, and return
/// whether the row holds visible blinking text (`row_has_blink`, folded
/// into the same pass over the cells).
///
/// Continuation cells are excluded: they're the second half of a width-2
/// cluster whose primary cell already contributes its full multi-cell
/// glyph to the shaper. Feeding the continuation in as a space would
/// insert an extra glyph cosmic-text would shape, shifting every
/// downstream cluster's snapped column by one.
fn fill_row_text(out: &mut String, term: &Term, row: &toastty_term::Row, suppress: bool) -> bool {
    out.clear();
    let mut blink = false;
    for c in row.cells() {
        blink |= c.style.flags.blink() && !c.style.flags.hidden();
        if c.is_continuation {
            continue;
        }
        if suppress {
            out.push(' ');
            continue;
        }
        // Replace Kitty Unicode placeholder cells (U+10EEEE) with a space
        // before shaping. The codepoint shapes to `.notdef` and its
        // cluster width can throw off the snap of neighboring chars — we
        // don't want it in the glyph cache and the image pipeline draws
        // the real pixels anyway.
        let ch = if c.ch == '\0' || c.ch == toastty_term::PLACEHOLDER {
            ' '
        } else {
            c.ch
        };
        out.push(ch);
        // The rest of a multi-codepoint grapheme (combining marks, ZWJ
        // tail, VS16) so it shapes as one cluster; its extra glyphs come
        // back as secondary glyphs of this cell (see
        // `instance::secondary_glyph_key`).
        if c.extra.is_some() && ch != ' ' {
            out.push_str(term.grapheme_tail(c));
        }
    }
    debug_assert_eq!(blink, row_has_blink(row));
    blink
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::glyph_rasterizer::DEFAULT_LINE_HEIGHT_RATIO;
    use toastty_parser::Parser;

    const TEST_FONT: &[u8] = include_bytes!("../../fonts/FiraMono-Medium.ttf");

    fn setup() -> (Queue, GlyphRasterizer, Term) {
        let (device, queue) = crate::test_gpu::device_and_queue();
        let rasterizer = GlyphRasterizer::new(
            &device,
            16.0,
            DEFAULT_LINE_HEIGHT_RATIO,
            Some("Fira Mono"),
            Some(TEST_FONT),
        );
        let mut term = Term::new(3, 12, 0);
        Parser::new().advance(&mut term, "ab 你好\r\ncd\r\n\x1b[5mblink".as_bytes());
        (queue, rasterizer, term)
    }

    /// Only dirty (or never-shaped) rows are shaped; clean rows keep
    /// their cached glyphs.
    #[test]
    fn shapes_only_dirty_rows() {
        let (queue, mut rasterizer, mut term) = setup();
        let mut rows = RowShaper::new();
        assert_eq!(rows.shape_rows(&mut rasterizer, &queue, &term, 3, &[]), 3);
        assert!(rows.glyph(0, 0, 'a').is_some());
        assert!(
            rows.glyph(0, 3, '你').is_some(),
            "wide char keyed at its column"
        );
        assert_eq!(rows.row_blink()[..3], [false, false, true]);
        term.clear_damage();
        assert_eq!(rows.shape_rows(&mut rasterizer, &queue, &term, 3, &[]), 0);
        term.mark_cell_dirty(1, 0);
        assert_eq!(rows.shape_rows(&mut rasterizer, &queue, &term, 3, &[]), 1);
        assert!(rows.glyph(1, 1, 'd').is_some());
        term.clear_damage();
        rows.invalidate();
        assert_eq!(rows.shape_rows(&mut rasterizer, &queue, &term, 3, &[]), 3);
    }

    /// A rich-slice row shapes as blanks, so its text draws no glyphs.
    #[test]
    fn suppressed_rows_shape_blank() {
        let (queue, mut rasterizer, term) = setup();
        let mut rows = RowShaper::new();
        rows.shape_rows(&mut rasterizer, &queue, &term, 3, &[true, false]);
        assert!(rows.glyph(0, 0, 'a').is_none());
        assert!(rows.glyph(1, 0, 'c').is_some());
    }
}
