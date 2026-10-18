//! Mermaid → [`Raster`]: `mermaid-svg` parses and lays the diagram out as
//! SVG, resvg rasterizes it with the bundled Noto Sans.
//!
//! - **Theme:** the renderer's theme (its `themeVariables` equivalent) is
//!   built from the terminal palette: text and lines from `fg`, node
//!   fills are ANSI accents blended into `bg`, categorical palettes (pie,
//!   git, journey …) from the ANSI accents. Theme settings in the source
//!   (`%%{init}%%`, frontmatter `config`) are ignored so diagrams always
//!   match the terminal. The few colours the renderer hard-codes (gantt
//!   bars, marker fills) are remapped in the SVG. The canvas is
//!   transparent; only the diagram's own shapes are painted.
//! - **Fonts:** hermetic. The renderer estimates text widths itself
//!   (no font access) and resvg sees a font database holding only the
//!   bundled faces.
//! - **Size:** laid out at [`BASE_FONT_PX`] and scaled so labels come out
//!   at `font_px`, then scaled down to fit `max_width_px` (at most
//!   [`MAX_WIDTH_PX`]), [`MAX_HEIGHT_PX`] and [`MAX_PIXELS`]. The canvas
//!   grows to the drawn content when the renderer's view box is too
//!   tight, so nothing is clipped.
//! - **Robustness:** sources over [`MAX_SOURCE_BYTES`], or graphs with
//!   more than [`MAX_NODES`] nodes / [`MAX_EDGES`] edges, are rejected
//!   before layout, and panics inside the renderer are caught and
//!   returned as errors.

use std::borrow::Cow;
use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, OnceLock};

use mermaid_svg::ast::Diagram;
use resvg::usvg;
use tiny_skia::{Pixmap, Transform};

use crate::{Raster, Rgba, Theme};

/// Largest accepted Mermaid source.
pub const MAX_SOURCE_BYTES: usize = 64 * 1024;
/// Most nodes (flowchart nodes and subgraphs, classes, states, entities)
/// a graph diagram may have; graph layout is super-linear.
pub const MAX_NODES: usize = 500;
/// Most edges (links, relations, transitions) a graph diagram may have.
pub const MAX_EDGES: usize = 600;
/// Deepest allowed nesting of subgraphs / composite states.
pub const MAX_NESTING: usize = 32;
/// Output width cap regardless of `max_width_px`.
pub const MAX_WIDTH_PX: u32 = 4096;
/// Output height cap; taller diagrams are scaled down to fit.
pub const MAX_HEIGHT_PX: u32 = 8192;
/// Output pixel-count cap (64 MiB of RGBA).
pub const MAX_PIXELS: u64 = 16 * 1024 * 1024;
/// Smallest label size worth showing: diagrams that only fit by shrinking
/// labels below this are rejected (the caller shows the source instead).
pub const MIN_LABEL_PX: f32 = 4.0;
/// Label font size the diagram is laid out at (the renderer's tuning
/// size) before scaling to `font_px`.
pub const BASE_FONT_PX: f32 = 14.0;

const FONT_FAMILY: &str = "Noto Sans";
static FONT_REGULAR: &[u8] = include_bytes!("../fonts/NotoSans-Regular.ttf");
static FONT_BOLD: &[u8] = include_bytes!("../fonts/NotoSans-Bold.ttf");

/// Render a Mermaid diagram no wider than `max_width_px`, with labels at
/// about `font_px` pixels and colours taken from `theme`. Returns a
/// straight-alpha RGBA raster with a transparent background and
/// `baseline: None`.
///
/// # Errors
/// Returns a message describing the failure (bad parameters, source too
/// large or too complex, parse error, renderer panic); callers fall back
/// to showing the source as a code block.
pub fn render_mermaid(
    src: &str,
    max_width_px: u32,
    font_px: f32,
    theme: &Theme,
) -> Result<Raster, String> {
    if src.len() > MAX_SOURCE_BYTES {
        return Err(format!(
            "mermaid source is {} bytes (limit {MAX_SOURCE_BYTES})",
            src.len()
        ));
    }
    if src.trim().is_empty() {
        return Err("empty mermaid source".into());
    }
    if max_width_px == 0 {
        return Err("zero width".into());
    }
    if !font_px.is_finite() || font_px <= 0.0 {
        return Err(format!("invalid font size {font_px}"));
    }
    guarded(|| {
        let svg = render_svg(src, theme)?;
        rasterize(&svg, max_width_px, font_px)
    })
}

/// Run `f`, turning a panic into an error.
fn guarded<T>(f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .map(ToString::to_string)
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".into());
            Err(format!("mermaid renderer panicked: {msg}"))
        }
    }
}

/// Mermaid source → themed SVG string.
fn render_svg(src: &str, theme: &Theme) -> Result<String, String> {
    let mut diagram = mermaid_svg::parse(src).map_err(|e| e.to_string())?;
    check_complexity(&diagram)?;
    if let Diagram::Gantt(g) = &mut diagram {
        check_gantt(g)?;
        // An explicit tick interval over a long span emits one label per
        // tick (millions for `1day` over centuries); the automatic step
        // is sized to the width instead.
        g.tick_interval = None;
    }
    let palette = Palette::new(theme);
    // `render_diagram_with` ignores the source's theme/config preamble.
    let svg = mermaid_svg::render_diagram_with(&diagram, &palette.mermaid_theme())
        .map_err(|e| e.to_string())?;
    Ok(palette.remap_literals(&svg, matches!(diagram, Diagram::Gantt(_))))
}

fn check_complexity(diagram: &Diagram) -> Result<(), String> {
    let (nodes, edges) = match diagram {
        Diagram::Flowchart(f) => {
            let children: HashMap<&str, Vec<&str>> = f
                .subgraphs
                .iter()
                .map(|g| {
                    let kids = g.child_subgraph_ids.iter().chain(&g.node_ids);
                    (g.id.as_str(), kids.map(String::as_str).collect())
                })
                .collect();
            check_nesting(&children)?;
            (f.nodes.len() + f.subgraphs.len(), f.edges.len())
        }
        Diagram::State(s) => {
            let children: HashMap<&str, Vec<&str>> = s
                .composites
                .iter()
                .map(|c| {
                    let kids = c.regions.iter().flatten().map(String::as_str);
                    (c.id.as_str(), kids.collect())
                })
                .collect();
            check_nesting(&children)?;
            (
                s.states.len() + s.composites.len() + s.notes.len(),
                s.transitions.len(),
            )
        }
        Diagram::Class(c) => (
            c.classes.len() + c.namespaces.len() + c.notes.len(),
            c.relations.len(),
        ),
        Diagram::Er(e) => (e.entities.len(), e.relations.len()),
        Diagram::Mindmap(m) => {
            // Iterative count (the tree can be deep).
            let mut count = 0;
            let mut stack: Vec<&mermaid_svg::ast::MindmapNode> = m.root.iter().collect();
            while let Some(n) = stack.pop() {
                count += 1;
                stack.extend(&n.children);
            }
            (count, 0)
        }
        _ => (0, 0),
    };
    if nodes > MAX_NODES {
        return Err(format!("diagram has {nodes} nodes (limit {MAX_NODES})"));
    }
    if edges > MAX_EDGES {
        return Err(format!("diagram has {edges} edges (limit {MAX_EDGES})"));
    }
    Ok(())
}

/// Longest total of gantt task durations, in days. The renderer walks
/// the chart day by day in places.
pub const MAX_GANTT_DAYS: f64 = 100.0 * 366.0;

/// Bound a gantt chart's time span: durations add up to at most
/// [`MAX_GANTT_DAYS`] and dates have at most four-digit numbers (years
/// 0–9999), or plausible Unix timestamps for `dateFormat X` / `x`.
fn check_gantt(g: &mermaid_svg::ast::GanttDiagram) -> Result<(), String> {
    use mermaid_svg::ast::{TaskEnd, TaskStart};
    let format = g.date_format.as_deref().unwrap_or("");
    let max_digits = if format.contains('x') {
        13 // milliseconds, up to year 2286
    } else if format.contains('X') {
        10 // seconds, up to year 2286
    } else {
        4
    };
    let date_ok = |d: &str| {
        d.split(|c: char| !c.is_ascii_digit())
            .all(|run| run.len() <= max_digits)
    };
    let mut total = 0.0;
    for t in g.sections.iter().flat_map(|s| s.tasks.iter()) {
        if let TaskStart::Date(d) = &t.start
            && !date_ok(d)
        {
            return Err(format!("gantt date out of range: {d:?}"));
        }
        match &t.end {
            TaskEnd::Date(d) if !date_ok(d) => {
                return Err(format!("gantt date out of range: {d:?}"));
            }
            TaskEnd::Duration(days) => {
                if !days.is_finite() || *days < 0.0 {
                    return Err(format!("bad gantt duration {days}"));
                }
                total += days;
            }
            _ => {}
        }
    }
    if total > MAX_GANTT_DAYS {
        return Err(format!(
            "gantt spans {total:.0} days (limit {MAX_GANTT_DAYS})"
        ));
    }
    for d in &g.excludes {
        if !date_ok(d) {
            return Err(format!("gantt date out of range: {d:?}"));
        }
    }
    Ok(())
}

/// Reject containment cycles (a composite state inside itself sends the
/// renderer into unbounded recursion and a stack overflow, which
/// `catch_unwind` cannot catch) and nesting deeper than [`MAX_NESTING`].
fn check_nesting(children: &HashMap<&str, Vec<&str>>) -> Result<(), String> {
    // Iterative DFS computing each container's height (levels of nesting
    // at and below it); `on_path` detects cycles.
    let mut height: HashMap<&str, usize> = HashMap::new();
    let kids_of = |n: &str| children.get(n).map_or(&[][..], Vec::as_slice);
    for &root in children.keys() {
        let mut on_path: Vec<&str> = Vec::new();
        let mut stack: Vec<(&str, usize)> = vec![(root, 0)];
        while let Some(&(node, next)) = stack.last() {
            if next == 0 {
                if height.contains_key(node) {
                    stack.pop();
                    continue;
                }
                on_path.push(node);
            }
            if let Some(&kid) = kids_of(node).get(next) {
                if let Some(top) = stack.last_mut() {
                    top.1 += 1;
                }
                if !children.contains_key(kid) || height.contains_key(kid) {
                    continue;
                }
                if on_path.contains(&kid) {
                    return Err(format!("{kid:?} is nested inside itself"));
                }
                stack.push((kid, 0));
            } else {
                let h = 1 + kids_of(node)
                    .iter()
                    .filter_map(|k| height.get(k))
                    .max()
                    .unwrap_or(&0);
                if h > MAX_NESTING {
                    return Err(format!("nesting deeper than {MAX_NESTING}"));
                }
                height.insert(node, h);
                on_path.pop();
                stack.pop();
            }
        }
    }
    Ok(())
}

fn fontdb() -> Arc<usvg::fontdb::Database> {
    static DB: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = usvg::fontdb::Database::new();
        db.load_font_data(FONT_REGULAR.to_vec());
        db.load_font_data(FONT_BOLD.to_vec());
        // Every generic family (and usvg's last-resort serif) resolves to
        // the bundled face.
        db.set_serif_family(FONT_FAMILY);
        db.set_sans_serif_family(FONT_FAMILY);
        db.set_monospace_family(FONT_FAMILY);
        db.set_cursive_family(FONT_FAMILY);
        db.set_fantasy_family(FONT_FAMILY);
        Arc::new(db)
    })
    .clone()
}

/// Scale and integer raster size for content `w`×`h` (laid out at
/// [`BASE_FONT_PX`]).
// The capped values are far below the precision limits of f32 / f64.
#[allow(clippy::cast_precision_loss)]
fn fit(w: f32, h: f32, max_width_px: u32, font_px: f32) -> (f32, u32, u32) {
    let max_w = max_width_px.min(MAX_WIDTH_PX) as f32;
    let mut scale = font_px / BASE_FONT_PX;
    if w * scale > max_w {
        scale = max_w / w;
    }
    if h * scale > MAX_HEIGHT_PX as f32 {
        scale = MAX_HEIGHT_PX as f32 / h;
    }
    let pixels = f64::from(w * scale) * f64::from(h * scale);
    if pixels > MAX_PIXELS as f64 {
        // A hair under the cap so rounding up below cannot cross it.
        scale *= ((MAX_PIXELS as f64 / pixels).sqrt() * 0.999) as f32;
    }
    // Round up (labels keep their size) but never past a cap.
    let pw = ((w * scale).ceil() as u32).clamp(1, max_w as u32);
    let ph = ((h * scale).ceil() as u32).clamp(1, MAX_HEIGHT_PX);
    (scale, pw, ph)
}

fn rasterize(svg: &str, max_width_px: u32, font_px: f32) -> Result<Raster, String> {
    let opts = usvg::Options {
        font_family: FONT_FAMILY.to_owned(),
        fontdb: fontdb(),
        ..usvg::Options::default()
    };
    let tree = usvg::Tree::from_str(svg, &opts).map_err(|e| format!("invalid svg: {e}"))?;
    let size = tree.size();
    // Content can spill past the renderer's view box (e.g. a sequence note
    // right of the last participant): grow the canvas to cover it.
    let (mut x0, mut y0, mut x1, mut y1) = (0.0_f32, 0.0_f32, size.width(), size.height());
    let bbox = tree.root().abs_stroke_bounding_box();
    if bbox.width() > 0.0 && bbox.height() > 0.0 {
        x0 = x0.min(bbox.left().floor());
        y0 = y0.min(bbox.top().floor());
        x1 = x1.max(bbox.right().ceil());
        y1 = y1.max(bbox.bottom().ceil());
    }
    let (w, h) = (x1 - x0, y1 - y0);
    if !(w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0) {
        return Err("diagram has no size".into());
    }
    let (scale, pw, ph) = fit(w, h, max_width_px, font_px);
    if scale * BASE_FONT_PX < MIN_LABEL_PX.min(font_px) {
        return Err(format!(
            "diagram too large to show legibly ({w:.0}x{h:.0} at {max_width_px} px wide)"
        ));
    }
    let mut pixmap = Pixmap::new(pw, ph).ok_or("cannot allocate raster")?;
    resvg::render(
        &tree,
        Transform::from_translate(-x0, -y0).post_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let mut pixels = Vec::with_capacity(pw as usize * ph as usize * 4);
    for px in pixmap.pixels() {
        let c = px.demultiply();
        pixels.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    if pixels.as_chunks::<4>().0.iter().all(|p| p[3] == 0) {
        return Err("diagram rendered empty".into());
    }
    Ok(Raster {
        width: pw,
        height: ph,
        pixels,
        baseline: None,
    })
}

// --- theme -------------------------------------------------------------

fn hex(c: Rgba) -> String {
    format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
}

fn owned(c: Rgba) -> Cow<'static, str> {
    Cow::Owned(hex(c))
}

/// `a` blended towards `b` by `t` (0 = a, 1 = b), opaque.
fn mix(a: Rgba, b: Rgba, t: f32) -> Rgba {
    let ch = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    [ch(a[0], b[0]), ch(a[1], b[1]), ch(a[2], b[2]), 0xff]
}

/// Relative luminance (sRGB, WCAG).
fn luminance(c: Rgba) -> f32 {
    let lin = |v: u8| {
        let v = f32::from(v) / 255.0;
        if v <= 0.040_45 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(c[0]) + 0.7152 * lin(c[1]) + 0.0722 * lin(c[2])
}

/// WCAG contrast ratio (1–21).
fn contrast(a: Rgba, b: Rgba) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// Colours derived from the terminal theme.
struct Palette {
    fg: Rgba,
    bg: Rgba,
    /// Lines and arrows.
    line: Rgba,
    muted: Rgba,
    /// Node fill and border.
    fill: Rgba,
    stroke: Rgba,
    note_fill: Rgba,
    note_stroke: Rgba,
    /// Accents (blue, green, yellow, magenta, cyan, red), visible on bg.
    accents: [Rgba; 6],
    red: Rgba,
}

impl Palette {
    fn new(t: &Theme) -> Self {
        let (fg, bg) = (t.fg, t.bg);
        let dark = luminance(bg) < luminance(fg);
        // Bright variants read better on dark backgrounds, normal ones on
        // light. Accents too close to bg (yellow on white, black on black)
        // are pulled towards fg until they stand out.
        let accent = |i: usize| {
            let c = if dark { t.ansi[i + 8] } else { t.ansi[i] };
            let mut s = 0.0;
            loop {
                let v = mix(c, fg, s);
                if s >= 1.0 || contrast(v, bg) >= 2.0 {
                    return v;
                }
                s += 0.1;
            }
        };
        let accents = [
            accent(4),
            accent(2),
            accent(3),
            accent(5),
            accent(6),
            accent(1),
        ];
        let tint = if dark { 0.22 } else { 0.16 };
        let blue = accents[0];
        let yellow = accents[2];
        Self {
            fg,
            bg,
            line: mix(bg, fg, 0.75),
            muted: mix(bg, fg, 0.6),
            fill: readable_tint(bg, blue, fg, tint),
            stroke: blue,
            note_fill: readable_tint(bg, yellow, fg, tint),
            note_stroke: yellow,
            accents,
            red: accents[5],
        }
    }

    /// Categorical fills for `n` series, readable under fg labels.
    fn series(&self, n: usize, strength: f32) -> Cow<'static, [Cow<'static, str>]> {
        let v: Vec<Cow<'static, str>> = (0..n)
            .map(|i| {
                let base = self.accents[i % self.accents.len()];
                let s = if i < self.accents.len() {
                    strength
                } else {
                    strength * 0.6
                };
                owned(readable_tint_with(self.bg, base, self.fg, s, 3.0))
            })
            .collect();
        Cow::Owned(v)
    }

    fn mermaid_theme(&self) -> mermaid_svg::Theme {
        let (fg, bg) = (self.fg, self.bg);
        let quadrant = |s: f32| owned(readable_tint(bg, self.stroke, fg, s));
        mermaid_svg::Theme {
            fg: owned(fg),
            fg_muted: owned(self.muted),
            bg: owned(bg),
            actor_fill: owned(self.fill),
            actor_stroke: owned(self.stroke),
            actor_text_color: None,
            lifeline: owned(mix(bg, fg, 0.45)),
            arrow_stroke: owned(self.line),
            signal_text_color: None,
            note_fill: owned(self.note_fill),
            note_stroke: owned(self.note_stroke),
            activation_fill: owned(mix(bg, fg, 0.15)),
            activation_stroke: owned(mix(bg, fg, 0.5)),
            frame_label_fill: owned(self.fill),
            title_color: None,
            flow_node_fill: owned(self.fill),
            flow_node_stroke: owned(self.stroke),
            flow_edge_stroke: owned(self.line),
            flow_label_bg: owned(bg),
            flow_cluster_fill: owned(mix(bg, fg, 0.05)),
            flow_cluster_stroke: owned(mix(bg, fg, 0.35)),
            cscale_palette: self.series(12, 0.45),
            pie_palette: self.series(12, 0.6),
            git_palette: self.series(8, 0.7),
            xychart_palette: Cow::Owned(self.accents.iter().map(|&c| owned(c)).collect()),
            pie_stroke: Some(owned(bg)),
            pie_opacity: None,
            commit_label_color: None,
            tag_label_color: None,
            quadrant_fills: [None, None, None, None],
            quadrant_default_fills: [
                quadrant(0.30),
                quadrant(0.22),
                quadrant(0.14),
                quadrant(0.08),
            ],
            font_family: Cow::Borrowed(FONT_FAMILY),
            font_size: f64::from(BASE_FONT_PX),
            responsive: false,
        }
    }

    /// Replace colours the renderer hard-codes (Mermaid default-theme
    /// literals) with theme colours. On shapes, white means "knock-out"
    /// (→ bg) and black / #333 means "ink" (→ fg); on text, white means
    /// "light text on a dark fill" and keeps that role (→ the lighter of
    /// fg / bg), dark means the opposite.
    fn remap_literals(&self, svg: &str, gantt: bool) -> String {
        let (fg, bg) = (self.fg, self.bg);
        let (light, dark) = if luminance(fg) > luminance(bg) {
            (fg, bg)
        } else {
            (bg, fg)
        };
        let blue = self.stroke;
        let mut shape: Vec<(&str, Rgba)> = vec![
            ("#fff", bg),
            ("#ffffff", bg),
            ("white", bg),
            ("#333", fg),
            ("#333333", fg),
            ("black", fg),
            ("#999", self.muted),
            // Default-theme note colours (state notes).
            ("#fff5ad", self.note_fill),
            ("#aaaa33", self.note_stroke),
        ];
        let mut text: Vec<(&str, Rgba)> = vec![
            ("#fff", light),
            ("#ffffff", light),
            ("white", light),
            ("#333", dark),
            ("#333333", dark),
            ("black", dark),
            ("#999", self.muted),
        ];
        if gantt {
            shape.extend([
                // Light section bands (white at 20 %) → faint fg bands.
                ("#ffffff", mix(bg, fg, 0.5)),
                ("#6666ff", blue),
                ("#fff400", self.accents[2]),
                // Task bars: normal, active, done, crit.
                ("#8a90dd", blue),
                ("#534fbc", mix(blue, fg, 0.35)),
                ("#bfc7ff", readable_tint(bg, blue, fg, 0.45)),
                ("#d3d3d3", mix(bg, fg, 0.3)),
                ("#808080", mix(bg, fg, 0.55)),
                ("red", self.red),
                ("#ff8888", mix(self.red, fg, 0.35)),
            ]);
            // Text inside a normal bar was white on purple: use whichever
            // of bg / fg reads better on the new bar colour. Active and
            // done bars get fills that suit fg.
            let ink = if contrast(fg, blue) > contrast(bg, blue) {
                fg
            } else {
                bg
            };
            text = vec![("#fff", ink), ("#333", fg)];
        }
        // Earlier entries win, so put the gantt overrides first.
        shape.reverse();

        let mut out = String::with_capacity(svg.len());
        let mut rest = svg;
        while let Some(lt) = rest.find('<') {
            out.push_str(&rest[..lt]);
            rest = &rest[lt..];
            let end = rest.find('>').map_or(rest.len(), |i| i + 1);
            let tag = &rest[..end];
            let name = tag[1..]
                .split(|c: char| c.is_ascii_whitespace() || c == '>' || c == '/')
                .next()
                .unwrap_or("");
            let map = if matches!(name, "text" | "tspan") {
                &text
            } else {
                &shape
            };
            remap_tag(tag, map, &mut out);
            rest = &rest[end..];
        }
        out.push_str(rest);
        out
    }
}

/// Copy `tag` to `out`, replacing colour attribute values found in `map`.
fn remap_tag(tag: &str, map: &[(&str, Rgba)], out: &mut String) {
    let mut rest = tag;
    while let Some(i) = rest.find("=\"") {
        let (head, tail) = rest.split_at(i + 2);
        out.push_str(head);
        let is_colour = ["fill", "stroke", "stop-color", "color"]
            .iter()
            .any(|a| head[..i].ends_with(a));
        let Some(end) = tail.find('"') else {
            rest = tail;
            break;
        };
        let value = &tail[..end];
        match map
            .iter()
            .find(|(k, _)| is_colour && value.eq_ignore_ascii_case(k))
        {
            Some((_, c)) => out.push_str(&hex(*c)),
            None => out.push_str(value),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
}

/// `accent` blended into `bg` by `strength`, backed off until `fg` text
/// on it keeps a 4.5:1 contrast (WCAG AA).
fn readable_tint(bg: Rgba, accent: Rgba, fg: Rgba, strength: f32) -> Rgba {
    readable_tint_with(bg, accent, fg, strength, 4.5)
}

/// [`readable_tint`] with an explicit minimum contrast.
fn readable_tint_with(bg: Rgba, accent: Rgba, fg: Rgba, strength: f32, min: f32) -> Rgba {
    let mut s = strength;
    loop {
        let c = mix(bg, accent, s);
        if s <= 0.0 || contrast(fg, c) >= min {
            return c;
        }
        s -= 0.05;
    }
}
#[cfg(test)]
#[allow(clippy::cast_precision_loss, clippy::many_single_char_names)]
mod tests {
    use std::path::PathBuf;
    use std::time::Instant;

    use super::*;

    const FLOWCHART: &str = "flowchart TD
    A[Start] --> B{Is it working?}
    B -->|Yes| C[Ship it]
    B -->|No| D[Debug]
    D --> B
    C --> E((Done))
";
    const SEQUENCE: &str = "sequenceDiagram
    participant A as Alice
    participant B as Bob
    A->>B: Hello Bob, how are you?
    B-->>A: Great!
    A-)B: See you later
    Note right of B: Bob thinks
";
    const CLASS: &str = "classDiagram
    class Animal {
        +String name
        +int age
        +makeSound() void
    }
    Animal <|-- Dog
    Animal <|-- Cat
";
    const STATE: &str = "stateDiagram-v2
    [*] --> Idle
    Idle --> Running: start
    Running --> Idle: stop
    Running --> [*]
";
    const ER: &str = "erDiagram
    CUSTOMER ||--o{ ORDER : places
    ORDER ||--|{ LINE_ITEM : contains
    CUSTOMER {
        string name
        int id PK
    }
";
    const GANTT: &str = "gantt
    title Plan
    dateFormat YYYY-MM-DD
    section Build
    Design :a1, 2024-01-01, 7d
    Code :after a1, 10d
    section Test
    QA :2024-01-15, 5d
";
    const PIE: &str = "pie title Pets
    \"Dogs\" : 386
    \"Cats\" : 85
    \"Rats\" : 15
";
    const SAMPLES: &[(&str, &str)] = &[
        ("flowchart", FLOWCHART),
        ("sequence", SEQUENCE),
        ("class", CLASS),
        ("state", STATE),
        ("er", ER),
        ("gantt", GANTT),
        ("pie", PIE),
    ];

    const DARK_BG: Rgba = [0x14, 0x14, 0x18, 0xff];

    fn dark() -> Theme {
        Theme::default()
    }

    /// A light theme (Tomorrow-ish palette on near-white).
    fn light() -> Theme {
        Theme {
            fg: [0x1f, 0x23, 0x28, 0xff],
            bg: [0xfa, 0xfa, 0xf8, 0xff],
            ansi: [
                [0x00, 0x00, 0x00, 0xff],
                [0xc8, 0x28, 0x29, 0xff],
                [0x71, 0x8c, 0x00, 0xff],
                [0xea, 0xb7, 0x00, 0xff],
                [0x42, 0x71, 0xae, 0xff],
                [0x89, 0x59, 0xa8, 0xff],
                [0x3e, 0x99, 0x9f, 0xff],
                [0xd6, 0xd6, 0xd6, 0xff],
                [0x8e, 0x90, 0x8c, 0xff],
                [0xc8, 0x28, 0x29, 0xff],
                [0x71, 0x8c, 0x00, 0xff],
                [0xea, 0xb7, 0x00, 0xff],
                [0x42, 0x71, 0xae, 0xff],
                [0x89, 0x59, 0xa8, 0xff],
                [0x3e, 0x99, 0x9f, 0xff],
                [0xff, 0xff, 0xff, 0xff],
            ],
        }
    }

    fn out_dir() -> PathBuf {
        let dir = std::env::temp_dir().join("md-mermaid");
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// Composite a straight-alpha raster over `bg` (what the terminal
    /// shows).
    fn over(r: &Raster, bg: Rgba) -> Vec<u8> {
        let mut out = Vec::with_capacity(r.pixels.len());
        for p in r.pixels.as_chunks::<4>().0 {
            let a = u32::from(p[3]);
            for c in 0..3 {
                let v = (u32::from(p[c]) * a + u32::from(bg[c]) * (255 - a) + 127) / 255;
                out.push(v as u8);
            }
            out.push(0xff);
        }
        out
    }

    /// Save the raster composited over the theme background, for eyeballing.
    fn save(name: &str, r: &Raster, bg: Rgba) {
        let path = out_dir().join(format!("{name}.png"));
        let _ = image::save_buffer(
            &path,
            &over(r, bg),
            r.width,
            r.height,
            image::ExtendedColorType::Rgba8,
        );
    }

    fn render(src: &str, theme: &Theme) -> Raster {
        render_mermaid(src, 1600, 16.0, theme).unwrap_or_else(|e| panic!("render failed: {e}"))
    }

    fn opaque_count(r: &Raster) -> usize {
        r.pixels
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[3] > 0)
            .count()
    }

    fn close(a: &[u8], b: Rgba, tol: u8) -> bool {
        (0..3).all(|i| a[i].abs_diff(b[i]) <= tol)
    }

    /// Fraction of opaque pixels whose colour is within `tol` of `c`.
    fn share_of(r: &Raster, c: Rgba, tol: u8) -> f64 {
        let opaque = opaque_count(r).max(1);
        let n = r
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[3] == 255 && close(&p[..], c, tol))
            .count();
        n as f64 / opaque as f64
    }

    #[test]
    fn every_supported_type_renders() {
        for (name, src) in SAMPLES {
            for (tname, theme) in [("dark", dark()), ("light", light())] {
                let t = Instant::now();
                let r = render(src, &theme);
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                eprintln!("{name}/{tname}: {}x{} in {ms:.1} ms", r.width, r.height);
                assert!(
                    r.width > 50 && r.height > 30,
                    "{name}: tiny {}x{}",
                    r.width,
                    r.height
                );
                assert_eq!(r.pixels.len(), (r.width * r.height * 4) as usize);
                assert_eq!(r.baseline, None);
                let opaque = opaque_count(&r);
                assert!(opaque > 500, "{name}: almost nothing drawn ({opaque} px)");
                assert!(
                    opaque < r.pixels.len() / 4,
                    "{name}: background not transparent"
                );
                // fg (text / lines) appears.
                assert!(
                    share_of(&r, theme.fg, 40) > 0.0,
                    "{name}/{tname}: no fg-coloured pixels"
                );
                save(&format!("{name}-{tname}"), &r, theme.bg);
                let svg = render_svg(src, &theme).unwrap();
                let _ = std::fs::write(out_dir().join(format!("{name}-{tname}.svg")), svg);
            }
        }
    }

    #[test]
    fn corners_are_transparent() {
        for (name, src) in SAMPLES {
            let r = render(src, &dark());
            let w = r.width as usize;
            let h = r.height as usize;
            let alpha = |x: usize, y: usize| r.pixels[(y * w + x) * 4 + 3];
            assert_eq!(alpha(0, 0), 0, "{name}: top-left corner painted");
            assert_eq!(
                alpha(w - 1, h - 1),
                0,
                "{name}: bottom-right corner painted"
            );
        }
    }

    #[test]
    fn dark_and_light_differ_and_use_theme_colours() {
        for (name, src) in SAMPLES {
            let d = render(src, &dark());
            let l = render(src, &light());
            assert_ne!(
                d.pixels, l.pixels,
                "{name}: themes produced identical pixels"
            );
        }
        // Flowchart node fill is the blue accent blended into bg.
        let d = render(FLOWCHART, &dark());
        let t = dark();
        let fill = readable_tint(t.bg, t.ansi[12], t.fg, 0.22);
        assert!(share_of(&d, fill, 3) > 0.2, "dark node fill missing");
        let l = render(FLOWCHART, &light());
        let t = light();
        let fill = readable_tint(t.bg, t.ansi[4], t.fg, 0.16);
        assert!(share_of(&l, fill, 3) > 0.2, "light node fill missing");
        // Mermaid's default dark text and white fills never appear.
        for (name, src) in SAMPLES {
            let d = render(src, &dark());
            // (Antialiased edges may hit a stray pixel or two.)
            assert!(
                share_of(&d, [0x33, 0x33, 0x33, 0xff], 4) < 0.002,
                "{name}: #333"
            );
            assert!(
                share_of(&d, [0xff, 0xff, 0xff, 0xff], 4) < 0.002,
                "{name}: white"
            );
        }
    }

    fn parse_hex(s: &str) -> Rgba {
        let v = u32::from_str_radix(&s[1..], 16).unwrap();
        [(v >> 16) as u8, (v >> 8) as u8, v as u8, 0xff]
    }

    #[test]
    fn theme_contrast() {
        for t in [dark(), light()] {
            let m = Palette::new(&t).mermaid_theme();
            assert_eq!(m.fg, hex(t.fg));
            assert_eq!(m.bg, hex(t.bg));
            assert!(!m.responsive);
            // Labels on node fills, notes and categorical fills stay readable.
            for fill in [
                &m.flow_node_fill,
                &m.actor_fill,
                &m.note_fill,
                &m.frame_label_fill,
            ]
            .into_iter()
            .chain(m.quadrant_default_fills.iter())
            {
                let c = contrast(t.fg, parse_hex(fill));
                assert!(c >= 4.5, "fg on {fill}: contrast {c:.2}");
            }
            for fill in m.pie_palette.iter().chain(m.cscale_palette.iter()) {
                let c = contrast(t.fg, parse_hex(fill));
                assert!(c >= 3.0, "fg on {fill}: contrast {c:.2}");
            }
            // Accent strokes stand out from the background.
            for c in Palette::new(&t).accents {
                assert!(contrast(c, t.bg) >= 2.0, "{} on bg", hex(c));
            }
            for line in [&m.flow_edge_stroke, &m.arrow_stroke, &m.flow_node_stroke] {
                let c = contrast(parse_hex(line), t.bg);
                assert!(c >= 3.0, "{line} on bg: contrast {c:.2}");
            }
        }
    }

    #[test]
    fn invisible_accent_falls_back() {
        let mut t = dark();
        t.ansi[12] = t.bg;
        let p = Palette::new(&t);
        assert!(contrast(p.stroke, t.bg) >= 2.0);
    }

    #[test]
    fn readable_tint_backs_off() {
        let (bg, fg) = ([0x14, 0x14, 0x18, 0xff], [0xd8, 0xd8, 0xd8, 0xff]);
        let c = readable_tint(bg, [0xff, 0xff, 0xff, 0xff], fg, 0.9);
        assert!(contrast(fg, c) >= 4.5);
        assert_ne!(c, bg);
    }

    #[test]
    fn remap_only_touches_colour_attributes() {
        let p = Palette::new(&dark());
        let bg = hex(dark().bg);
        let fg = hex(dark().fg);
        let svg = r##"<svg><circle fill="#fff" stroke="black"/><text id="white" fill="#ffffff">white</text><rect fill="#123456"/></svg>"##;
        let out = p.remap_literals(svg, false);
        // Shapes: white = knock-out (bg); text: white = light (fg on a dark theme).
        assert_eq!(
            out,
            format!(
                r##"<svg><circle fill="{bg}" stroke="{fg}"/><text id="white" fill="{fg}">white</text><rect fill="#123456"/></svg>"##
            )
        );
        let lp = Palette::new(&light());
        let out = lp.remap_literals(
            r##"<text fill="#fff">x</text><text fill="#333">y</text>"##,
            false,
        );
        assert_eq!(
            out,
            format!(
                r#"<text fill="{}">x</text><text fill="{}">y</text>"#,
                hex(light().bg),
                hex(light().fg)
            )
        );
        // Gantt-only literals are left alone elsewhere.
        assert!(
            p.remap_literals(r##"<rect fill="#8a90dd"/>"##, false)
                .contains("#8a90dd")
        );
        assert!(
            !p.remap_literals(r##"<rect fill="#8a90dd"/>"##, true)
                .contains("#8a90dd")
        );
    }

    #[test]
    fn straight_alpha_output() {
        let r = render(FLOWCHART, &dark());
        // Antialiased edges of light lines: with straight alpha the colour
        // channels exceed alpha (premultiplied would cap them at alpha).
        let edge = r
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[3] > 0 && p[3] < 128)
            .filter(|p| p[0].max(p[1]).max(p[2]) > p[3])
            .count();
        assert!(edge > 50, "no straight-alpha edge pixels ({edge})");
    }

    #[test]
    fn font_px_scales_output() {
        let a = render_mermaid(FLOWCHART, 4096, 16.0, &dark()).unwrap();
        let b = render_mermaid(FLOWCHART, 4096, 32.0, &dark()).unwrap();
        let ratio = f64::from(b.width) / f64::from(a.width);
        assert!((1.95..=2.05).contains(&ratio), "ratio {ratio}");
        let ratio = f64::from(b.height) / f64::from(a.height);
        assert!((1.95..=2.05).contains(&ratio), "ratio {ratio}");
    }

    #[test]
    fn label_text_height_tracks_font_px() {
        // A lone node: the label's ink height is a fraction of font_px
        // (cap height ≈ 0.7 em for Noto Sans).
        for font_px in [12.0_f32, 20.0, 28.0] {
            let r = render_mermaid("flowchart LR\n  A[HHHH]\n", 4096, font_px, &dark()).unwrap();
            let fg = dark().fg;
            let w = r.width as usize;
            let rows: Vec<usize> = (0..r.height as usize)
                .filter(|&y| {
                    (0..w).any(|x| {
                        let p = &r.pixels[(y * w + x) * 4..][..4];
                        p[3] == 255 && close(p, fg, 30)
                    })
                })
                .collect();
            let ink = (rows.last().unwrap() - rows.first().unwrap() + 1) as f32;
            let em = ink / 0.714;
            assert!(
                (em - font_px).abs() <= font_px * 0.15 + 1.0,
                "font_px {font_px}: label em ≈ {em}"
            );
        }
    }

    #[test]
    fn width_cap_respected() {
        let wide = "flowchart LR\n  A[A fairly long first label] --> B[Another long label] --> C[Yet another label here] --> D[And the final long label]\n";
        let natural = render_mermaid(wide, 4096, 16.0, &dark()).unwrap();
        assert!(natural.width > 300, "natural width {}", natural.width);
        for cap in [natural.width - 1, 800, 600, 400] {
            let r = render_mermaid(wide, cap, 16.0, &dark()).unwrap();
            assert!(r.width <= cap, "width {} > cap {cap}", r.width);
            assert!(r.width + 1 >= cap, "width {} far below cap {cap}", r.width);
            // Aspect ratio kept.
            let ratio = |r: &Raster| f64::from(r.width) / f64::from(r.height);
            assert!((ratio(&r) - ratio(&natural)).abs() / ratio(&natural) < 0.05);
        }
        // So narrow that labels would be specks: an error, not a smear.
        let e = render_mermaid(wide, 100, 16.0, &dark()).unwrap_err();
        assert!(e.contains("legibly"), "{e}");
        // Never scaled up past the natural size.
        let r = render_mermaid(FLOWCHART, 4096, 16.0, &dark()).unwrap();
        let r2 = render_mermaid(FLOWCHART, 10_000, 16.0, &dark()).unwrap();
        assert_eq!(r.width, r2.width);
        // Hard cap regardless of max_width_px.
        let r = render_mermaid(wide, u32::MAX, 200.0, &dark()).unwrap();
        assert!(r.width <= MAX_WIDTH_PX);
    }

    #[test]
    fn height_cap_respected() {
        use std::fmt::Write as _;
        let mut src = String::from("sequenceDiagram\n  participant A\n  participant B\n");
        for i in 0..400 {
            let _ = writeln!(src, "  A->>B: message {i}");
        }
        let t = Instant::now();
        let r = render_mermaid(&src, 4096, 16.0, &dark()).unwrap();
        eprintln!(
            "400-message sequence: {}x{} in {:?}",
            r.width,
            r.height,
            t.elapsed()
        );
        assert!(r.height <= MAX_HEIGHT_PX);
        assert!(
            r.height >= MAX_HEIGHT_PX - 1,
            "expected scale-down, got {}",
            r.height
        );
    }

    #[test]
    fn fit_caps() {
        // Natural.
        assert_eq!(fit(100.0, 50.0, 1000, BASE_FONT_PX), (1.0, 100, 50));
        // font_px scaling.
        assert_eq!(fit(100.0, 50.0, 1000, 2.0 * BASE_FONT_PX), (2.0, 200, 100));
        // Width.
        let (s, w, h) = fit(1000.0, 100.0, 500, BASE_FONT_PX);
        assert!((s - 0.5).abs() < 1e-6 && w == 500 && h == 50);
        // Height.
        let (_, w, h) = fit(100.0, 20_000.0, 4096, 16.0);
        assert!(h <= MAX_HEIGHT_PX && w <= 41);
        // Pixels.
        let (_, w, h) = fit(4096.0, 8192.0, 4096, 16.0);
        assert!(u64::from(w) * u64::from(h) <= MAX_PIXELS, "{w}x{h}");
        // Degenerate sizes still give at least one pixel.
        let (_, w, h) = fit(10_000.0, 0.01, 4096, 16.0);
        assert!(w >= 1 && h >= 1);
    }

    #[test]
    fn invalid_source_is_err() {
        for src in [
            "",
            "   \n",
            "this is not mermaid",
            "flowchart TD\n  A --> \n  ((((\n",
            "flowchart TD\n  A[unclosed --> B\n",
            "pie\n  \"a\" : not-a-number\n",
        ] {
            let r = render_mermaid(src, 800, 16.0, &dark());
            assert!(
                r.is_err(),
                "{src:?} rendered: {:?}",
                r.map(|r| (r.width, r.height))
            );
        }
    }

    #[test]
    fn bad_parameters_are_err() {
        assert!(render_mermaid(FLOWCHART, 0, 16.0, &dark()).is_err());
        for f in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            assert!(
                render_mermaid(FLOWCHART, 800, f, &dark()).is_err(),
                "font_px {f}"
            );
        }
    }

    #[test]
    fn oversize_source_is_err() {
        let mut src = String::from("flowchart TD\n");
        while src.len() <= MAX_SOURCE_BYTES {
            src.push_str("  %% padding comment to grow the source\n");
        }
        let e = render_mermaid(&src, 800, 16.0, &dark()).unwrap_err();
        assert!(e.contains("bytes"), "{e}");
    }

    #[test]
    fn huge_mindmap_is_err() {
        use std::fmt::Write as _;
        let mut src = String::from("mindmap\n  root\n");
        for i in 0..MAX_NODES {
            let _ = writeln!(src, "    n{i}");
        }
        let e = render_mermaid(&src, 800, 16.0, &dark()).unwrap_err();
        assert!(e.contains("nodes"), "{e}");
    }

    #[test]
    fn too_many_nodes_is_err() {
        use std::fmt::Write as _;
        let mut src = String::from("flowchart TD\n");
        for i in 0..=MAX_NODES {
            let _ = writeln!(src, "  n{i}");
        }
        let e = render_mermaid(&src, 800, 16.0, &dark()).unwrap_err();
        assert!(e.contains("nodes"), "{e}");

        let mut src = String::from("flowchart TD\n");
        for i in 0..40 {
            for j in 0..30 {
                let _ = writeln!(src, "  a{i} --> b{j}");
            }
        }
        let e = render_mermaid(&src, 800, 16.0, &dark()).unwrap_err();
        assert!(e.contains("edges"), "{e}");
    }

    #[test]
    fn large_diagrams_finish_quickly() {
        use std::fmt::Write as _;
        // A chain at the node limit and a dense graph under the edge limit.
        let mut chain = String::from("flowchart TD\n");
        for i in 0..MAX_NODES - 1 {
            let _ = writeln!(chain, "  n{i}[Node {i}] --> n{}[Node {}]", i + 1, i + 1);
        }
        let mut dense = String::from("flowchart LR\n");
        for i in 0..30 {
            for j in 0..30 {
                let _ = writeln!(dense, "  a{i} --> b{j}");
            }
        }
        let mut gantt = String::from("gantt\n  dateFormat YYYY-MM-DD\n  section S\n");
        let _ = writeln!(gantt, "  Long :2000-01-01, 36500d");
        for (name, src) in [
            ("chain", &chain),
            ("dense", &dense),
            ("gantt-century", &gantt),
        ] {
            let t = Instant::now();
            let r = render_mermaid(src, 1600, 16.0, &dark());
            let el = t.elapsed();
            eprintln!(
                "{name}: {:?} in {el:?}",
                r.as_ref().map(|r| (r.width, r.height))
            );
            // Generous bound for slow debug builds on shared CI.
            assert!(el.as_secs() < 60, "{name} took {el:?}");
        }
    }

    #[test]
    fn containment_cycles_are_err() {
        // Each of these overflowed the renderer's stack (uncatchable).
        for src in [
            "stateDiagram-v2\n  state S {\n    S\n  }\n",
            "stateDiagram-v2\n  state S {\n    S --> X\n  }\n",
            "stateDiagram-v2\n  state A {\n    B\n  }\n  state B {\n    A\n  }\n",
            "stateDiagram-v2\n  state A {\n    state B {\n      C\n    }\n  }\n  state C {\n    A\n  }\n",
        ] {
            let e = render_mermaid(src, 800, 16.0, &dark()).unwrap_err();
            assert!(e.contains("inside itself"), "{src:?}: {e}");
        }
    }

    #[test]
    fn nesting_depth_limited() {
        use std::fmt::Write as _;
        let nested = |depth: usize| {
            let mut s = String::from("stateDiagram-v2\n");
            for i in 0..depth {
                let _ = writeln!(s, "{:w$}state S{i} {{", "", w = i + 1);
            }
            let _ = writeln!(s, "{:w$}leaf", "", w = depth + 1);
            for i in (0..depth).rev() {
                let _ = writeln!(s, "{:w$}}}", "", w = i + 1);
            }
            s
        };
        assert!(render_mermaid(&nested(4), 800, 16.0, &dark()).is_ok());
        let e = render_mermaid(&nested(MAX_NESTING + 1), 800, 16.0, &dark()).unwrap_err();
        assert!(e.contains("nesting"), "{e}");
        let mut flow = String::from("flowchart TD\n");
        for i in 0..=MAX_NESTING {
            let _ = writeln!(flow, "subgraph G{i}");
        }
        flow.push_str("x\n");
        for _ in 0..=MAX_NESTING {
            flow.push_str("end\n");
        }
        let e = render_mermaid(&flow, 800, 16.0, &dark()).unwrap_err();
        assert!(e.contains("nesting"), "{e}");
    }

    #[test]
    fn deep_inputs_fit_a_small_stack() {
        use std::fmt::Write as _;
        // Deep-but-legal inputs on a 2 MiB thread (the std default for
        // spawned threads); a stack overflow would abort the test binary.
        let mut cases = Vec::new();
        let mut s = String::from("stateDiagram-v2\n");
        for i in 0..MAX_NESTING {
            let _ = writeln!(s, "state S{i} {{");
        }
        s.push_str("leaf\n");
        for _ in 0..MAX_NESTING {
            s.push_str("}\n");
        }
        cases.push(s);
        let mut s = String::from("flowchart TD\n");
        for i in 0..MAX_NESTING {
            let _ = writeln!(s, "subgraph G{i}");
        }
        s.push_str("x --> y\n");
        for _ in 0..MAX_NESTING {
            s.push_str("end\n");
        }
        cases.push(s);
        let mut s = String::from("mindmap\n  root\n");
        for i in 0..300 {
            let _ = writeln!(s, "{:w$}n{i}", "", w = i + 3);
        }
        cases.push(s);
        let mut s = String::from("classDiagram\n");
        for i in 0..MAX_NESTING {
            let _ = writeln!(s, "namespace N{i} {{");
        }
        s.push_str("class C\n");
        for _ in 0..MAX_NESTING {
            s.push_str("}\n");
        }
        cases.push(s);
        let mut s = String::from("block-beta\n");
        for i in 0..MAX_NESTING {
            let _ = writeln!(s, "block:B{i}");
        }
        s.push_str("x\n");
        for _ in 0..MAX_NESTING {
            s.push_str("end\n");
        }
        cases.push(s);
        let handle = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || {
                for src in &cases {
                    let _ = render_mermaid(src, 800, 16.0, &dark());
                }
            })
            .unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn gantt_span_limited() {
        let t = Instant::now();
        for src in [
            // Took ~10 s (7-digit year → a two-million-year span).
            "gantt\n  dateFormat YYYY-MM-DD\n  section S\n  T :a, 2022024-01-01, 3d\n  U :4-01-01, 3d\n",
            "gantt\n  dateFormat YYYY-MM-DD\n  section S\n  T :2024-01-01, 99999999d\n",
            "gantt\n  dateFormat YYYY-MM-DD\n  excludes weekends\n  section S\n  T :a, 2024-01-01, 30000d\n  U :after a, 30000d\n",
        ] {
            match render_mermaid(src, 800, 16.0, &dark()) {
                Ok(r) => panic!("{src:?} rendered {}x{}", r.width, r.height),
                Err(e) => assert!(e.contains("gantt"), "{src:?}: {e}"),
            }
        }
        // A century with an explicit daily tick interval stays fast.
        let src = "gantt\n  dateFormat YYYY-MM-DD\n  tickInterval 1day\n  excludes weekends\n  section S\n  T :2000-01-01, 36000d\n";
        let r = render_mermaid(src, 800, 16.0, &dark()).unwrap();
        assert!(r.width <= 800);
        assert!(t.elapsed().as_secs() < 30, "{:?}", t.elapsed());
    }

    #[test]
    fn panics_become_errors() {
        let r: Result<(), String> = guarded(|| panic!("boom"));
        let e = r.unwrap_err();
        assert!(e.contains("panicked") && e.contains("boom"), "{e}");
        let r: Result<(), String> = guarded(|| panic!("{}", String::from("owned")));
        assert!(r.unwrap_err().contains("owned"));
    }

    #[test]
    fn init_directive_cannot_override_theme() {
        let src = "%%{init: {'theme': 'forest', 'themeVariables': {'primaryColor': '#ff0000'}}}%%\nflowchart LR\n  A --> B\n";
        let plain = render("flowchart LR\n  A --> B\n", &dark());
        let r = render(src, &dark());
        assert_eq!(share_of(&r, [0xff, 0, 0, 0xff], 10), 0.0);
        assert_eq!(r, plain);
    }

    #[test]
    fn non_ascii_labels_render() {
        let r = render("flowchart LR\n  A[Grüße — Ωμέγα] --> B[Привет]\n", &dark());
        assert!(opaque_count(&r) > 500);
        save("non-ascii-dark", &r, DARK_BG);
    }

    #[test]
    fn bundled_fonts_only() {
        let db = fontdb();
        assert_eq!(db.len(), 2);
        assert!(
            db.faces()
                .all(|f| f.families.iter().any(|(n, _)| n == FONT_FAMILY))
        );
    }

    #[test]
    fn deterministic() {
        let a = render(SEQUENCE, &dark());
        let b = render(SEQUENCE, &dark());
        assert_eq!(a, b);
    }

    // --- golden snapshots -------------------------------------------------

    fn golden_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/golden")
            .join(format!("mermaid_{name}.png"))
    }

    /// Compare against `tests/golden/mermaid_<name>.png` (straight RGBA).
    /// Set `MD_UPDATE_GOLDEN=1` to (re)write the snapshot. Tolerant of
    /// small antialiasing differences between platforms / SIMD paths.
    fn check_golden(name: &str, r: &Raster) {
        let path = golden_path(name);
        if std::env::var_os("MD_UPDATE_GOLDEN").is_some_and(|v| v == "1") {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            image::save_buffer(
                &path,
                &r.pixels,
                r.width,
                r.height,
                image::ExtendedColorType::Rgba8,
            )
            .unwrap();
            return;
        }
        let golden = image::open(&path)
            .unwrap_or_else(|e| panic!("{}: {e} (run with MD_UPDATE_GOLDEN=1)", path.display()))
            .to_rgba8();
        let actual_path = out_dir().join(format!("golden-actual-{name}.png"));
        let _ = image::save_buffer(
            &actual_path,
            &r.pixels,
            r.width,
            r.height,
            image::ExtendedColorType::Rgba8,
        );
        // Allow ±2 px of size drift from float rounding.
        assert!(
            golden.width().abs_diff(r.width) <= 2 && golden.height().abs_diff(r.height) <= 2,
            "{name}: size {}x{} vs golden {}x{} (actual saved to {})",
            r.width,
            r.height,
            golden.width(),
            golden.height(),
            actual_path.display()
        );
        let w = golden.width().min(r.width);
        let h = golden.height().min(r.height);
        let mut bad = 0u64;
        for y in 0..h {
            for x in 0..w {
                let g = golden.get_pixel(x, y).0;
                let i = ((y * r.width + x) * 4) as usize;
                let a = &r.pixels[i..i + 4];
                // Compare premultiplied so colour noise in near-transparent
                // pixels does not count.
                let pm = |p: &[u8], c: usize| u32::from(p[c]) * u32::from(p[3]) / 255;
                let diff = (0..3)
                    .map(|c| pm(&g, c).abs_diff(pm(a, c)))
                    .chain(std::iter::once(u32::from(g[3]).abs_diff(u32::from(a[3]))))
                    .max()
                    .unwrap_or(0);
                if diff > 24 {
                    bad += 1;
                }
            }
        }
        let total = u64::from(w) * u64::from(h);
        assert!(
            bad * 100 <= total,
            "{name}: {bad}/{total} pixels differ from golden (actual saved to {})",
            actual_path.display()
        );
    }

    #[test]
    fn golden_compare_detects_changes() {
        // The light render must not pass as the dark snapshot. (Skipped
        // while regenerating, when check_golden would overwrite it.)
        if std::env::var_os("MD_UPDATE_GOLDEN").is_some() {
            return;
        }
        let light_render = render(FLOWCHART, &light());
        let r = std::panic::catch_unwind(|| check_golden("flowchart_dark", &light_render));
        assert!(r.is_err(), "tolerance too loose");
    }

    #[test]
    fn golden_flowchart_dark() {
        check_golden("flowchart_dark", &render(FLOWCHART, &dark()));
    }

    #[test]
    fn golden_sequence_dark() {
        check_golden("sequence_dark", &render(SEQUENCE, &dark()));
    }

    #[test]
    fn golden_pie_light() {
        check_golden("pie_light", &render(PIE, &light()));
    }
}
