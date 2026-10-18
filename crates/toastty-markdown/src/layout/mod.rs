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
//! before the pixmap is allocated; the height is then snapped up to a
//! whole number of rows. Every line pitch is at least one cell tall, so
//! no two lines' baselines share a row and each row's text reads in
//! order.

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

use cosmic_text::{FontSystem, SwashCache};
use pulldown_cmark::{Alignment, BlockQuoteKind};

use crate::{Error, LayoutParams, Raster, RenderedBlock, Rgba, TextRun, Theme};
use fonts::Fonts;
use grid::{Frag, LinkPx};
use highlight::Highlighter;
use inline::{Align, Atom, Boxed, Line, LineCtx, Para, Piece, Shaped, SpanInfo};
use paint::Op;
use parse::{Block, Inline, Item};
use style::Palette;

/// Widest block we rasterize.
pub(crate) const MAX_WIDTH_PX: u32 = 32_768;
/// Tallest cell we accept.
pub(crate) const MAX_CELL_H: u32 = 4_096;
/// Cap on the block's RGBA size; past it the block is truncated like
/// `max_rows`.
const MAX_BLOCK_BYTES: u64 = 96 * 1024 * 1024;
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

/// Per-renderer state reused across renders.
pub(crate) struct Caches {
    fonts: Fonts,
    swash: SwashCache,
    hl: Highlighter,
    pub(crate) math: MathFn,
    pub(crate) mermaid: MermaidFn,
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
        Self {
            fonts: fonts::resolve(fs),
            swash: SwashCache::new(),
            hl: Highlighter::default(),
            math: crate::math::render_math,
            mermaid: crate::mermaid::render_mermaid,
        }
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
    math: MathFn,
    mermaid: MermaidFn,
    p: &'a LayoutParams,
    pal: Palette,
    links: &'a [String],
    ops: Vec<Op>,
    frags: Vec<Frag>,
    link_px: Vec<LinkPx>,
    rasters: Vec<Raster>,
    image_budget: u64,
    /// Body font size and line pitch.
    f: f32,
    line_h: f32,
    /// Display scale relative to a 16 px body font.
    scale: f32,
    /// Stop laying out blocks below this y (truncation).
    y_limit: f32,
}

pub(crate) fn render(
    fs: &mut FontSystem,
    caches: &mut Caches,
    markdown: &str,
    params: &LayoutParams,
) -> Result<RenderedBlock, Error> {
    let width = params.width_px();
    let cell_h = params.cell_h;
    let row_bytes = u64::from(width) * u64::from(cell_h) * 4;
    let byte_rows = (MAX_BLOCK_BYTES / row_bytes.max(1)).clamp(1, u64::from(u16::MAX));
    let row_cap = params.max_rows.max(1).min(byte_rows as u16);

    let doc = parse::parse(markdown);
    let f = params.font_px.clamp(4.0, 512.0);
    let line_h = (cell_h as f32).max((f * 1.2).ceil());
    let mut lay = Lay {
        fs,
        fonts: &caches.fonts,
        hl: &mut caches.hl,
        math: caches.math,
        mermaid: caches.mermaid,
        p: params,
        pal: Palette::new(&params.theme),
        links: &doc.links,
        ops: vec![],
        frags: vec![],
        link_px: vec![],
        rasters: vec![],
        image_budget: MAX_IMAGE_BUDGET,
        f,
        line_h,
        scale: f / 16.0,
        y_limit: f32::from(row_cap) * cell_h as f32 + line_h,
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

    let needed = ((end.ceil() as u64).div_ceil(u64::from(cell_h))).max(1);
    let truncated = needed > u64::from(row_cap);
    let rows = if truncated { row_cap } else { needed as u16 };
    let height = u32::from(rows) * cell_h;
    let mut pm = tiny_skia::Pixmap::new(width, height).ok_or(Error::Render("pixmap".into()))?;
    let text_rows = if truncated {
        usize::from(rows) - 1
    } else {
        usize::from(rows)
    };
    paint::execute(&mut pm, &lay.ops, &lay.rasters, lay.fs, &mut caches.swash);
    let mut text = grid::text_rows(&lay.frags, params.cell_w, cell_h, text_rows);
    let links = grid::link_spans(&lay.link_px, params.cols, params.cell_w, cell_h, text_rows);
    if truncated {
        // Clear the last row and write the note there.
        let row_start = text_rows * (cell_h as usize) * (width as usize) * 4;
        pm.data_mut()[row_start..].fill(0);
        lay.ops.clear();
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
        let lctx = lay.line_ctx(base.size, false, Align::Left, cell_h as f32);
        let lines = shaped.break_lines(inner, &lctx);
        let top = text_rows as f32 * cell_h as f32
            + ((cell_h as f32 - inline::height(&lines)) / 2.0).floor();
        let (texts, _) = shaped.emit(&lines, x0, top, inner, &lctx, &mut lay.ops, lay.fonts);
        paint::execute(&mut pm, &lay.ops, &[], lay.fs, &mut caches.swash);
        let note: String = texts.into_iter().map(|t| t.text).collect();
        let col = (x0 / params.cell_w as f32).round() as u16;
        text.push(vec![TextRun { col, text: note }]);
    }
    if caches.swash.image_cache.len() > MAX_CACHED_GLYPHS {
        caches.swash.image_cache.clear();
    }
    debug_assert_eq!(text.len(), usize::from(rows));
    Ok(RenderedBlock {
        rows,
        image: paint::into_raster(pm),
        text,
        links,
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

impl Lay<'_> {
    fn gap(&self) -> f32 {
        (self.f * 0.75).round()
    }

    /// (top, bottom) margins of a block.
    fn margins(&self, b: &Block, ctx: Ctx) -> (f32, f32) {
        let g = self.gap();
        match b {
            Block::Heading(l, _) => {
                let top = if *l <= 2 { self.f * 1.5 } else { self.f * 1.25 };
                (top.round(), (self.f * 0.6).round())
            }
            Block::Paragraph(_) | Block::List { .. } if ctx.tight => (0.0, 0.0),
            Block::Rule => ((self.f * 1.1).round(), (self.f * 1.1).round()),
            _ => (g, g),
        }
    }

    fn blocks(&mut self, blocks: &[Block], x: f32, w: f32, y: f32, ctx: Ctx) -> f32 {
        let mut y = y;
        let mut prev: Option<f32> = None;
        for b in blocks {
            if y > self.y_limit {
                break;
            }
            let (top, bottom) = self.margins(b, ctx);
            if let Some(pb) = prev {
                y += pb.max(top);
            }
            y = self.block(b, x, w, y, ctx);
            prev = Some(bottom);
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
                let h = (self.f * 0.25).round().max(2.0);
                self.ops.push(Op::Rect {
                    x,
                    y,
                    w,
                    h,
                    r: 0.0,
                    color: self.pal.rule,
                });
                y + h
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

    fn line_ctx(&self, size: f32, mono: bool, align: Align, line_h: f32) -> LineCtx {
        let m = if mono {
            self.fonts.mono.metrics
        } else {
            self.fonts.sans.metrics
        };
        LineCtx {
            line_h,
            asc: m.ascent * size,
            desc: m.descent * size,
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
    fn fit(r: &Raster, max_w: f32, natural_scale: f32) -> (f32, f32) {
        let mut w = r.width as f32 * natural_scale;
        let mut h = r.height as f32 * natural_scale;
        if w > max_w && w > 0.0 {
            h *= max_w / w;
            w = max_w;
        }
        (w.max(1.0).round(), h.max(1.0).round())
    }

    fn push_raster(&mut self, r: Raster) -> usize {
        self.rasters.push(r);
        self.rasters.len() - 1
    }

    fn try_math(&mut self, tex: &str, display: bool, size: f32, color: Rgba) -> Option<Raster> {
        if tex.len() > MAX_MATH_SRC || tex.trim().is_empty() {
            return None;
        }
        match (self.math)(tex, display, size, color) {
            Ok(r)
                if r.width > 0
                    && r.height > 0
                    && r.pixels.len() >= (r.width as usize) * (r.height as usize) * 4 =>
            {
                Some(r)
            }
            Ok(_) => None,
            Err(e) => {
                tracing::debug!("maths fallback: {e}");
                None
            }
        }
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
                        let (w, h) = Self::fit(&r, max_w, 1.0);
                        let k = h / r.height as f32;
                        let asc = match r.baseline {
                            Some(b) if !*display => (b as f32 * k).round(),
                            _ => h,
                        };
                        let raster = self.push_raster(r);
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
        let loaded = images::load(url, &self.p.resources);
        let label = if alt.trim().is_empty() {
            "image".to_owned()
        } else {
            alt.trim().to_owned()
        };
        match loaded {
            Ok(r) if (r.pixels.len() as u64) <= self.image_budget => {
                self.image_budget -= r.pixels.len() as u64;
                let (w, h) = Self::fit(&r, max_w, self.scale);
                let idx = self.push_raster(r);
                para.pieces.push(Piece::Atom(Atom {
                    w,
                    h,
                    asc: h,
                    raster: idx,
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
        let lctx = self.line_ctx(
            base.size,
            false,
            align,
            self.line_h.max((base.size * 1.25).ceil()),
        );
        self.flow(&para, x, w, y, &lctx)
    }

    fn flow(&mut self, para: &Para, x: f32, w: f32, y: f32, lctx: &LineCtx) -> f32 {
        let shaped = Shaped::new(self.fs, self.fonts, para);
        let lines = shaped.break_lines(w, lctx);
        self.emit(&shaped, &lines, x, y, w, lctx);
        y + inline::height(&lines)
    }

    fn emit(&mut self, shaped: &Shaped, lines: &[Line], x: f32, y: f32, w: f32, lctx: &LineCtx) {
        let (texts, links) = shaped.emit(lines, x, y, w, lctx, &mut self.ops, self.fonts);
        self.frags.extend(texts.into_iter().map(|t| Frag {
            x: t.x,
            baseline: t.baseline,
            text: t.text,
        }));
        self.push_links(links);
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
        let lctx = self.line_ctx(
            size,
            false,
            Align::Left,
            self.line_h.max((size * 1.25).ceil()),
        );
        let mut y = self.flow(&para, x, w, y, &lctx);
        if level <= 2 {
            y += (size * 0.2).round();
            let th = self.scale.round().max(1.0);
            self.ops.push(Op::Rect {
                x,
                y,
                w,
                h: th,
                r: 0.0,
                color: self.pal.border,
            });
            y += th;
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
            if i > 0 {
                y += if tight {
                    (self.f * 0.15).round()
                } else {
                    self.gap()
                };
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
        let lctx = self.line_ctx(self.f, false, Align::Left, self.line_h);
        let default_base = y + ((lctx.line_h - lctx.asc - lctx.desc) / 2.0 + lctx.asc).round();
        let b = self.frags[first_frag..]
            .iter()
            .map(|f| f.baseline)
            .fold(None, |m: Option<f32>, v| Some(m.map_or(v, |m| m.min(v))))
            .unwrap_or(default_base);
        let end = end.max(y + self.line_h);
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
                let shaped = Shaped::new(self.fs, self.fonts, &para);
                let lctx = self.line_ctx(size, false, Align::Right, self.line_h);
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
                if !blocks.is_empty() {
                    yy += (self.f * 0.4).round();
                }
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
        let end = self
            .blocks(blocks, cx, cw, yy, c)
            .max(yy)
            .max(y + self.line_h);
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
        let pad_y = (self.line_h * 0.5).round();
        let inner = (w - 2.0 * pad_x).max(1.0);
        let first_op = self.ops.len();
        let lines = self.hl.highlight(text, lang, &self.pal);
        let lctx = self.line_ctx(self.f, true, Align::Left, self.line_h);
        let mut cy = y + pad_y;
        let lines = if lines.is_empty() {
            vec![vec![]]
        } else {
            lines
        };
        for toks in &lines {
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
            let shaped = Shaped::new(self.fs, self.fonts, &para);
            let ls = shaped.break_lines(inner, &lctx);
            if ls.is_empty() {
                cy += self.line_h;
                continue;
            }
            self.emit(&shaped, &ls, x + pad_x, cy, inner, &lctx);
            cy += inline::height(&ls);
        }
        let end = cy + pad_y;
        let r = (self.f * 0.375).round();
        self.ops.insert(
            first_op,
            Op::Rect {
                x,
                y,
                w,
                h: end - y,
                r,
                color: self.pal.code_bg,
            },
        );
        end
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
        let (dw, dh) = Self::fit(&r, w, 1.0);
        let pad = (self.f * 0.25).round();
        let ix = x + ((w - dw) / 2.0).floor();
        let idx = self.push_raster(r);
        self.ops.push(Op::Image {
            idx,
            x: ix,
            y: y + pad,
            w: dw,
            h: dh,
        });
        let h = dh + 2.0 * pad;
        let rows = (h / self.p.cell_h as f32).floor().max(1.0);
        let base = y + ((rows / 2.0).floor() + 0.75) * self.p.cell_h as f32;
        self.frags.push(Frag {
            x,
            baseline: base.min(y + h),
            text: format!("$${}$$", tex.replace('\n', " ")),
        });
        y + h.max(self.line_h)
    }

    fn diagram(&mut self, src: &str, x: f32, w: f32, y: f32) -> f32 {
        let theme = self.p.theme.clone();
        let result = (self.mermaid)(src, w.max(1.0) as u32, self.f, &theme);
        let r = match result {
            Ok(r)
                if r.width > 0
                    && r.height > 0
                    && r.pixels.len() >= (r.width as usize) * (r.height as usize) * 4 =>
            {
                r
            }
            other => {
                if let Err(e) = other {
                    tracing::debug!("mermaid fallback: {e}");
                }
                return self.code("mermaid", src, x, w, y);
            }
        };
        let (dw, dh) = Self::fit(&r, w, 1.0);
        let ix = x + ((w - dw) / 2.0).floor();
        let idx = self.push_raster(r);
        self.ops.push(Op::Image {
            idx,
            x: ix,
            y,
            w: dw,
            h: dh,
        });
        let ch = self.p.cell_h as f32;
        let rows = ((dh / ch).floor() as usize).max(1);
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
            self.frags.push(Frag {
                x,
                baseline: y + ch * (i as f32 + 0.75),
                text,
            });
        }
        y + dh.max(self.line_h)
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
        if ncols == 0 {
            return y;
        }
        let pad_x = (self.f * 0.75).round();
        let pad_y = (self.f * 0.375).round();
        let bw = self.scale.round().max(1.0);
        let empty: Vec<Inline> = vec![];
        let all: Vec<(bool, &[Vec<Inline>])> = std::iter::once((true, head))
            .chain(rows.iter().map(|r| (false, r.as_slice())))
            .collect();
        // Shape every cell once.
        let mut shaped: Vec<Vec<Shaped>> = vec![];
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
                out.push(Shaped::new(self.fs, self.fonts, &para));
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
            me.line_ctx(me.f, false, align, me.line_h)
        };
        let first_op = self.ops.len();
        let mut bg_ops = vec![];
        let mut row_ys = vec![];
        let mut ry = y + bw;
        let mut body_i = 0usize;
        for (ri, row) in shaped.iter().enumerate() {
            if ri == 0 && skip_head {
                continue;
            }
            if ry > self.y_limit {
                break;
            }
            let lines: Vec<Vec<Line>> = row
                .iter()
                .enumerate()
                .map(|(c, s)| s.break_lines((widths[c] - 2.0 * pad_x).max(1.0), &lctx_of(self, c)))
                .collect();
            let content_h = lines
                .iter()
                .map(|l| inline::height(l))
                .fold(self.line_h, f32::max);
            let rh = (content_h + 2.0 * pad_y).round();
            if ri == 0 {
                bg_ops.push(Op::Rect {
                    x,
                    y: ry,
                    w: table_w,
                    h: rh,
                    r: 0.0,
                    color: self.pal.table_head_bg,
                });
            } else {
                if body_i % 2 == 1 {
                    bg_ops.push(Op::Rect {
                        x,
                        y: ry,
                        w: table_w,
                        h: rh,
                        r: 0.0,
                        color: self.pal.table_stripe_bg,
                    });
                }
                body_i += 1;
            }
            let mut cx = x + bw;
            let mut per_line: Vec<(f32, Vec<String>)> = vec![];
            for (c, s) in row.iter().enumerate() {
                let lctx = lctx_of(self, c);
                let cw = (widths[c] - 2.0 * pad_x).max(1.0);
                let (texts, links) = s.emit(
                    &lines[c],
                    cx + pad_x,
                    ry + pad_y,
                    cw,
                    &lctx,
                    &mut self.ops,
                    self.fonts,
                );
                self.push_links(links);
                // Line k of every cell shares a baseline (top-aligned).
                let mut top = ry + pad_y;
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
            row_ys.push((ry, rh));
            ry += rh + bw;
        }
        let end = ry;
        // Borders: outer frame plus inner lines.
        let bc = self.pal.border;
        bg_ops.push(Op::Rect {
            x,
            y,
            w: table_w,
            h: bw,
            r: 0.0,
            color: bc,
        });
        for &(ry, rh) in &row_ys {
            bg_ops.push(Op::Rect {
                x,
                y: ry + rh,
                w: table_w,
                h: bw,
                r: 0.0,
                color: bc,
            });
        }
        let mut cx = x;
        for cw in widths.iter().map(Some).chain([None]) {
            bg_ops.push(Op::Rect {
                x: cx,
                y,
                w: bw,
                h: end - y,
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
