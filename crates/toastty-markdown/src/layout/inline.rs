//! Inline layout: styled text runs and inline objects (maths rasters,
//! images) → shaped glyphs → greedily wrapped lines → draw ops, text
//! fragments and link rectangles.
//!
//! Text is shaped one paragraph-run at a time with cosmic-text (no
//! wrapping), then broken into lines here so inline code boxes, inline
//! rasters and per-glyph styles stay under our control.

use cosmic_text::{
    Attrs, Buffer, CacheKey, CacheKeyFlags, Family, FontSystem, Metrics, Shaping, Style, Weight,
    Wrap, fontdb,
};

use unicode_segmentation::UnicodeSegmentation;

use super::fonts::Fonts;
use super::grid::grapheme_cells;
use super::paint::{Op, Src};
use crate::Rgba;

/// Background/border decoration around a span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Boxed {
    None,
    /// Inline code: rounded background.
    Code,
    /// Image placeholder: bordered box with an icon.
    Placeholder,
}

#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct SpanInfo {
    pub mono: bool,
    pub size: f32,
    pub bold: bool,
    pub italic: bool,
    pub color: Rgba,
    pub underline: bool,
    pub strike: bool,
    pub boxed: Boxed,
    /// Box background (inline code).
    pub bg: Rgba,
    /// Baseline shift upwards in px (superscript footnote references).
    pub rise: f32,
    pub link: Option<usize>,
}

/// An inline object laid out as one unbreakable box.
#[derive(Debug, Clone)]
pub(crate) struct Atom {
    pub w: f32,
    pub h: f32,
    /// Height above the baseline.
    pub asc: f32,
    /// What to draw.
    pub raster: Src,
    pub link: Option<usize>,
    /// Plain text the atom stands for (selection/copy).
    pub text: String,
}

#[derive(Debug, Clone)]
pub(crate) enum Piece {
    Text(String, SpanInfo),
    Atom(Atom),
    Break,
    /// An atom on a line of its own, centered (display maths).
    Display(Atom),
}

/// Pieces of one paragraph-like run of inline content.
#[derive(Debug, Default, Clone)]
pub(crate) struct Para {
    pub pieces: Vec<Piece>,
    /// Lay the text on a cell grid this many pixels wide (code blocks):
    /// every grapheme cluster advances exactly `cell_w` per terminal
    /// cell it takes (see [`grapheme_cells`]), so it lands on the same
    /// column the terminal writes it into.
    pub grid: Option<u32>,
}

impl Para {
    pub(crate) fn text(&mut self, text: &str, span: SpanInfo) {
        if text.is_empty() {
            return;
        }
        let clean: String = text
            .chars()
            .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
            .collect();
        let clean = clean.replace('\t', "    ");
        self.pieces.push(Piece::Text(clean, span));
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.pieces.is_empty()
    }
}

/// Force each cluster of `glyphs` to advance exactly `cell_w` times
/// the cells the terminal gives its text, the way the terminal's
/// cluster-width snap does for its own text. A unit is a run of glyphs
/// whose source ranges touch the same grapheme clusters (a shaping
/// cluster, a ligature over several graphemes, or a grapheme the shaper
/// split over several glyphs); it takes the sum of its graphemes'
/// [`grapheme_cells`]. The whole advance goes on the unit's first glyph
/// and the others keep their shaped offsets from the unit start (scaled
/// to the new width) with no advance of their own, so pen positions stay
/// whole cells and a line break can never split a unit.
fn snap_to_cells(glyphs: &mut [Glyph], text: &str, cell_w: f32) {
    let (Some(lo), Some(hi)) = (
        glyphs.iter().map(|g| g.start).min(),
        glyphs.iter().map(|g| g.end).max(),
    ) else {
        return;
    };
    let Some(src) = text.get(lo..hi) else {
        return;
    };
    // (start byte, cells) of each grapheme in `lo..hi`.
    let graphemes: Vec<(usize, u32)> = src
        .grapheme_indices(true)
        .map(|(i, g)| (lo + i, grapheme_cells(g)))
        .collect();
    let index = |b: usize| graphemes.partition_point(|g| g.0 <= b).saturating_sub(1);
    let span = |g: &Glyph| (index(g.start), index(g.end.saturating_sub(1).max(g.start)));
    let mut i = 0;
    while i < glyphs.len() {
        let (mut a, mut b) = span(&glyphs[i]);
        let mut j = i + 1;
        while j < glyphs.len() {
            let (s, e) = span(&glyphs[j]);
            if s > b || e < a {
                break;
            }
            (a, b) = (a.min(s), b.max(e));
            j += 1;
        }
        let cells: u32 = graphemes[a..=b].iter().map(|g| g.1).sum();
        let target = cells as f32 * cell_w;
        let natural: f32 = glyphs[i..j].iter().map(|g| g.w).sum();
        let k = if natural > f32::EPSILON {
            target / natural
        } else {
            0.0
        };
        let mut off = 0.0;
        for (n, g) in glyphs[i..j].iter_mut().enumerate() {
            let w = g.w * k;
            if n == 0 {
                g.w = target;
            } else {
                g.x_off += off - target;
                g.w = 0.0;
            }
            off += w;
        }
        i = j;
    }
}

#[derive(Debug, Clone, Copy)]
struct Glyph {
    start: usize,
    end: usize,
    w: f32,
    x_off: f32,
    y_off: f32,
    font_id: fontdb::ID,
    id: u16,
    size: f32,
    weight: Weight,
    flags: CacheKeyFlags,
    span: usize,
    /// Extra space before / after the glyph (box padding).
    lead: f32,
    trail: f32,
}

impl Glyph {
    fn adv(&self) -> f32 {
        self.lead + self.w + self.trail
    }
}

#[derive(Debug, Clone)]
enum Tok {
    Word(std::ops::Range<usize>),
    Space(std::ops::Range<usize>),
    Atom(usize),
    Break,
    Display(usize),
}

/// Vertical metrics and wrapping behaviour of a run of lines.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LineCtx {
    /// Minimum line pitch.
    pub line_h: f32,
    /// Ascent / descent of the run's main font at its size, in px.
    pub asc: f32,
    pub desc: f32,
    /// Keep runs of spaces (code); otherwise they collapse to one.
    pub preserve_ws: bool,
    pub align: Align,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Align {
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, Copy)]
enum PlacedKind {
    Glyph(usize),
    Space(usize, usize),
    Atom(usize),
}

#[derive(Debug, Clone, Copy)]
struct Placed {
    kind: PlacedKind,
    x: f32,
    w: f32,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Line {
    items: Vec<Placed>,
    pub w: f32,
    center: bool,
    /// Line height and baseline offset from the line top.
    pub h: f32,
    pub base: f32,
}

/// A shaped paragraph, ready to be broken into lines.
#[derive(Debug, Clone, Default)]
pub(crate) struct Shaped {
    text: String,
    spans: Vec<SpanInfo>,
    glyphs: Vec<Glyph>,
    pub atoms: Vec<Atom>,
    toks: Vec<Tok>,
}

/// A laid-out line's text and baseline, for the text grid.
#[derive(Debug, Clone)]
pub(crate) struct LineText {
    pub x: f32,
    pub baseline: f32,
    pub text: String,
}

/// A link's horizontal extent on one line.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LinkRect {
    pub x0: f32,
    pub x1: f32,
    pub baseline: f32,
    pub link: usize,
}

fn is_break_after(c: char) -> bool {
    matches!(c as u32,
        0x2E80..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF | 0x20000..=0x3FFFF)
}

fn is_space(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_whitespace() && c != '\u{a0}')
}

fn box_pads(b: Boxed, size: f32) -> (f32, f32) {
    match b {
        Boxed::None => (0.0, 0.0),
        Boxed::Code => (size * 0.3, size * 0.3),
        Boxed::Placeholder => (size * 1.45, size * 0.45),
    }
}

impl Shaped {
    pub(crate) fn new(fs: &mut FontSystem, fonts: &Fonts, para: &Para) -> Self {
        let mut s = Shaped::default();
        let mut group: Vec<(usize, usize)> = vec![]; // (text start, span)
        let grid = para.grid.map(|c| c as f32);
        let flush = |s: &mut Shaped, group: &mut Vec<(usize, usize)>, fs: &mut FontSystem| {
            if !group.is_empty() {
                s.shape_group(fs, fonts, group, grid);
                group.clear();
            }
        };
        for p in &para.pieces {
            match p {
                Piece::Text(t, span) => {
                    let start = s.text.len();
                    s.text.push_str(t);
                    s.spans.push(*span);
                    group.push((start, s.spans.len() - 1));
                }
                Piece::Atom(a) => {
                    flush(&mut s, &mut group, fs);
                    s.atoms.push(a.clone());
                    s.toks.push(Tok::Atom(s.atoms.len() - 1));
                }
                Piece::Display(a) => {
                    flush(&mut s, &mut group, fs);
                    s.atoms.push(a.clone());
                    s.toks.push(Tok::Display(s.atoms.len() - 1));
                }
                Piece::Break => {
                    flush(&mut s, &mut group, fs);
                    s.toks.push(Tok::Break);
                }
            }
        }
        flush(&mut s, &mut group, fs);
        s
    }

    fn shape_group(
        &mut self,
        fs: &mut FontSystem,
        fonts: &Fonts,
        group: &[(usize, usize)],
        grid: Option<f32>,
    ) {
        let end_of = |i: usize| group.get(i + 1).map_or(self.text.len(), |g| g.0);
        let first = self.spans[group[0].1];
        let mut buf = Buffer::new_empty(Metrics::new(first.size, first.size * 1.2));
        buf.set_wrap(Wrap::None);
        buf.set_size(None, None);
        let spans: Vec<(&str, Attrs<'_>)> = group
            .iter()
            .enumerate()
            .map(|(i, &(start, si))| {
                let sp = &self.spans[si];
                let role = if sp.mono { &fonts.mono } else { &fonts.sans };
                let weight = if sp.bold && !sp.mono {
                    Weight::BOLD
                } else {
                    role.regular
                };
                let attrs = Attrs::new()
                    .family(Family::Name(&role.family))
                    .weight(weight)
                    .style(if sp.italic {
                        Style::Italic
                    } else {
                        Style::Normal
                    })
                    .metrics(Metrics::new(sp.size, sp.size * 1.2))
                    .metadata(si);
                (&self.text[start..end_of(i)], attrs)
            })
            .collect();
        let default = spans[0].1.clone();
        buf.set_rich_text(spans.iter().cloned(), &default, Shaping::Advanced, None);
        buf.shape_until_scroll(fs, false);
        let base = group[0].0;
        let g0 = self.glyphs.len();
        for run in buf.layout_runs() {
            for g in run.glyphs {
                self.glyphs.push(Glyph {
                    start: base + g.start,
                    end: base + g.end,
                    w: g.w,
                    x_off: g.x_offset * g.font_size,
                    y_off: g.y_offset * g.font_size,
                    font_id: g.font_id,
                    id: g.glyph_id,
                    size: g.font_size,
                    weight: g.font_weight,
                    flags: g.cache_key_flags,
                    span: g.metadata,
                    lead: 0.0,
                    trail: 0.0,
                });
            }
        }
        let n = self.glyphs.len();
        if let Some(cell_w) = grid {
            snap_to_cells(&mut self.glyphs[g0..n], &self.text, cell_w);
        }
        // Box padding at span boundaries.
        for i in g0..n {
            let sp = self.glyphs[i].span;
            let info = self.spans[sp];
            if info.boxed == Boxed::None {
                continue;
            }
            let (l, t) = box_pads(info.boxed, info.size);
            if i == g0 || self.glyphs[i - 1].span != sp {
                self.glyphs[i].lead = l;
            }
            if i + 1 == n || self.glyphs[i + 1].span != sp {
                self.glyphs[i].trail = t;
            }
        }
        // Tokens.
        let mut i = g0;
        while i < n {
            let g = self.glyphs[i];
            if is_space(&self.text[g.start..g.end]) {
                let s = i;
                while i < n && is_space(&self.text[self.glyphs[i].start..self.glyphs[i].end]) {
                    i += 1;
                }
                self.toks.push(Tok::Space(s..i));
            } else {
                let s = i;
                while i < n {
                    let gi = self.glyphs[i];
                    let t = &self.text[gi.start..gi.end];
                    if is_space(t) {
                        break;
                    }
                    i += 1;
                    if t.chars().last().is_some_and(is_break_after) {
                        break;
                    }
                }
                self.toks.push(Tok::Word(s..i));
            }
        }
    }

    fn width(&self, r: &std::ops::Range<usize>) -> f32 {
        self.glyphs[r.clone()].iter().map(Glyph::adv).sum()
    }

    /// (widest unbreakable piece, width with no wrapping).
    pub(crate) fn measure(&self) -> (f32, f32) {
        let (mut min, mut max, mut cur) = (0.0f32, 0.0f32, 0.0f32);
        for t in &self.toks {
            match t {
                Tok::Word(r) => {
                    let w = self.width(r);
                    min = min.max(w);
                    cur += w;
                }
                Tok::Space(r) => cur += self.width(r),
                Tok::Atom(a) => {
                    min = min.max(self.atoms[*a].w);
                    cur += self.atoms[*a].w;
                }
                Tok::Break => {
                    max = max.max(cur);
                    cur = 0.0;
                }
                Tok::Display(a) => {
                    max = max.max(cur).max(self.atoms[*a].w);
                    min = min.max(self.atoms[*a].w);
                    cur = 0.0;
                }
            }
        }
        (min, max.max(cur))
    }

    /// Greedy line breaking at `width`.
    pub(crate) fn break_lines(&self, width: f32, ctx: &LineCtx) -> Vec<Line> {
        let width = width.max(1.0);
        let mut lines = vec![];
        let mut cur = Line::default();
        let mut x = 0.0f32;
        let mut pend: Option<std::ops::Range<usize>> = None;
        let mut after_wrap = false;
        let mut after_display = false;
        // Code: wrapped continuation lines keep the source line's indent.
        let mut indent = 0.0f32;
        let push = |lines: &mut Vec<Line>, cur: &mut Line, x: &mut f32| {
            cur.w = *x;
            lines.push(std::mem::take(cur));
            *x = 0.0;
        };
        for t in &self.toks {
            let was_display = std::mem::replace(&mut after_display, matches!(t, Tok::Display(_)));
            match t {
                Tok::Space(r) => {
                    if ctx.preserve_ws && cur.items.is_empty() && !after_wrap {
                        let w = self.width(r);
                        cur.items.push(Placed {
                            kind: PlacedKind::Space(r.start, r.end),
                            x,
                            w,
                        });
                        x += w;
                        indent = if w < width / 2.0 { w } else { 0.0 };
                    } else {
                        pend = Some(match pend.take() {
                            Some(p) => p.start..r.end,
                            None => r.clone(),
                        });
                    }
                }
                Tok::Word(_) | Tok::Atom(_) => {
                    let w = match t {
                        Tok::Word(r) => self.width(r),
                        Tok::Atom(a) => self.atoms[*a].w,
                        _ => unreachable!(),
                    };
                    let sp = pend.as_ref().map_or(0.0, |p| {
                        if ctx.preserve_ws {
                            self.width(p)
                        } else {
                            self.glyphs[p.start].adv()
                        }
                    });
                    if !cur.items.is_empty() && x + sp + w > width + 0.01 {
                        push(&mut lines, &mut cur, &mut x);
                        after_wrap = true;
                        if ctx.preserve_ws && indent + w <= width {
                            x = indent;
                        }
                    } else if let Some(p) = pend.take()
                        && !cur.items.is_empty()
                    {
                        cur.items.push(Placed {
                            kind: PlacedKind::Space(p.start, p.end),
                            x,
                            w: sp,
                        });
                        x += sp;
                    }
                    pend = None;
                    match t {
                        Tok::Word(r) => {
                            let split = w > width && cur.items.is_empty();
                            for gi in r.clone() {
                                let a = self.glyphs[gi].adv();
                                // Zero-advance glyphs (marks, the tail of
                                // a cell-snapped cluster) never start a line.
                                if split && a > 0.0 && !cur.items.is_empty() && x + a > width + 0.01
                                {
                                    push(&mut lines, &mut cur, &mut x);
                                    after_wrap = true;
                                }
                                cur.items.push(Placed {
                                    kind: PlacedKind::Glyph(gi),
                                    x,
                                    w: a,
                                });
                                x += a;
                            }
                        }
                        Tok::Atom(a) => {
                            cur.items.push(Placed {
                                kind: PlacedKind::Atom(*a),
                                x,
                                w,
                            });
                            x += w;
                        }
                        _ => unreachable!(),
                    }
                }
                Tok::Break => {
                    if !(cur.items.is_empty() && was_display) {
                        push(&mut lines, &mut cur, &mut x);
                    }
                    indent = 0.0;
                    after_wrap = false;
                    pend = None;
                }
                Tok::Display(a) => {
                    if !cur.items.is_empty() {
                        push(&mut lines, &mut cur, &mut x);
                    }
                    let w = self.atoms[*a].w;
                    cur.items.push(Placed {
                        kind: PlacedKind::Atom(*a),
                        x: 0.0,
                        w,
                    });
                    cur.center = true;
                    x = w;
                    push(&mut lines, &mut cur, &mut x);
                    after_wrap = false;
                    pend = None;
                }
            }
        }
        if !cur.items.is_empty() {
            push(&mut lines, &mut cur, &mut x);
        }
        // Vertical metrics.
        let leading = (ctx.line_h - (ctx.asc + ctx.desc)).max(0.0);
        for l in &mut lines {
            let (mut asc, mut desc) = (ctx.asc, ctx.desc);
            for p in &l.items {
                if let PlacedKind::Atom(a) = p.kind {
                    let at = &self.atoms[a];
                    asc = asc.max(at.asc);
                    desc = desc.max(at.h - at.asc);
                }
            }
            let tall = asc > ctx.asc + 0.5 || desc > ctx.desc + 0.5;
            let pad = if tall {
                (leading / 2.0).max(ctx.line_h * 0.1)
            } else {
                leading / 2.0
            };
            l.h = (asc + desc + 2.0 * pad).max(ctx.line_h).ceil();
            l.base = ((l.h - asc - desc) / 2.0 + asc).round();
        }
        lines
    }

    fn line_offset(line: &Line, width: f32, align: Align) -> f32 {
        let free = (width - line.w).max(0.0);
        if line.center {
            return (free / 2.0).floor();
        }
        match align {
            Align::Left => 0.0,
            Align::Center => (free / 2.0).floor(),
            Align::Right => free,
        }
    }

    /// Draw `lines` with their top-left at (`x`, `y`) inside a box of
    /// `width`. Returns each line's text and the link rectangles.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn emit(
        &self,
        lines: &[Line],
        x: f32,
        y: f32,
        width: f32,
        ctx: &LineCtx,
        ops: &mut Vec<Op>,
        fonts: &Fonts,
    ) -> (Vec<LineText>, Vec<LinkRect>) {
        let mut texts = vec![];
        let mut links = vec![];
        let mut top = y;
        for line in lines {
            let ox = x + Self::line_offset(line, width, ctx.align);
            let base = top + line.base;
            // Boxes first so glyphs draw over them.
            let mut i = 0;
            while i < line.items.len() {
                let Some(sp) = self.span_of(&line.items[i]) else {
                    i += 1;
                    continue;
                };
                let info = self.spans[sp];
                if info.boxed == Boxed::None {
                    i += 1;
                    continue;
                }
                let x0 = line.items[i].x;
                let mut j = i;
                while j + 1 < line.items.len() && self.span_of(&line.items[j + 1]) == Some(sp) {
                    j += 1;
                }
                let x1 = line.items[j].x + line.items[j].w;
                let m = if info.mono {
                    fonts.mono.metrics
                } else {
                    fonts.sans.metrics
                };
                let a = (m.ascent * info.size).max(info.size * 0.9);
                let d = (m.descent * info.size).max(info.size * 0.22);
                let (bx, by, bw, bh) = (ox + x0, base - a - 1.0, x1 - x0, a + d + 2.0);
                match info.boxed {
                    Boxed::Code => ops.push(Op::Rect {
                        x: bx,
                        y: by,
                        w: bw,
                        h: bh,
                        r: (info.size * 0.25).min(6.0),
                        color: info.bg,
                    }),
                    Boxed::Placeholder => {
                        ops.push(Op::Stroke {
                            x: bx + 0.5,
                            y: by + 0.5,
                            w: bw - 1.0,
                            h: bh - 1.0,
                            r: (info.size * 0.25).min(6.0),
                            width: 1.0,
                            color: info.color,
                        });
                        let s = info.size * 0.85;
                        ops.push(Op::PictureIcon {
                            x: bx + info.size * 0.35,
                            y: base - s * 0.85,
                            s,
                            color: info.color,
                        });
                    }
                    Boxed::None => {}
                }
                i = j + 1;
            }
            let mut text = String::new();
            let mut last_cluster = usize::MAX;
            for p in &line.items {
                match p.kind {
                    PlacedKind::Glyph(gi) => {
                        let g = &self.glyphs[gi];
                        let info = &self.spans[g.span];
                        let (key, gx, gy) = CacheKey::new(
                            g.font_id,
                            g.id,
                            g.size,
                            (
                                ox + p.x + g.lead + g.x_off,
                                (base - info.rise - g.y_off).trunc(),
                            ),
                            g.weight,
                            g.flags,
                        );
                        ops.push(Op::Glyph {
                            key,
                            x: gx,
                            y: gy,
                            color: info.color,
                        });
                        if g.start != last_cluster {
                            text.push_str(&self.text[g.start..g.end]);
                            last_cluster = g.start;
                        }
                    }
                    PlacedKind::Space(s, e) => {
                        if ctx.preserve_ws {
                            let mut last = usize::MAX;
                            for g in &self.glyphs[s..e] {
                                if g.start != last {
                                    text.push_str(&self.text[g.start..g.end]);
                                    last = g.start;
                                }
                            }
                        } else {
                            text.push(' ');
                        }
                    }
                    PlacedKind::Atom(a) => {
                        let at = &self.atoms[a];
                        ops.push(Op::Image {
                            src: at.raster.clone(),
                            x: (ox + p.x).round(),
                            y: (base - at.asc).round(),
                            w: at.w,
                            h: at.h,
                        });
                        text.push_str(&at.text);
                    }
                }
            }
            // Decorations and links.
            let mut deco: Option<(f32, f32, Rgba, f32, f32)> = None;
            let flush_deco = |d: Option<(f32, f32, Rgba, f32, f32)>, ops: &mut Vec<Op>| {
                if let Some((x0, x1, color, yy, th)) = d {
                    ops.push(Op::Rect {
                        x: x0,
                        y: yy,
                        w: x1 - x0,
                        h: th,
                        r: 0.0,
                        color,
                    });
                }
            };
            for kind in [0u8, 1u8] {
                for p in &line.items {
                    let Some(sp) = self.span_of(p) else {
                        flush_deco(deco.take(), ops);
                        continue;
                    };
                    let info = self.spans[sp];
                    let on = if kind == 0 {
                        info.underline
                    } else {
                        info.strike
                    };
                    if !on {
                        flush_deco(deco.take(), ops);
                        continue;
                    }
                    let m = if info.mono {
                        fonts.mono.metrics
                    } else {
                        fonts.sans.metrics
                    };
                    let th = (m.underline_thickness * info.size).max(1.0).round();
                    let yy = if kind == 0 {
                        (base + (m.underline_offset * info.size).max(1.0)).round()
                    } else {
                        (base - m.x_height * info.size * 0.5 - th / 2.0).round()
                    };
                    let (x0, x1) = (ox + p.x, ox + p.x + p.w);
                    deco = match deco {
                        Some((a, b, c, dy, dth))
                            if c == info.color && (dy - yy).abs() < 0.5 && (b - x0).abs() < 1.0 =>
                        {
                            Some((a, x1, c, dy, dth))
                        }
                        other => {
                            flush_deco(other, ops);
                            Some((x0, x1, info.color, yy, th))
                        }
                    };
                }
                flush_deco(deco.take(), ops);
            }
            let mut cur: Option<LinkRect> = None;
            for p in &line.items {
                let link = match p.kind {
                    PlacedKind::Atom(a) => self.atoms[a].link,
                    _ => self.span_of(p).and_then(|s| self.spans[s].link),
                };
                let (x0, x1) = (ox + p.x, ox + p.x + p.w);
                match (link, &mut cur) {
                    (Some(l), Some(c)) if c.link == l => c.x1 = x1,
                    (Some(l), _) => {
                        if let Some(c) = cur.take() {
                            links.push(c);
                        }
                        cur = Some(LinkRect {
                            x0,
                            x1,
                            baseline: base,
                            link: l,
                        });
                    }
                    (None, _) => {
                        if let Some(c) = cur.take() {
                            links.push(c);
                        }
                    }
                }
            }
            if let Some(c) = cur.take() {
                links.push(c);
            }
            let trimmed = if ctx.preserve_ws {
                text.trim_end().to_owned()
            } else {
                text.trim().to_owned()
            };
            if !trimmed.is_empty() {
                let first_x = line.items.first().map_or(0.0, |p| p.x);
                let first_x = if ctx.preserve_ws && first_x > 0.0 {
                    first_x
                } else {
                    0.0
                };
                texts.push(LineText {
                    x: ox + first_x,
                    baseline: base,
                    text: trimmed,
                });
            }
            top += line.h;
        }
        (texts, links)
    }

    fn span_of(&self, p: &Placed) -> Option<usize> {
        match p.kind {
            PlacedKind::Glyph(g) => Some(self.glyphs[g].span),
            PlacedKind::Space(s, _) => Some(self.glyphs[s].span),
            PlacedKind::Atom(_) => None,
        }
    }
}

/// Total height of `lines`.
pub(crate) fn height(lines: &[Line]) -> f32 {
    lines.iter().map(|l| l.h).sum()
}

/// Content key of a paragraph: everything `Shaped::new` reads. 128 bits
/// (two independently seeded hashes) so a collision is not a concern.
pub(crate) fn para_key(para: &Para) -> u128 {
    use std::hash::{Hash, Hasher};
    fn span<H: Hasher>(h: &mut H, s: &SpanInfo) {
        (
            s.mono,
            s.bold,
            s.italic,
            s.underline,
            s.strike,
            s.boxed as u8,
        )
            .hash(h);
        (s.size.to_bits(), s.rise.to_bits(), s.color, s.bg, s.link).hash(h);
    }
    fn atom<H: Hasher>(h: &mut H, a: &Atom) {
        (
            a.w.to_bits(),
            a.h.to_bits(),
            a.asc.to_bits(),
            a.raster.id(),
            a.link,
        )
            .hash(h);
        a.text.hash(h);
    }
    let mut out = 0u128;
    for seed in [0x9e37_79b9_u64, 0x85eb_ca6b] {
        let mut h = std::hash::DefaultHasher::new();
        seed.hash(&mut h);
        para.pieces.len().hash(&mut h);
        para.grid.hash(&mut h);
        for p in &para.pieces {
            match p {
                Piece::Text(t, s) => {
                    0u8.hash(&mut h);
                    t.hash(&mut h);
                    span(&mut h, s);
                }
                Piece::Atom(a) => {
                    1u8.hash(&mut h);
                    atom(&mut h, a);
                }
                Piece::Break => 2u8.hash(&mut h),
                Piece::Display(a) => {
                    3u8.hash(&mut h);
                    atom(&mut h, a);
                }
            }
        }
        out = (out << 64) | u128::from(h.finish());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glyph(start: usize, end: usize, w: f32) -> Glyph {
        Glyph {
            start,
            end,
            w,
            x_off: 0.0,
            y_off: 0.0,
            font_id: fontdb::ID::dummy(),
            id: 1,
            size: 16.0,
            weight: Weight::NORMAL,
            flags: CacheKeyFlags::empty(),
            span: 0,
            lead: 0.0,
            trail: 0.0,
        }
    }

    #[test]
    fn clusters_snap_to_whole_cells() {
        // "a", "中" (2 cells), "e" + combining acute as one two-glyph
        // cluster, a lone zero-width space.
        let text = "a\u{4e2d}e\u{301}\u{200b}";
        let mut gs = [
            glyph(0, 1, 9.6),
            glyph(1, 4, 15.0),
            glyph(4, 7, 9.6),
            glyph(4, 7, 2.4),
            glyph(7, 10, 0.0),
        ];
        snap_to_cells(&mut gs, text, 10.0);
        let adv: Vec<f32> = gs.iter().map(|g| g.w).collect();
        assert_eq!(adv, [10.0, 20.0, 10.0, 0.0, 0.0]);
        // The mark keeps its shaped offset from the cluster start
        // (scaled to the cell): drawn at start + 9.6 * 10/12 = start + 8,
        // i.e. at pen (start + 10) + x_off.
        assert!((gs[3].x_off - (8.0 - 10.0)).abs() < 1e-4, "{}", gs[3].x_off);
        assert!(gs[2].x_off.abs() < 1e-6);
    }

    #[test]
    fn graphemes_split_over_shaping_clusters_are_one_unit() {
        // A family emoji shaped without an emoji font: one glyph per
        // codepoint, each its own shaping cluster. The grapheme is 2
        // cells, all on the first glyph.
        let text = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}z";
        let mut gs = [
            glyph(0, 4, 8.0),
            glyph(4, 7, 0.0),
            glyph(7, 11, 8.0),
            glyph(11, 14, 0.0),
            glyph(14, 18, 8.0),
            glyph(18, 19, 8.0),
        ];
        snap_to_cells(&mut gs, text, 10.0);
        let adv: Vec<f32> = gs.iter().map(|g| g.w).collect();
        assert_eq!(adv, [20.0, 0.0, 0.0, 0.0, 0.0, 10.0]);
        // ❤ + VS16: emoji presentation, 2 cells.
        let text = "x\u{2764}\u{fe0f}y";
        let mut gs = [
            glyph(0, 1, 8.0),
            glyph(1, 4, 9.0),
            glyph(4, 7, 0.0),
            glyph(7, 8, 8.0),
        ];
        snap_to_cells(&mut gs, text, 10.0);
        let adv: Vec<f32> = gs.iter().map(|g| g.w).collect();
        assert_eq!(adv, [10.0, 20.0, 0.0, 10.0]);
        // A ligature glyph over two graphemes ("->") takes both cells.
        let mut gs = [glyph(0, 2, 7.0), glyph(2, 3, 9.6)];
        snap_to_cells(&mut gs, "->x", 10.0);
        let adv: Vec<f32> = gs.iter().map(|g| g.w).collect();
        assert_eq!(adv, [20.0, 10.0]);
    }
}
