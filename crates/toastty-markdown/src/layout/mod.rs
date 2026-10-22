//! Markdown layout: Markdown → block tree → boxes → pixels, plus the
//! plain text and links under every row.
//!
//! - [`parse`]: pulldown-cmark events → a small block tree.
//! - `inline`: styled runs + inline rasters → shaped, wrapped lines.
//! - `blocks` (this module): block layout (headings, lists, quotes,
//!   code, tables, maths, diagrams, rules).
//! - `paint`: draw ops → tiny-skia → straight-alpha RGBA.
//! - `grid`: pixel-space text/links → per-row runs and link spans.
//!
//! Everything is laid out in pixels first, so the height is known
//! before the pixmap is allocated.
//!
//! Vertically, everything snaps to the terminal's cell grid, so the
//! plain text written into each row's cells sits on the text drawn
//! there (selection highlights and link hit areas match the image):
//!
//! - Every text line (paragraph, heading, list item, quote, code, table
//!   cell, footnote, the truncation note) starts on a row boundary and
//!   is a whole number of rows tall: one row for body text and code,
//!   more for large headings and for lines holding a tall inline
//!   raster. Its text is reported on the row its baseline falls in
//!   (for one-row lines, the line's row).
//! - Margins, gaps and padding between blocks are whole rows. Decorations
//!   (code panels, quote bars, rules, table borders and backgrounds, the
//!   H1/H2 underline) may sit at sub-row positions inside those rows.
//! - Block rasters (display maths, Mermaid) take whole rows, centred in
//!   them; an inline raster (maths, image) taller than its text grows its
//!   line by whole rows.

// Pixel geometry is small (≤ 32k px, ≤ 2^16 rows), so integer ↔ float
// casts are exact in practice; short coordinate names (x, y, w, h)
// read better than long ones in drawing code.
#![allow(
    clippy::cast_precision_loss,
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::too_many_lines
)]

mod fonts;
mod grid;
mod highlight;
mod images;
mod inline;
mod paint;
pub(crate) mod parse;
mod style;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use cosmic_text::{FontSystem, SwashCache};
use pulldown_cmark::{Alignment, BlockQuoteKind};

use crate::mermaid::PreparedDiagram;
use crate::{Error, LayoutParams, Raster, RenderedBlock, Rgba, TextRun, Theme};
use fonts::Fonts;
use grid::{Frag, LinkPx};
use highlight::{Highlighter, Tok};
use inline::{Align, Atom, Boxed, Line, LineCtx, Para, Piece, Shaped, SpanInfo};
use paint::{Img, Op, Src};
use parse::{Block, Inline, Item};
use style::Palette;

/// Widest block we rasterize.
pub(crate) const MAX_WIDTH_PX: u32 = 32_768;
/// Tallest cell we accept.
pub(crate) const MAX_CELL_H: u32 = 4_096;
/// Total decoded image bytes per render; later images show alt text.
const MAX_IMAGE_BUDGET: u64 = 128 * 1024 * 1024;
/// Maths source longer than this falls back to inline code.
const MAX_MATH_SRC: usize = 4 * 1024;
/// Text shaped per paragraph (time guard); the rest is dropped.
const MAX_PARA_TEXT: usize = 256 * 1024;
/// Table rows laid out (time guard).
const MAX_TABLE_ROWS: usize = 4_000;
/// Rasterized glyphs kept between renders.
const MAX_CACHED_GLYPHS: usize = 16_384;

pub(crate) type MathFn = fn(&str, bool, f32, Rgba) -> Result<Raster, String>;
pub(crate) type MermaidFn = fn(&str, u32, f32, &Theme) -> Result<Raster, String>;

/// How diagrams are produced: the real renderer (prepared once and
/// cached, rasterized lazily at paint) or a test stand-in called on
/// every layout.
#[derive(Clone, Copy)]
pub(crate) enum MermaidBackend {
    Real,
    #[cfg_attr(not(test), allow(dead_code))]
    Fake(MermaidFn),
}

/// Entry caps: past one, that cache is cleared (documents rarely come
/// close; this only bounds memory for pathological input).
const MAX_MEMO_ENTRIES: usize = 8_192;
const MAX_MEMO_DIAGRAMS: usize = 64;
const MAX_MEMO_IMAGE_BYTES: u64 = 256 * 1024 * 1024;

type MathKey = (String, bool, u32, Rgba);
type ImageKey = (String, bool, Option<PathBuf>);
type DiagramKey = (String, u32, Theme);
type CodeKey = (String, String, Theme);

/// Width-independent results, kept across layouts so re-laying out a
/// document at a new width (window resize) only re-wraps and re-places.
#[derive(Default)]
pub(crate) struct Memo {
    math: HashMap<MathKey, Option<Arc<Img>>>,
    images: HashMap<ImageKey, Result<Arc<Img>, images::Denied>>,
    image_bytes: u64,
    diagrams: HashMap<DiagramKey, Result<Arc<PreparedDiagram>, String>>,
    code: HashMap<CodeKey, Arc<Vec<Vec<Tok>>>>,
    shaped: HashMap<u128, Arc<Shaped>>,
}

impl Memo {
    fn trim(&mut self) {
        if self.math.len() > MAX_MEMO_ENTRIES {
            self.math.clear();
        }
        if self.images.len() > MAX_MEMO_ENTRIES || self.image_bytes > MAX_MEMO_IMAGE_BYTES {
            self.images.clear();
            self.image_bytes = 0;
        }
        if self.diagrams.len() > MAX_MEMO_DIAGRAMS {
            self.diagrams.clear();
        }
        if self.code.len() > MAX_MEMO_ENTRIES {
            self.code.clear();
        }
        if self.shaped.len() > MAX_MEMO_ENTRIES * 4 {
            self.shaped.clear();
        }
    }
}

/// Per-renderer state reused across renders.
pub(crate) struct Caches {
    /// Fonts in use: `default_mono`, or the requested family for code.
    fonts: Fonts,
    /// The monospace role picked without a requested family.
    default_mono: fonts::Role,
    /// `LayoutParams::mono_family` that `fonts.mono` was resolved for.
    mono_request: Option<String>,
    swash: SwashCache,
    hl: Highlighter,
    memo: Memo,
    scaled: paint::Scaled,
    pub(crate) math: MathFn,
    pub(crate) mermaid: MermaidBackend,
}

impl std::fmt::Debug for Caches {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Caches")
            .field("fonts", &self.fonts)
            .finish_non_exhaustive()
    }
}

impl Caches {
    pub(crate) fn new(fs: &mut FontSystem) -> Self {
        let fonts = fonts::resolve(fs);
        Self {
            default_mono: fonts.mono.clone(),
            mono_request: None,
            fonts,
            swash: SwashCache::new(),
            hl: Highlighter::default(),
            memo: Memo::default(),
            scaled: paint::Scaled::default(),
            math: crate::math::render_math,
            mermaid: MermaidBackend::Real,
        }
    }

    /// Use the family `family` for code when the font database has it,
    /// else the default monospace face. Shaped paragraphs are cached by
    /// content only, so they are dropped when the face changes.
    fn select_mono(&mut self, fs: &mut FontSystem, family: Option<&str>) {
        if self.mono_request.as_deref() == family {
            return;
        }
        self.mono_request = family.map(str::to_owned);
        let mono = family
            .and_then(|f| fonts::named(fs, f))
            .unwrap_or_else(|| self.default_mono.clone());
        if mono.family != self.fonts.mono.family || mono.regular != self.fonts.mono.regular {
            self.memo.shaped.clear();
        }
        self.fonts.mono = mono;
    }

    /// Family code is currently shaped with.
    #[cfg(test)]
    pub(crate) fn mono_family(&self) -> &str {
        &self.fonts.mono.family
    }
}

/// Inherited block context.
#[derive(Debug, Clone, Copy)]
struct Ctx {
    color: Rgba,
    depth: usize,
    tight: bool,
}

/// Base text style of a paragraph.
#[derive(Debug, Clone, Copy)]
struct Base {
    size: f32,
    bold: bool,
    italic: bool,
    color: Rgba,
}

enum Marker {
    Bullet(usize),
    Number(String),
    Task(bool),
    Label(String),
}

struct Lay<'a> {
    fs: &'a mut FontSystem,
    fonts: &'a Fonts,
    hl: &'a mut Highlighter,
    memo: &'a mut Memo,
    math: MathFn,
    mermaid: MermaidBackend,
    p: &'a LayoutParams,
    pal: Palette,
    links: &'a [String],
    ops: Vec<Op>,
    frags: Vec<Frag>,
    link_px: Vec<LinkPx>,
    image_budget: u64,
    /// Body font size.
    f: f32,
    /// Row (cell) height: the unit of all vertical layout.
    ch: f32,
    /// Display scale relative to a 16 px body font.
    scale: f32,
    /// Stop laying out blocks below this y (truncation).
    y_limit: f32,
    /// Lowest text descent drawn: a last line whose descenders hang
    /// below the block gets a row more, so they aren't cut off.
    ink_bottom: f32,
    /// Vertical offset of the next rule from its default spot (see
    /// [`Lay::blocks`]).
    rule_shift: f32,
    /// Every laid-out line, for the grid-snapping tests.
    #[cfg(test)]
    boxes: Vec<LineBox>,
    /// Current block (or table row): lines of one table row share rows.
    #[cfg(test)]
    group: usize,
}

/// A laid-out line's box and the text reported for it (tests only).
#[cfg(test)]
#[derive(Debug, Clone)]
pub(crate) struct LineBox {
    pub top: f32,
    pub h: f32,
    pub base: f32,
    pub text: String,
    /// "text", "code", "cell" (table) or "atom" (display maths, diagram).
    pub kind: &'static str,
    pub group: usize,
    /// Table column + 1 (0 outside tables).
    pub cell: usize,
}

/// A laid-out block: everything but pixels. Paint any band of it with
/// [`paint`]. Shareable across threads.
#[derive(Debug)]
pub(crate) struct Laid {
    pub rows: u16,
    pub width: u32,
    pub height: u32,
    pub cell_h: u32,
    pub text: Vec<Vec<TextRun>>,
    pub links: Vec<crate::LinkSpan>,
    ops: Vec<Op>,
    /// When truncated: rows from here down are cleared and `note` drawn.
    text_rows: u32,
    note: Vec<Op>,
    #[cfg(test)]
    pub boxes: Vec<LineBox>,
}

impl Laid {
    pub(crate) fn is_blank(&self) -> bool {
        self.ops.is_empty() && self.note.is_empty()
    }

    pub(crate) fn approx_bytes(&self) -> u64 {
        let ops = (self.ops.len() + self.note.len()) * std::mem::size_of::<Op>();
        let text: usize = self
            .text
            .iter()
            .flatten()
            .map(|r| r.text.len() + std::mem::size_of::<TextRun>())
            .sum();
        let links: usize = self.links.iter().map(|l| l.url.len() + 32).sum();
        (ops + text + links + 256) as u64
    }
}

pub(crate) fn layout(
    fs: &mut FontSystem,
    caches: &mut Caches,
    markdown: &str,
    params: &LayoutParams,
) -> Laid {
    let width = params.width_px();
    let cell_h = params.cell_h;
    // Only `max_rows` bounds the block. Blocks are painted lazily in
    // bands (only the visible GPU tiles), so the full RGBA image is never
    // held at once and a pixel-size cap would just cut wide windows short.
    let row_cap = params.max_rows.max(1);

    let doc = parse::parse(markdown);
    let f = params.font_px.clamp(4.0, 512.0);
    let ch = cell_h as f32;
    caches.select_mono(fs, params.mono_family.as_deref());
    caches.memo.trim();
    let mut lay = Lay {
        fs,
        fonts: &caches.fonts,
        hl: &mut caches.hl,
        memo: &mut caches.memo,
        math: caches.math,
        mermaid: caches.mermaid,
        p: params,
        pal: Palette::new(&params.theme),
        links: &doc.links,
        ops: vec![],
        frags: vec![],
        link_px: vec![],
        image_budget: MAX_IMAGE_BUDGET,
        f,
        ch,
        scale: f / 16.0,
        y_limit: f32::from(row_cap) * ch + ch,
        ink_bottom: 0.0,
        rule_shift: 0.0,
        #[cfg(test)]
        boxes: vec![],
        #[cfg(test)]
        group: 0,
    };
    let pad = params.cell_w as f32;
    let x0 = if width as f32 > pad * 6.0 { pad } else { 0.0 };
    let inner = (width as f32 - 2.0 * x0).max(1.0);
    let ctx = Ctx {
        color: lay.pal.fg,
        depth: 0,
        tight: false,
    };
    let end = lay.blocks(&doc.blocks, x0, inner, 0.0, ctx);
    let end = if lay.ink_bottom > end + INK_SLACK * ch {
        end + ch
    } else {
        end
    };

    let needed = ((end.ceil() as u64).div_ceil(u64::from(cell_h))).max(1);
    let truncated = needed > u64::from(row_cap);
    let rows = if truncated { row_cap } else { needed as u16 };
    let height = u32::from(rows) * cell_h;
    let text_rows = if truncated {
        usize::from(rows) - 1
    } else {
        usize::from(rows)
    };
    let mut text = grid::text_rows(&lay.frags, params.cell_w, cell_h, text_rows);
    let links = grid::link_spans(&lay.link_px, params.cols, params.cell_w, cell_h, text_rows);
    let ops = std::mem::take(&mut lay.ops);
    let mut note = vec![];
    if truncated {
        // The last row is cleared at paint time and the note drawn there.
        let base = Base {
            size: f * 0.875,
            bold: false,
            italic: true,
            color: lay.pal.muted,
        };
        let mut para = Para::default();
        para.text(
            "\u{2026} truncated",
            lay.span(base, parse::Style::default()),
        );
        let shaped = Shaped::new(lay.fs, lay.fonts, &para);
        let lctx = lay.line_ctx(base.size, false, Align::Left);
        let lines = shaped.break_lines(inner, &lctx);
        let top = text_rows as f32 * ch;
        let (texts, _) = shaped.emit(
            &lines[..lines.len().min(1)],
            x0,
            top,
            inner,
            &lctx,
            &mut note,
            lay.fonts,
        );
        let note_text: String = texts.into_iter().map(|t| t.text).collect();
        let col = (x0 / params.cell_w as f32).round() as u16;
        text.push(vec![TextRun {
            col,
            text: note_text,
        }]);
    }
    debug_assert_eq!(text.len(), usize::from(rows));
    Laid {
        rows,
        width,
        height,
        cell_h,
        text,
        links,
        ops,
        text_rows: if truncated {
            text_rows as u32
        } else {
            u32::from(rows)
        },
        note,
        #[cfg(test)]
        boxes: lay.boxes,
    }
}

/// How far (in rows) the last line's font descent may hang below the
/// block before it gets an extra row. The font box's descent is deeper
/// than real descender ink, so a small overhang only clips empty space.
const INK_SLACK: f32 = 0.1;

/// Extra rows painted (and cropped) around a band; see [`paint_band`].
const BAND_MARGIN: u32 = 8;

/// Paint block pixel rows `y0 .. y0 + h` (clamped to the block) as a
/// premultiplied pixmap. `None` when the band is empty.
pub(crate) fn paint_band(
    fs: &mut FontSystem,
    caches: &mut Caches,
    laid: &Laid,
    y0: u32,
    h: u32,
) -> Option<tiny_skia::Pixmap> {
    let y0 = y0.min(laid.height);
    let h = h.min(laid.height - y0);
    if h == 0 || laid.width == 0 {
        return None;
    }
    // Paint a margin above and below and crop it: the rasterizer treats
    // paths that cross the pixmap edge slightly differently, so without
    // it anti-aliasing at band edges (tile seams) could differ from the
    // same rows painted as part of a taller band.
    let top = y0.saturating_sub(BAND_MARGIN);
    let bottom = (y0 + h + BAND_MARGIN).min(laid.height);
    let mut pm = tiny_skia::Pixmap::new(laid.width, bottom - top)?;
    paint::execute(
        &mut pm,
        &laid.ops,
        top,
        fs,
        &mut caches.swash,
        &mut caches.scaled,
    );
    let clear_from = laid.text_rows * laid.cell_h;
    if clear_from < laid.height && bottom > clear_from {
        let from = clear_from.saturating_sub(top) as usize * laid.width as usize * 4;
        pm.data_mut()[from..].fill(0);
        paint::execute(
            &mut pm,
            &laid.note,
            top,
            fs,
            &mut caches.swash,
            &mut caches.scaled,
        );
    }
    if caches.swash.image_cache.len() > MAX_CACHED_GLYPHS {
        caches.swash.image_cache.clear();
    }
    if top == y0 && bottom == y0 + h {
        return Some(pm);
    }
    let stride = laid.width as usize * 4;
    let start = (y0 - top) as usize * stride;
    let mut out = tiny_skia::Pixmap::new(laid.width, h)?;
    out.data_mut()
        .copy_from_slice(&pm.data()[start..start + h as usize * stride]);
    pm = out;
    Some(pm)
}

/// Paint a band as a straight-alpha raster (always `width` × clamped `h`).
pub(crate) fn paint_raster(
    fs: &mut FontSystem,
    caches: &mut Caches,
    laid: &Laid,
    y0: u32,
    h: u32,
) -> Raster {
    match paint_band(fs, caches, laid, y0, h) {
        Some(pm) => paint::into_raster(pm),
        None => Raster {
            width: laid.width,
            height: 0,
            pixels: vec![],
            baseline: None,
        },
    }
}

pub(crate) fn render(
    fs: &mut FontSystem,
    caches: &mut Caches,
    markdown: &str,
    params: &LayoutParams,
) -> Result<RenderedBlock, Error> {
    let laid = layout(fs, caches, markdown, params);
    let pm = paint_band(fs, caches, &laid, 0, laid.height).ok_or(Error::Render("pixmap".into()))?;
    Ok(RenderedBlock {
        rows: laid.rows,
        image: paint::into_raster(pm),
        text: laid.text,
        links: laid.links,
    })
}

fn heading_scale(level: u8) -> f32 {
    match level {
        1 => 2.0,
        2 => 1.6,
        3 => 1.3,
        4 => 1.1,
        5 => 1.0,
        _ => 0.9,
    }
}

/// Whether the padding row at a block's top / bottom edge holds a panel
/// edge (code, table, diagram) drawn through its middle, rather than
/// blank space (rules, the H1/H2 underline row).
fn panel_edge(b: &Block, bottom: bool) -> bool {
    let (lead, trail) = space(b);
    let rows = if bottom { trail } else { lead };
    rows > 0.0 && !matches!(b, Block::Rule | Block::Heading(..))
}

/// Rows of blank space at the (top, bottom) of a block's own box: the
/// padding rows code panels, tables and diagrams draw into, the
/// underline row of H1/H2 and the rows around a rule. They count
/// towards the margin between blocks (see [`Lay::blocks`]), so a code
/// panel's padding row doubles as the blank row before it.
fn space(b: &Block) -> (f32, f32) {
    match b {
        Block::Code { .. } | Block::Mermaid(_) | Block::Table { .. } | Block::Rule => (1.0, 1.0),
        Block::Heading(l, _) if *l <= 2 => (0.0, 1.0),
        Block::List { items, .. } => (
            items
                .first()
                .and_then(|i| i.blocks.first())
                .map_or(0.0, |b| space(b).0),
            items
                .last()
                .and_then(|i| i.blocks.last())
                .map_or(0.0, |b| space(b).1),
        ),
        Block::Footnote(_, blocks) => (
            blocks.first().map_or(0.0, |b| space(b).0),
            blocks.last().map_or(0.0, |b| space(b).1),
        ),
        _ => (0.0, 0.0),
    }
}

impl Lay<'_> {
    /// Blank rows between sibling blocks (and loose list items).
    const GAP_ROWS: f32 = 1.0;

    /// (top, bottom) margins of a block, in rows.
    fn margins(b: &Block, ctx: Ctx) -> (f32, f32) {
        match b {
            // Large headings are taller than their text needs, which
            // already sets them off from the text above.
            Block::Heading(..) => (1.0, 1.0),
            Block::Paragraph(_) | Block::List { .. } if ctx.tight => (0.0, 0.0),
            _ => (Self::GAP_ROWS, Self::GAP_ROWS),
        }
    }

    /// Lay out sibling blocks from `y` (a row boundary); returns the
    /// bottom y (a row boundary). Adjacent margins collapse to the
    /// larger one, less any blank rows the blocks already have at that
    /// edge ([`space`]). A heading keeps its full top margin below a
    /// panel's (code, table, diagram) padding row, so it isn't crowded
    /// against the panel edge drawn in it.
    fn blocks(&mut self, blocks: &[Block], x: f32, w: f32, y: f32, ctx: Ctx) -> f32 {
        let mut y = y;
        let mut prev: Option<(f32, f32, &Block)> = None;
        for (i, b) in blocks.iter().enumerate() {
            if y > self.y_limit {
                break;
            }
            let (top, bottom) = Self::margins(b, ctx);
            let (lead, trail) = space(b);
            let after_panel = prev.is_some_and(|p| panel_edge(p.2, true));
            if let Some((pb, pt, _)) = prev {
                let pt = if after_panel && matches!(b, Block::Heading(..)) {
                    0.0
                } else {
                    pt
                };
                y += (pb.max(top) - pt - lead).max(0.0) * self.ch;
            }
            if matches!(b, Block::Rule) {
                // A panel edge sits half way into its padding row, so
                // nudge the rule a quarter row away from it to keep the
                // space on both sides equal.
                let before_panel = blocks.get(i + 1).is_some_and(|n| panel_edge(n, false));
                self.rule_shift = (f32::from(u8::from(before_panel))
                    - f32::from(u8::from(after_panel)))
                    * self.ch
                    / 4.0;
            }
            #[cfg(test)]
            {
                self.group += 1;
            }
            y = self.block(b, x, w, y, ctx);
            debug_assert!(y % self.ch < 1e-3, "block {b:?} ends off the row grid");
            prev = Some((bottom, trail, b));
        }
        y
    }

    fn block(&mut self, b: &Block, x: f32, w: f32, y: f32, ctx: Ctx) -> f32 {
        match b {
            Block::Heading(level, inl) => self.heading(*level, inl, x, w, y, ctx),
            Block::Paragraph(inl) => {
                let base = Base {
                    size: self.f,
                    bold: false,
                    italic: false,
                    color: ctx.color,
                };
                self.paragraph(inl, base, x, w, y, Align::Left)
            }
            Block::List {
                start,
                tight,
                items,
            } => self.list(*start, *tight, items, x, w, y, ctx),
            Block::Quote(kind, blocks) => self.quote(*kind, blocks, x, w, y, ctx),
            Block::Code { lang, text } => self.code(lang, text, x, w, y),
            Block::Math(tex) => self.display_math(tex, x, w, y, ctx),
            Block::Mermaid(src) => self.diagram(src, x, w, y),
            Block::Table { aligns, head, rows } => self.table(aligns, head, rows, x, w, y, ctx),
            Block::Rule => {
                // Two rows (see `space`), the rule on the line between them.
                let h = (self.f * 0.25).round().max(2.0);
                self.ops.push(Op::Rect {
                    x,
                    y: (y + self.ch + self.rule_shift - h / 2.0).round(),
                    w,
                    h,
                    r: 0.0,
                    color: self.pal.rule,
                });
                y + 2.0 * self.ch
            }
            Block::Html(text) => {
                let mut inl = vec![];
                for (i, l) in text.trim_end().lines().enumerate() {
                    if i > 0 {
                        inl.push(Inline::HardBreak);
                    }
                    inl.push(Inline::Text(l.to_owned(), parse::Style::default()));
                }
                let base = Base {
                    size: self.f,
                    bold: false,
                    italic: false,
                    color: ctx.color,
                };
                self.paragraph(&inl, base, x, w, y, Align::Left)
            }
            Block::Footnote(label, blocks) => {
                let c = Ctx {
                    color: self.pal.muted,
                    ..ctx
                };
                self.item(
                    &Marker::Label(label.clone()),
                    blocks,
                    x,
                    w,
                    y,
                    c,
                    (self.f * 2.0).round(),
                )
            }
        }
    }

    /// Line metrics for text of `size`. Body-sized (or smaller) text
    /// takes one row; larger text (headings) as many rows as its
    /// ascent-plus-descent box needs, rounding down only when the box
    /// overhangs the rows by less than 40% of a row (it may spill a
    /// little into the margins around it).
    fn line_ctx(&self, size: f32, mono: bool, align: Align) -> LineCtx {
        let m = if mono {
            self.fonts.mono.metrics
        } else {
            self.fonts.sans.metrics
        };
        let (asc, desc) = (m.ascent * size, m.descent * size);
        let rows = if size <= self.f + 0.01 {
            1
        } else {
            ((asc + desc) / self.ch - 0.4).ceil().max(1.0) as u32
        };
        LineCtx {
            row_h: self.ch,
            rows,
            asc,
            desc,
            preserve_ws: mono,
            align,
        }
    }

    fn span(&self, base: Base, st: parse::Style) -> SpanInfo {
        SpanInfo {
            mono: false,
            size: base.size,
            bold: base.bold || st.bold,
            italic: base.italic || st.italic,
            color: if st.link.is_some() {
                self.pal.link
            } else {
                base.color
            },
            underline: st.link.is_some(),
            strike: st.strike,
            boxed: Boxed::None,
            bg: self.pal.inline_code_bg,
            link: st.link,
            rise: 0.0,
        }
    }

    /// Fit a raster into at most `max_w` px wide at the display scale.
    fn fit((rw, rh): (u32, u32), max_w: f32, natural_scale: f32) -> (f32, f32) {
        let mut w = rw as f32 * natural_scale;
        let mut h = rh as f32 * natural_scale;
        if w > max_w && w > 0.0 {
            h *= max_w / w;
            w = max_w;
        }
        (w.max(1.0).round(), h.max(1.0).round())
    }

    /// Shape a paragraph, reusing the result of an identical one.
    fn shape(&mut self, para: &Para) -> Arc<Shaped> {
        let key = inline::para_key(para);
        if let Some(s) = self.memo.shaped.get(&key) {
            return Arc::clone(s);
        }
        let s = Arc::new(Shaped::new(self.fs, self.fonts, para));
        self.memo.shaped.insert(key, Arc::clone(&s));
        s
    }

    fn try_math(&mut self, tex: &str, display: bool, size: f32, color: Rgba) -> Option<Arc<Img>> {
        if tex.len() > MAX_MATH_SRC || tex.trim().is_empty() {
            return None;
        }
        let key = (tex.to_owned(), display, size.to_bits(), color);
        if let Some(hit) = self.memo.math.get(&key) {
            return hit.clone();
        }
        let img = match (self.math)(tex, display, size, color) {
            Ok(r)
                if r.width > 0
                    && r.height > 0
                    && r.pixels.len() >= (r.width as usize) * (r.height as usize) * 4 =>
            {
                Img::new(&r)
            }
            Ok(_) => None,
            Err(e) => {
                tracing::debug!("maths fallback: {e}");
                None
            }
        };
        self.memo.math.insert(key, img.clone());
        img
    }

    fn build_para(&mut self, inl: &[Inline], base: Base, max_w: f32) -> Para {
        let mut para = Para::default();
        let mut budget = MAX_PARA_TEXT;
        let code_span = |me: &Self, st: parse::Style| SpanInfo {
            mono: true,
            size: (base.size * 0.875).round().max(1.0),
            boxed: Boxed::Code,
            ..me.span(base, st)
        };
        for (i, it) in inl.iter().enumerate() {
            if budget == 0 {
                break;
            }
            match it {
                Inline::Text(t, st) => {
                    let t = clip(t, &mut budget);
                    para.text(t, self.span(base, *st));
                }
                Inline::Code(t, st) => {
                    let t = clip(t, &mut budget);
                    let span = SpanInfo {
                        underline: st.link.is_some(),
                        ..code_span(self, *st)
                    };
                    para.text(t, span);
                }
                Inline::Math {
                    tex,
                    display,
                    style,
                } => {
                    let color = if style.link.is_some() {
                        self.pal.link
                    } else {
                        base.color
                    };
                    let raster = self.try_math(tex, *display, base.size, color);
                    let delim = if *display { "$$" } else { "$" };
                    let src = format!("{delim}{}{delim}", tex.replace('\n', " "));
                    if let Some(r) = raster {
                        let (w, h) = Self::fit((r.width, r.height), max_w, 1.0);
                        let k = h / r.height as f32;
                        // Display atoms are centred on their own line, with
                        // the "baseline" (the row their text is reported
                        // on) through the middle.
                        let asc = match r.baseline {
                            _ if *display => (h / 2.0).round(),
                            Some(b) => (b as f32 * k).round(),
                            None => h,
                        };
                        let raster = Src::Img(r);
                        let atom = Atom {
                            w,
                            h,
                            asc,
                            raster,
                            link: style.link,
                            text: src,
                        };
                        para.pieces.push(if *display {
                            Piece::Display(atom)
                        } else {
                            Piece::Atom(atom)
                        });
                    } else {
                        if *display && i > 0 && !para.is_empty() {
                            para.pieces.push(Piece::Break);
                        }
                        para.text(&src, code_span(self, *style));
                        if *display && i + 1 < inl.len() {
                            para.pieces.push(Piece::Break);
                        }
                    }
                }
                Inline::Image { url, alt, style } => {
                    self.image(&mut para, url, alt, *style, base, max_w);
                }
                Inline::FootnoteRef(label, st) => {
                    let span = SpanInfo {
                        size: (base.size * 0.75).round(),
                        rise: (base.size * 0.35).round(),
                        color: self.pal.link,
                        ..self.span(base, *st)
                    };
                    para.text(&format!("[{label}]"), span);
                }
                Inline::SoftBreak => para.text(" ", self.span(base, parse::Style::default())),
                Inline::HardBreak => para.pieces.push(Piece::Break),
            }
        }
        para
    }

    fn image(
        &mut self,
        para: &mut Para,
        url: &str,
        alt: &str,
        st: parse::Style,
        base: Base,
        max_w: f32,
    ) {
        let key = (
            url.to_owned(),
            self.p.resources.allow_local_files,
            self.p.resources.base_dir.clone(),
        );
        let loaded = if let Some(hit) = self.memo.images.get(&key) {
            hit.clone()
        } else {
            let r = images::load(url, &self.p.resources).and_then(|r| {
                Img::new(&r).ok_or_else(|| images::Denied::Failed("empty image".into()))
            });
            if let Ok(img) = &r {
                self.memo.image_bytes += img.bytes();
            }
            self.memo.images.insert(key, r.clone());
            r
        };
        let label = if alt.trim().is_empty() {
            "image".to_owned()
        } else {
            alt.trim().to_owned()
        };
        match loaded {
            Ok(r) if r.bytes() <= self.image_budget => {
                self.image_budget -= r.bytes();
                let (w, h) = Self::fit((r.width, r.height), max_w, self.scale);
                para.pieces.push(Piece::Atom(Atom {
                    w,
                    h,
                    asc: h,
                    raster: Src::Img(r),
                    link: st.link,
                    text: label,
                }));
            }
            other => {
                if let Err(e) = other {
                    tracing::debug!("image {url:?} not shown: {e:?}");
                }
                let span = SpanInfo {
                    size: (base.size * 0.875).round(),
                    color: if st.link.is_some() {
                        self.pal.link
                    } else {
                        self.pal.muted
                    },
                    italic: false,
                    underline: false,
                    boxed: Boxed::Placeholder,
                    ..self.span(base, st)
                };
                para.text(&label, span);
            }
        }
    }

    /// Shape, wrap and draw a paragraph; returns the bottom y.
    #[allow(clippy::too_many_arguments)]
    fn paragraph(
        &mut self,
        inl: &[Inline],
        base: Base,
        x: f32,
        w: f32,
        y: f32,
        align: Align,
    ) -> f32 {
        let para = self.build_para(inl, base, w);
        let lctx = self.line_ctx(base.size, false, align);
        self.flow(&para, x, w, y, &lctx)
    }

    fn flow(&mut self, para: &Para, x: f32, w: f32, y: f32, lctx: &LineCtx) -> f32 {
        let shaped = self.shape(para);
        let lines = shaped.break_lines(w, lctx);
        self.emit(&shaped, &lines, x, y, w, lctx);
        y + inline::height(&lines)
    }

    fn emit(&mut self, shaped: &Shaped, lines: &[Line], x: f32, y: f32, w: f32, lctx: &LineCtx) {
        let (texts, links) = shaped.emit(lines, x, y, w, lctx, &mut self.ops, self.fonts);
        if let Some(last) = lines.last() {
            let top = y + inline::height(lines) - last.h;
            self.ink_bottom = self.ink_bottom.max(top + last.base + lctx.desc);
        }
        #[cfg(test)]
        self.record(
            lines,
            y,
            &texts,
            if lctx.preserve_ws { "code" } else { "text" },
            0,
        );
        self.frags.extend(texts.into_iter().map(|t| Frag {
            x: t.x,
            baseline: t.baseline,
            text: t.text,
        }));
        self.push_links(links);
    }

    #[cfg(test)]
    fn record(
        &mut self,
        lines: &[Line],
        y: f32,
        texts: &[inline::LineText],
        kind: &'static str,
        cell: usize,
    ) {
        let mut top = y;
        for l in lines {
            let base = top + l.base;
            let text = texts
                .iter()
                .filter(|t| (t.baseline - base).abs() < 0.5)
                .map(|t| t.text.as_str())
                .collect();
            self.boxes.push(LineBox {
                top,
                h: l.h,
                base,
                text,
                kind,
                group: self.group,
                cell,
            });
            top += l.h;
        }
    }

    fn push_links(&mut self, links: Vec<inline::LinkRect>) {
        for l in links {
            if let Some(url) = self.links.get(l.link) {
                self.link_px.push(LinkPx {
                    x0: l.x0,
                    x1: l.x1,
                    baseline: l.baseline,
                    url: url.clone(),
                });
            }
        }
    }

    fn heading(&mut self, level: u8, inl: &[Inline], x: f32, w: f32, y: f32, ctx: Ctx) -> f32 {
        let size = (self.f * heading_scale(level)).round();
        let color = if level == 6 {
            self.pal.muted
        } else if ctx.color == self.pal.fg {
            self.pal.heading
        } else {
            ctx.color
        };
        let base = Base {
            size,
            bold: true,
            italic: false,
            color,
        };
        let para = self.build_para(inl, base, w);
        let lctx = self.line_ctx(size, false, Align::Left);
        let mut y = self.flow(&para, x, w, y, &lctx);
        if level <= 2 {
            // The underline gets a row of its own (see `space`), drawn
            // just below the descenders.
            let th = self.scale.round().max(1.0);
            let off = (size * 0.2).round().min(self.ch - th);
            self.ops.push(Op::Rect {
                x,
                y: y + off,
                w,
                h: th,
                r: 0.0,
                color: self.pal.border,
            });
            y += self.ch;
        }
        y
    }

    #[allow(clippy::too_many_arguments)]
    fn list(
        &mut self,
        start: Option<u64>,
        tight: bool,
        items: &[Item],
        x: f32,
        w: f32,
        y: f32,
        ctx: Ctx,
    ) -> f32 {
        let n = items.len() as u64;
        let digits = start.map_or(1, |s| {
            (s.saturating_add(n.saturating_sub(1))).to_string().len()
        });
        let indent = (self.f * 1.75)
            .max(self.f * (0.6 * digits as f32 + 0.9))
            .round();
        let mut y = y;
        for (i, it) in items.iter().enumerate() {
            if y > self.y_limit {
                break;
            }
            if i > 0 && !tight {
                // A blank row, unless the previous item ends / this one
                // starts with a padding row of its own.
                let trail = items[i - 1].blocks.last().map_or(0.0, |b| space(b).1);
                let lead = it.blocks.first().map_or(0.0, |b| space(b).0);
                y += (Self::GAP_ROWS - trail - lead).max(0.0) * self.ch;
            }
            let marker = match (it.task, start) {
                (Some(done), _) => Marker::Task(done),
                (None, Some(s)) => Marker::Number(format!("{}.", s.saturating_add(i as u64))),
                (None, None) => Marker::Bullet(ctx.depth),
            };
            let c = Ctx {
                depth: ctx.depth + 1,
                tight,
                ..ctx
            };
            y = self.item(&marker, &it.blocks, x, w, y, c, indent);
        }
        y
    }

    #[allow(clippy::too_many_arguments)]
    fn item(
        &mut self,
        marker: &Marker,
        blocks: &[Block],
        x: f32,
        w: f32,
        y: f32,
        ctx: Ctx,
        indent: f32,
    ) -> f32 {
        let cx = x + indent;
        let first_frag = self.frags.len();
        let first_op = self.ops.len();
        let end = self.blocks(blocks, cx, (w - indent).max(1.0), y, ctx);
        let default_base = y + self.line_ctx(self.f, false, Align::Left).text_base();
        let b = self.frags[first_frag..]
            .iter()
            .map(|f| f.baseline)
            .fold(None, |m: Option<f32>, v| Some(m.map_or(v, |m| m.min(v))))
            .unwrap_or(default_base);
        let end = end.max(y + self.ch);
        let cw = self.p.cell_w as f32;
        let f = self.f;
        let xh = self.fonts.sans.metrics.x_height * f;
        let mut marker_ops = vec![];
        let frag = match marker {
            Marker::Bullet(depth) => {
                let cxm = cx - (f * 0.85).round();
                let cy = b - xh / 2.0;
                let r = (f * 0.16).max(2.0);
                match depth % 3 {
                    0 => marker_ops.push(Op::Circle {
                        cx: cxm,
                        cy,
                        r,
                        color: ctx.color,
                        stroke: None,
                    }),
                    1 => marker_ops.push(Op::Circle {
                        cx: cxm,
                        cy,
                        r: r - 0.5,
                        color: ctx.color,
                        stroke: Some((f * 0.08).max(1.0)),
                    }),
                    _ => {
                        let s = r * 1.7;
                        marker_ops.push(Op::Rect {
                            x: cxm - s / 2.0,
                            y: cy - s / 2.0,
                            w: s,
                            h: s,
                            r: 0.0,
                            color: ctx.color,
                        });
                    }
                }
                "- ".to_owned()
            }
            Marker::Task(done) => {
                let s = (f * 0.875).round();
                let bx = (cx - s - f * 0.4).round();
                let by = (b - xh / 2.0 - s / 2.0).round();
                let r = (s * 0.2).round();
                if *done {
                    marker_ops.push(Op::Rect {
                        x: bx,
                        y: by,
                        w: s,
                        h: s,
                        r,
                        color: self.pal.checkbox_on,
                    });
                    marker_ops.push(Op::Check {
                        x: bx,
                        y: by,
                        s,
                        color: self.pal.bg,
                    });
                } else {
                    let c = style::mix(ctx.color, self.pal.bg, 0.55);
                    let lw = (f * 0.08).max(1.0);
                    marker_ops.push(Op::Stroke {
                        x: bx + lw / 2.0,
                        y: by + lw / 2.0,
                        w: s - lw,
                        h: s - lw,
                        r,
                        width: lw,
                        color: c,
                    });
                }
                if *done {
                    "[x] ".to_owned()
                } else {
                    "[ ] ".to_owned()
                }
            }
            Marker::Number(t) | Marker::Label(t) => {
                let (txt, size, color) = match marker {
                    Marker::Label(_) => (format!("[{t}]"), (f * 0.875).round(), self.pal.muted),
                    _ => (t.clone(), f, ctx.color),
                };
                let base = Base {
                    size,
                    bold: false,
                    italic: false,
                    color,
                };
                let mut para = Para::default();
                para.text(&txt, self.span(base, parse::Style::default()));
                let shaped = self.shape(&para);
                let lctx = self.line_ctx(size, false, Align::Right);
                let lines = shaped.break_lines(f32::INFINITY, &lctx);
                let right = cx - (f * 0.4).round();
                let lw = lines.first().map_or(0.0, |l| l.w);
                let top = b - lines.first().map_or(0.0, |l| l.base);
                let _ = shaped.emit(
                    &lines,
                    right - lw,
                    top,
                    lw,
                    &lctx,
                    &mut marker_ops,
                    self.fonts,
                );
                format!("{txt} ")
            }
        };
        // Markers draw under the content (they never overlap it).
        self.ops.splice(first_op..first_op, marker_ops);
        let n = unicode_width::UnicodeWidthStr::width(frag.as_str()) as f32;
        let fx = (cx - n * cw).max(x - cw * 0.49).max(0.0);
        self.frags.insert(
            first_frag,
            Frag {
                x: fx,
                baseline: b,
                text: frag,
            },
        );
        end
    }

    fn quote(
        &mut self,
        kind: Option<BlockQuoteKind>,
        blocks: &[Block],
        x: f32,
        w: f32,
        y: f32,
        ctx: Ctx,
    ) -> f32 {
        let bar_w = (self.f * 0.25).round().max(3.0);
        let inset = bar_w + (self.f * 0.9).round();
        let (cx, cw) = (x + inset, (w - inset).max(1.0));
        let first_frag = self.frags.len();
        let mut yy = y;
        let (bar, inner) = match kind {
            Some(k) => {
                let (i, title) = match k {
                    BlockQuoteKind::Note => (0, "Note"),
                    BlockQuoteKind::Tip => (1, "Tip"),
                    BlockQuoteKind::Important => (2, "Important"),
                    BlockQuoteKind::Warning => (3, "Warning"),
                    BlockQuoteKind::Caution => (4, "Caution"),
                };
                let accent = self.pal.alerts[i];
                let base = Base {
                    size: self.f,
                    bold: true,
                    italic: false,
                    color: accent,
                };
                let inl = [Inline::Text(title.into(), parse::Style::default())];
                yy = self.paragraph(&inl, base, cx, cw, yy, Align::Left);
                (accent, ctx.color)
            }
            None => (
                self.pal.quote_bar,
                if ctx.color == self.pal.fg {
                    self.pal.muted
                } else {
                    ctx.color
                },
            ),
        };
        let c = Ctx {
            color: inner,
            tight: false,
            ..ctx
        };
        let end = self.blocks(blocks, cx, cw, yy, c).max(yy).max(y + self.ch);
        self.ops.push(Op::Rect {
            x,
            y,
            w: bar_w,
            h: end - y,
            r: 0.0,
            color: bar,
        });
        // "> " in front of every line of the quote, for copy.
        let mut bases: Vec<f32> = self.frags[first_frag..]
            .iter()
            .map(|f| f.baseline)
            .collect();
        bases.sort_by(f32::total_cmp);
        bases.dedup_by(|a, b| (*a - *b).abs() < 0.5);
        for b in bases {
            self.frags.push(Frag {
                x,
                baseline: b,
                text: "> ".into(),
            });
        }
        end
    }

    fn code(&mut self, lang: &str, text: &str, x: f32, w: f32, y: f32) -> f32 {
        let pad_x = (self.p.cell_w as f32).max((self.f * 0.75).round());
        // A padding row above and below the lines (see `space`); the
        // panel reaches `pad_y` into each.
        let pad_y = (self.ch * 0.5).round();
        // Code sits on the terminal's column grid (see `code_grid`).
        let (tx, inner) = code_grid(x, w, pad_x, self.p.cell_w);
        let first_frag = self.frags.len();
        let first_op = self.ops.len();
        let key = (lang.to_owned(), text.to_owned(), self.p.theme.clone());
        let lines = if let Some(hit) = self.memo.code.get(&key) {
            Arc::clone(hit)
        } else {
            let lines = self.hl.highlight(text, lang, &self.pal);
            let lines = Arc::new(if lines.is_empty() {
                vec![vec![]]
            } else {
                lines
            });
            self.memo.code.insert(key, Arc::clone(&lines));
            lines
        };
        let lctx = self.line_ctx(self.f, true, Align::Left);
        let mut cy = y + self.ch;
        for toks in lines.iter() {
            if cy > self.y_limit {
                break;
            }
            let mut para = Para::default();
            let mut budget = MAX_PARA_TEXT;
            for t in toks {
                let span = SpanInfo {
                    mono: true,
                    size: self.f,
                    bold: t.bold,
                    italic: t.italic,
                    color: t.color,
                    underline: false,
                    strike: false,
                    boxed: Boxed::None,
                    bg: self.pal.code_bg,
                    link: None,
                    rise: 0.0,
                };
                para.text(clip(&t.text, &mut budget), span);
            }
            para.grid = Some(self.p.cell_w);
            let shaped = self.shape(&para);
            let ls = shaped.break_lines(inner, &lctx);
            if ls.is_empty() {
                #[cfg(test)]
                self.boxes.push(LineBox {
                    top: cy,
                    h: self.ch,
                    base: cy + lctx.text_base(),
                    text: String::new(),
                    kind: "code",
                    group: self.group,
                    cell: 0,
                });
                cy += self.ch;
                continue;
            }
            self.emit(&shaped, &ls, tx, cy, inner, &lctx);
            cy += inline::height(&ls);
        }
        // Every line's text starts on a whole cell, so the grid's column
        // mapping is exact (no rounding).
        debug_assert!(
            self.frags[first_frag..]
                .iter()
                .all(|f| f.x.rem_euclid(self.p.cell_w as f32) == 0.0),
            "code text off the cell grid"
        );
        let r = (self.f * 0.375).round();
        let top = y + self.ch - pad_y;
        self.ops.insert(
            first_op,
            Op::Rect {
                x,
                y: top,
                w,
                h: cy + pad_y - top,
                r,
                color: self.pal.code_bg,
            },
        );
        cy + self.ch
    }

    fn display_math(&mut self, tex: &str, x: f32, w: f32, y: f32, ctx: Ctx) -> f32 {
        let Some(r) = self.try_math(tex, true, self.f, ctx.color) else {
            let inl = [Inline::Code(
                format!("$${}$$", tex.replace('\n', " ")),
                parse::Style::default(),
            )];
            let base = Base {
                size: self.f,
                bold: false,
                italic: false,
                color: ctx.color,
            };
            return self.paragraph(&inl, base, x, w, y, Align::Left);
        };
        // Whole rows, the raster centred in them; the source is reported
        // on the middle row.
        let (dw, dh) = Self::fit((r.width, r.height), w, 1.0);
        let ch = self.ch;
        let rows = (dh / ch).ceil().max(1.0);
        let ix = x + ((w - dw) / 2.0).floor();
        self.ops.push(Op::Image {
            src: Src::Img(r),
            x: ix,
            y: y + ((rows * ch - dh) / 2.0).floor(),
            w: dw,
            h: dh,
        });
        let text = format!("$${}$$", tex.replace('\n', " "));
        let base = y + ((rows / 2.0).floor() + 0.75) * ch;
        #[cfg(test)]
        self.boxes.push(LineBox {
            top: y,
            h: rows * ch,
            base,
            text: text.clone(),
            kind: "atom",
            group: self.group,
            cell: 0,
        });
        self.frags.push(Frag {
            x,
            baseline: base,
            text,
        });
        y + rows * ch
    }

    fn diagram(&mut self, src: &str, x: f32, w: f32, y: f32) -> f32 {
        let max_w = w.max(1.0) as u32;
        let placed: Result<(Src, (u32, u32)), String> = match self.mermaid {
            MermaidBackend::Real => {
                let key = (src.to_owned(), self.f.to_bits(), self.p.theme.clone());
                let prepared = if let Some(hit) = self.memo.diagrams.get(&key) {
                    hit.clone()
                } else {
                    let r = crate::mermaid::prepare_mermaid(src, self.f, &self.p.theme);
                    self.memo.diagrams.insert(key, r.clone());
                    r
                };
                prepared.and_then(|d| {
                    let size = d.size_for(max_w)?;
                    Ok((Src::Diagram(d, max_w), size))
                })
            }
            MermaidBackend::Fake(f) => f(src, max_w, self.f, &self.p.theme).and_then(|r| {
                if r.pixels.len() < (r.width as usize) * (r.height as usize) * 4 {
                    return Err("short raster".into());
                }
                let img = Img::new(&r).ok_or("empty raster")?;
                Ok((Src::Img(img), (r.width, r.height)))
            }),
        };
        let (src_img, size) = match placed {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!("mermaid fallback: {e}");
                return self.code("mermaid", src, x, w, y);
            }
        };
        // A padding row above and below (see `space`), then whole rows
        // with the diagram centred in them and its source lines on them.
        let (dw, dh) = Self::fit(size, w, 1.0);
        let ch = self.ch;
        let rows = ((dh / ch).ceil() as usize).max(1);
        let y = y + ch;
        let ix = x + ((w - dw) / 2.0).floor();
        self.ops.push(Op::Image {
            src: src_img,
            x: ix,
            y: y + ((rows as f32 * ch - dh) / 2.0).floor(),
            w: dw,
            h: dh,
        });
        // The diagram's rows, so tests can check the raster stays in them.
        #[cfg(test)]
        self.boxes.push(LineBox {
            top: y,
            h: rows as f32 * ch,
            base: y + ch * 0.75,
            text: String::new(),
            kind: "diagram",
            group: self.group,
            cell: 0,
        });
        let lines: Vec<&str> = src.lines().filter(|l| !l.trim().is_empty()).collect();
        for (i, l) in lines.iter().enumerate().take(rows) {
            let text = if i + 1 == rows && lines.len() > rows {
                lines[i..]
                    .iter()
                    .map(|s| s.trim())
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                (*l).to_owned()
            };
            #[cfg(test)]
            self.boxes.push(LineBox {
                top: y + ch * i as f32,
                h: ch,
                base: y + ch * (i as f32 + 0.75),
                text: text.clone(),
                kind: "atom",
                group: self.group,
                cell: 0,
            });
            self.frags.push(Frag {
                x,
                baseline: y + ch * (i as f32 + 0.75),
                text,
            });
        }
        y + (rows as f32 + 1.0) * ch
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn table(
        &mut self,
        aligns: &[Alignment],
        head: &[Vec<Inline>],
        rows: &[Vec<Vec<Inline>>],
        x: f32,
        w: f32,
        y: f32,
        ctx: Ctx,
    ) -> f32 {
        let rows = &rows[..rows.len().min(MAX_TABLE_ROWS)];
        let ncols = rows
            .iter()
            .map(Vec::len)
            .chain([head.len(), aligns.len()])
            .max()
            .unwrap_or(0);
        let ch = self.ch;
        if ncols == 0 {
            // Just the padding rows `space` promises.
            return y + 2.0 * ch;
        }
        let pad_x = (self.f * 0.75).round();
        let bw = self.scale.round().max(1.0);
        // A padding row above and below the table and one after the
        // header, each with a border line through its middle. Body rows
        // sit on consecutive rows, told apart by stripes.
        let border_at = |sep_top: f32| sep_top + ((ch - bw) / 2.0).round();
        let empty: Vec<Inline> = vec![];
        let all: Vec<(bool, &[Vec<Inline>])> = std::iter::once((true, head))
            .chain(rows.iter().map(|r| (false, r.as_slice())))
            .collect();
        // Shape every cell once.
        let mut shaped: Vec<Vec<Arc<Shaped>>> = vec![];
        for (is_head, r) in &all {
            let mut out = vec![];
            for c in 0..ncols {
                let inl = r.get(c).unwrap_or(&empty);
                let base = Base {
                    size: self.f,
                    bold: *is_head,
                    italic: false,
                    color: ctx.color,
                };
                let para = self.build_para(inl, base, w);
                out.push(self.shape(&para));
            }
            shaped.push(out);
        }
        let skip_head = head.is_empty();
        // Column widths.
        let mut mins = vec![0.0f32; ncols];
        let mut maxs = vec![0.0f32; ncols];
        for row in &shaped {
            for (c, s) in row.iter().enumerate() {
                let (mn, mx) = s.measure();
                mins[c] = mins[c].max(mn.ceil() + 2.0 * pad_x);
                maxs[c] = maxs[c].max(mx.ceil() + 2.0 * pad_x);
            }
        }
        let avail = (w - (ncols as f32 + 1.0) * bw).max(ncols as f32);
        let (sum_min, sum_max): (f32, f32) = (mins.iter().sum(), maxs.iter().sum());
        let widths: Vec<f32> = if sum_max <= avail {
            maxs.clone()
        } else if sum_min <= avail {
            let extra = avail - sum_min;
            let span = (sum_max - sum_min).max(1.0);
            mins.iter()
                .zip(&maxs)
                .map(|(mn, mx)| (mn + (mx - mn) * extra / span).floor())
                .collect()
        } else {
            mins.iter()
                .map(|mn| (avail * mn / sum_min).floor().max(1.0))
                .collect()
        };
        let table_w = widths.iter().sum::<f32>() + (ncols as f32 + 1.0) * bw;
        let lctx_of = |me: &Self, c: usize| {
            let align = match aligns.get(c) {
                Some(Alignment::Center) => Align::Center,
                Some(Alignment::Right) => Align::Right,
                _ => Align::Left,
            };
            me.line_ctx(me.f, false, align)
        };
        let first_op = self.ops.len();
        let mut bg_ops = vec![];
        // y of every horizontal border line, top to bottom.
        let mut borders = vec![border_at(y)];
        let mut ry = y + ch;
        let mut body_i = 0usize;
        for (ri, row) in shaped.iter().enumerate() {
            if ri == 0 && skip_head {
                continue;
            }
            if ry > self.y_limit {
                break;
            }
            #[cfg(test)]
            {
                self.group += 1;
            }
            let lines: Vec<Vec<Line>> = row
                .iter()
                .enumerate()
                .map(|(c, s)| s.break_lines((widths[c] - 2.0 * pad_x).max(1.0), &lctx_of(self, c)))
                .collect();
            let rh = lines.iter().map(|l| inline::height(l)).fold(ch, f32::max);
            let last = ri + 1 == shaped.len();
            // The first row's background starts at the border above it,
            // the last one's ends at the border below.
            let first = ri == 0 || body_i == 0;
            let bg_top = if first {
                borders[borders.len() - 1] + bw
            } else {
                ry
            };
            let bg_bottom = if last || ri == 0 {
                border_at(ry + rh)
            } else {
                ry + rh
            };
            let bg = if ri == 0 {
                Some(self.pal.table_head_bg)
            } else {
                body_i += 1;
                body_i.is_multiple_of(2).then_some(self.pal.table_stripe_bg)
            };
            if let Some(color) = bg {
                bg_ops.push(Op::Rect {
                    x,
                    y: bg_top,
                    w: table_w,
                    h: bg_bottom - bg_top,
                    r: 0.0,
                    color,
                });
            }
            let mut cx = x + bw;
            let mut per_line: Vec<(f32, Vec<String>)> = vec![];
            for (c, s) in row.iter().enumerate() {
                let lctx = lctx_of(self, c);
                let cw = (widths[c] - 2.0 * pad_x).max(1.0);
                let (texts, links) = s.emit(
                    &lines[c],
                    cx + pad_x,
                    ry,
                    cw,
                    &lctx,
                    &mut self.ops,
                    self.fonts,
                );
                #[cfg(test)]
                self.record(&lines[c], ry, &texts, "cell", c + 1);
                self.push_links(links);
                // Line k of every cell shares a baseline (top-aligned).
                let mut top = ry;
                for (k, l) in lines[c].iter().enumerate() {
                    if per_line.len() <= k {
                        per_line.push((top + l.base, vec![String::new(); ncols]));
                    }
                    top += l.h;
                }
                for t in texts {
                    if let Some(slot) = per_line
                        .iter_mut()
                        .find(|(b, _)| (b - t.baseline).abs() < 0.5)
                    {
                        slot.1[c] = t.text;
                    } else {
                        let mut v = vec![String::new(); ncols];
                        v[c] = t.text;
                        per_line.push((t.baseline, v));
                    }
                }
                cx += widths[c] + bw;
            }
            for (b, cells) in per_line {
                let text = cells.join(" | ");
                let text = text.trim_end().to_owned();
                if !text.trim().is_empty() {
                    self.frags.push(Frag {
                        x: x + bw + pad_x,
                        baseline: b,
                        text,
                    });
                }
            }
            if ri == 0 && !last {
                borders.push(border_at(ry + rh));
                ry += rh + ch;
            } else {
                ry += rh;
            }
        }
        borders.push(border_at(ry));
        let end = ry + ch;
        // Borders: outer frame plus inner lines.
        let bc = self.pal.border;
        for &by in &borders {
            bg_ops.push(Op::Rect {
                x,
                y: by,
                w: table_w,
                h: bw,
                r: 0.0,
                color: bc,
            });
        }
        let (frame_top, frame_bottom) = (borders[0], borders[borders.len() - 1] + bw);
        let mut cx = x;
        for cw in widths.iter().map(Some).chain([None]) {
            bg_ops.push(Op::Rect {
                x: cx,
                y: frame_top,
                w: bw,
                h: frame_bottom - frame_top,
                r: 0.0,
                color: bc,
            });
            if let Some(cw) = cw {
                cx += cw + bw;
            }
        }
        self.ops.splice(first_op..first_op, bg_ops);
        end
    }
}

/// Where a code block's text goes on the terminal's column grid, for a
/// panel at `x` that is `w` wide with horizontal padding `pad_x`:
/// `(text x, text width)`. The text starts on the cell boundary nearest
/// `x + pad_x` (at least half a cell inside the panel) and is a whole
/// number of cells wide (at least one), so with cell-snapped advances
/// every grapheme cluster, including on wrapped continuation lines,
/// lands exactly on a column and lines wrap at a whole cell.
fn code_grid(x: f32, w: f32, pad_x: f32, cell_w: u32) -> (f32, f32) {
    let cw = cell_w.max(1) as f32;
    let col = ((x + pad_x) / cw).round().max(((x + cw * 0.5) / cw).ceil());
    let tx = col * cw;
    let cells = ((x + w - pad_x - tx) / cw).floor().max(1.0);
    (tx, cells * cw)
}

/// Take up to `budget` bytes of `t` (on a char boundary).
fn clip<'t>(t: &'t str, budget: &mut usize) -> &'t str {
    if t.len() <= *budget {
        *budget -= t.len();
        return t;
    }
    let mut end = *budget;
    while !t.is_char_boundary(end) {
        end -= 1;
    }
    *budget = 0;
    &t[..end]
}
