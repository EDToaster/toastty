//! Styling absent from mermaid-svg's public Theme API.
//!
//! Parse once during immutable scene preparation, then splice only identified
//! attribute values. Never serialize the XML: label bytes, links, metadata,
//! font metrics, and path geometry must remain untouched. Explicit source
//! styles opt the whole diagram out rather than guessing attribute provenance.

use mermaid_svg::ast::{BlockItem, Diagram, SequenceItem};
use roxmltree::Node;
use std::borrow::Cow;

use super::{Palette, Rgba, contrast_ink, hex, mix, readable_tint};

pub(super) fn apply(svg: &str, diagram: &Diagram, palette: &Palette) -> Result<String, String> {
    if has_custom_styles(diagram) {
        return Ok(svg.to_owned());
    }
    let document =
        roxmltree::Document::parse(svg).map_err(|e| format!("invalid diagram SVG: {e}"))?;
    let mut edits = Vec::new();
    let fill = hex(palette.fill);
    let stroke = hex(palette.stroke);
    let line = hex(palette.line);
    let categories = palette.series(12, 0.55);
    for node in document.descendants().filter(Node::is_element) {
        let tag = node.tag_name().name();
        // Rich-label spans can carry author colors independently of AST styles.
        if !matches!(
            tag,
            "rect"
                | "path"
                | "circle"
                | "ellipse"
                | "line"
                | "polygon"
                | "polyline"
                | "text"
                | "g"
                | "stop"
        ) || node.ancestors().any(|n| n.has_tag_name("foreignObject"))
        {
            continue;
        }
        let card = tag == "rect"
            && node.attribute("fill") == Some(fill.as_str())
            && node.attribute("stroke") == Some(stroke.as_str())
            && match diagram {
                Diagram::Sequence(_) => node.attribute("rx") == Some("4"),
                Diagram::Class(_) => node.attribute("rx") == Some("2"),
                _ => false,
            };
        let connector = is_connector(node, diagram, &line);
        for attribute in node.attributes() {
            if attribute.namespace().is_some() {
                continue;
            }
            let (name, value) = (attribute.name(), attribute.value());
            let replacement = match name {
                "fill" | "stroke" | "stop-color" => {
                    if matches!(diagram, Diagram::Radar(_)) && name == "stroke" {
                        radar_stroke(value, palette, &categories)
                    } else {
                        None
                    }
                    .or_else(|| literal_color(node, name, value, diagram, palette))
                    .map(hex)
                }
                "rx" if card => Some("6".to_owned()),
                "stroke-width" if card || connector => Some("1.25".to_owned()),
                "stroke-width" if matches!(diagram, Diagram::Pie(_)) && value == "2" => {
                    Some("1.25".to_owned())
                }
                // A quiet radar plot, not a gray plate. Grid marks are
                // decorative; labels retain the contrast-safe fg_muted token.
                "fill-opacity" if matches!(diagram, Diagram::Radar(_)) && value == "0.12" => {
                    Some("0.025".to_owned())
                }
                "fill-opacity" if matches!(diagram, Diagram::Radar(_)) && value == "0.5" => {
                    Some("0.16".to_owned())
                }
                "filter"
                    if matches!(diagram, Diagram::Mindmap(_)) && value == "url(#mm-shadow)" =>
                {
                    Some("none".to_owned())
                }
                _ => None,
            };
            if let Some(replacement) = replacement.filter(|r| r != value) {
                edits.push((attribute.range_value(), replacement));
            }
        }
        if connector && !node.has_attribute("stroke-linejoin") {
            // Insert before an existing attribute, never search for '>' (it
            // is legal inside quoted values). Marker paths are never touched.
            if let Some(attribute) = node.attributes().next() {
                let at = attribute.range().start;
                edits.push((at..at, "stroke-linejoin=\"round\" ".to_owned()));
            }
        }
    }
    edits.sort_unstable_by_key(|(range, _)| range.start);
    let mut out = String::with_capacity(svg.len());
    let mut cursor = 0;
    for (range, replacement) in edits {
        out.push_str(&svg[cursor..range.start]);
        out.push_str(&replacement);
        cursor = range.end;
    }
    out.push_str(&svg[cursor..]);
    Ok(out)
}

fn is_connector(node: Node<'_, '_>, diagram: &Diagram, line: &str) -> bool {
    !node.ancestors().any(|n| n.has_tag_name("marker"))
        && matches!(
            diagram,
            Diagram::Flowchart(_)
                | Diagram::Sequence(_)
                | Diagram::State(_)
                | Diagram::Class(_)
                | Diagram::Requirement(_)
        )
        && matches!(node.tag_name().name(), "path" | "line")
        && node.attribute("stroke") == Some(line)
        && node.attribute("stroke-width") == Some("1.5")
        && node.attribute("stroke-dasharray").is_none()
        && node.attribute("marker-end").is_some()
}

fn radar_stroke(value: &str, p: &Palette, categories: &[Cow<'static, str>]) -> Option<Rgba> {
    categories
        .iter()
        .position(|c| c.as_ref() == value)
        .map(|i| {
            contrast_ink(
                p.accents[i % 6],
                p.fg,
                &[p.bg, mix(p.bg, p.accents[i % 6], 0.10)],
                3.0,
            )
        })
}

fn literal_color(
    node: Node<'_, '_>,
    attribute: &str,
    value: &str,
    diagram: &Diagram,
    p: &Palette,
) -> Option<Rgba> {
    let text = node.has_tag_name("text");
    let fill = attribute == "fill";
    let value = value.to_ascii_lowercase();
    let tint = |index: usize, strength| readable_tint(p.bg, p.accents[index], p.fg, strength);
    match diagram {
        Diagram::Sequence(_) => {
            let badge = |n: Node<'_, '_>| {
                n.has_tag_name("circle")
                    && n.attribute("r") == Some("9")
                    && n.attribute("fill") == Some(hex(p.line).as_str())
                    && n.attribute("stroke") == Some("none")
            };
            if fill && badge(node) {
                return Some(p.fg);
            }
            if fill && text && value == "#fff" && node.prev_sibling_element().is_some_and(badge) {
                return Some(p.bg);
            }
        }
        Diagram::Gantt(_) => {
            let color = match value.as_str() {
                "#000080" => contrast_ink(p.accents[0], p.fg, &[p.bg], 4.5),
                "#d33" => contrast_ink(p.red, p.fg, &[p.bg], if text { 4.5 } else { 3.0 }),
                "#6666ff" | "#fff400" | "#ffffff" if !text => p.muted,
                "#8a90dd" => tint(0, 0.45),
                "#bfc7ff" => tint(0, 0.25),
                "#534fbc" => contrast_ink(p.accents[0], p.fg, &[p.bg, tint(0, 0.45)], 3.0),
                "#d3d3d3" if fill => p.fill,
                "#d3d3d3" | "#808080" => p.stroke,
                "red" => tint(5, 0.45),
                "#ff8888" => contrast_ink(p.red, p.fg, &[p.bg, tint(5, 0.45)], 3.0),
                _ => return default_color(text, attribute, &value, diagram, p),
            };
            return Some(color);
        }
        Diagram::C4(_) => {
            let color = match value.as_str() {
                "#ffffff" | "#0b2b4a" if text || node.has_tag_name("g") => p.fg,
                "#d9e5f2" | "#3a5a7a" => p.fg,
                // Retain internal/external/type differentiation without the
                // old saturated blue blocks and low-contrast dark outlines.
                "#999999" if fill => p.group_fill,
                "#08427b" if fill => tint(0, 0.16),
                "#1168bd" if fill => tint(0, 0.12),
                "#438dd5" if fill => tint(0, 0.09),
                "#85bbf0" if fill => tint(0, 0.06),
                "#444444" if fill && node.has_tag_name("rect") => p.fill,
                "#6b6b6b" | "#073b6f" | "#0d5ba8" | "#3a7dbe" | "#6fa8dc" | "#2e2e2e" => {
                    contrast_ink(p.stroke, p.fg, &[p.bg, tint(0, 0.16)], 3.0)
                }
                "#444444" if text => p.fg,
                "#444444" => p.line,
                _ => return default_color(text, attribute, &value, diagram, p),
            };
            return Some(color);
        }
        Diagram::Sankey(_) => {
            // Upstream ignores cscale and uses fixed Tableau10 for both nodes
            // and gradient stops. Preserve gradient endpoints and flow widths.
            const TABLEAU: [&str; 10] = [
                "#4e79a7", "#f28e2c", "#e15759", "#76b7b2", "#59a14f", "#edc949", "#af7aa1",
                "#ff9da7", "#9c755f", "#bab0ac",
            ];
            if let Some(index) = TABLEAU.iter().position(|c| *c == value) {
                // Labels may lie over ribbons, not just the canvas. Keep the
                // 4.5:1-readable fill even under the existing translucent
                // gradient; contrasting node outlines carry the boundaries.
                return Some(tint(index % 6, if index < 6 { 0.60 } else { 0.40 }));
            }
        }
        Diagram::GitGraph(_) | Diagram::Mindmap(_) => {
            let pill = node.has_tag_name("rect")
                && node.attribute("rx") == Some("10")
                && node.attribute("ry") == Some("10");
            let mindmap_node = matches!(diagram, Diagram::Mindmap(_))
                && node.attribute("filter") == Some("url(#mm-shadow)");
            if fill
                && (pill || mindmap_node)
                && let Some(index) = p.git_colors().iter().position(|&c| hex(c) == value)
            {
                return Some(tint(index % 6, if index < 6 { 0.60 } else { 0.36 }));
            }
            if !fill && value == "none" && mindmap_node {
                return Some(p.chart_border);
            }
        }
        Diagram::Architecture(_) if value == "#4a72d6" => return Some(p.fill),
        Diagram::Architecture(_)
            if attribute == "stroke"
                && value == "none"
                && node.has_tag_name("rect")
                && node.attribute("fill") == Some("#4a72d6") =>
        {
            return Some(p.stroke);
        }
        _ => {}
    }
    default_color(text, attribute, &value, diagram, p)
}

fn default_color(
    text: bool,
    attribute: &str,
    value: &str,
    diagram: &Diagram,
    p: &Palette,
) -> Option<Rgba> {
    Some(match value {
        "#fff" | "#ffffff" | "white" if text || matches!(diagram, Diagram::Architecture(_)) => p.fg,
        "#fff" | "#ffffff" | "white"
            if attribute == "stroke"
                && matches!(diagram, Diagram::Treemap(_) | Diagram::Sankey(_)) =>
        {
            p.chart_border
        }
        "#fff" | "#ffffff" | "white" => p.bg,
        "#333" | "#333333" | "black" => p.fg,
        "#999" => {
            if text {
                p.muted
            } else {
                p.stroke
            }
        }
        "#666" if matches!(diagram, Diagram::Sequence(_)) => p.stroke,
        "#fff5ad"
            if matches!(
                diagram,
                Diagram::State(_) | Diagram::Class(_) | Diagram::GitGraph(_)
            ) =>
        {
            p.note_fill
        }
        "#aaaa33"
            if matches!(
                diagram,
                Diagram::State(_) | Diagram::Class(_) | Diagram::GitGraph(_)
            ) =>
        {
            p.note_stroke
        }
        "#efefef" if matches!(diagram, Diagram::Packet(_)) => p.fill,
        "#e8e8e8" if matches!(diagram, Diagram::Requirement(_)) => p.bg,
        _ => return None,
    })
}

/// Conservative opt-out: author declarations may intentionally equal a
/// default literal, width, or radius. Their provenance is lost in SVG.
fn has_custom_styles(diagram: &Diagram) -> bool {
    match diagram {
        Diagram::Flowchart(d) => {
            !d.class_defs.is_empty()
                || !d.edge_styles.is_empty()
                || !d.link_style_default.is_empty()
                || d.nodes.iter().any(|n| !n.style.is_empty())
                || d.subgraphs.iter().any(|n| !n.style.is_empty())
        }
        Diagram::Class(d) => {
            !d.class_defs.is_empty() || d.classes.iter().any(|n| !n.style.is_empty())
        }
        Diagram::State(d) => {
            !d.class_defs.is_empty() || d.states.iter().any(|n| !n.style.is_empty())
        }
        Diagram::Er(d) => {
            !d.class_defs.is_empty() || d.entities.iter().any(|n| !n.style.is_empty())
        }
        Diagram::Mindmap(d) => !d.class_defs.is_empty(),
        Diagram::Treemap(d) => !d.class_defs.is_empty(),
        Diagram::Requirement(d) => !d.class_defs.is_empty() || !d.node_styles.is_empty(),
        Diagram::C4(d) => {
            !d.element_styles.is_empty()
                || !d.boundary_styles.is_empty()
                || !d.rel_styles.is_empty()
        }
        Diagram::Quadrant(d) => {
            !d.classes.is_empty()
                || d.points.iter().any(|p| {
                    p.color.is_some() || p.stroke_color.is_some() || p.stroke_width.is_some()
                })
        }
        Diagram::XyChart(d) => !d.plot_color_palette.is_empty(),
        Diagram::Gantt(d) => d.today_marker.as_deref().is_some_and(|s| s != "off"),
        Diagram::Sankey(d) => d
            .link_color
            .as_deref()
            .is_some_and(|c| !matches!(c, "gradient" | "source" | "target")),
        Diagram::Block(d) => {
            if !d.class_defs.is_empty() {
                return true;
            }
            let mut pending: Vec<_> = d.items.iter().collect();
            while let Some(item) = pending.pop() {
                match item {
                    BlockItem::Block(b) if !b.style.is_empty() => return true,
                    BlockItem::Group(g) => pending.extend(&g.items),
                    _ => {}
                }
            }
            false
        }
        Diagram::Sequence(d) => {
            let mut pending: Vec<_> = d.items.iter().collect();
            while let Some(item) = pending.pop() {
                match item {
                    SequenceItem::Box(b) if b.color.is_some() => return true,
                    SequenceItem::Rect(r) => {
                        if r.color.is_some() {
                            return true;
                        }
                        pending.extend(&r.items);
                    }
                    SequenceItem::Alt(branches)
                    | SequenceItem::Par(branches)
                    | SequenceItem::Critical(branches) => {
                        pending.extend(branches.iter().flat_map(|b| &b.items));
                    }
                    SequenceItem::Loop(b) | SequenceItem::Opt(b) | SequenceItem::Break(b) => {
                        pending.extend(&b.items);
                    }
                    _ => {}
                }
            }
            false
        }
        _ => false,
    }
}

#[cfg(test)]
#[path = "style_tests.rs"]
mod tests;
