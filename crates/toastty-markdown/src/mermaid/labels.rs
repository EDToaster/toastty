//! Bounded, font-aware AST label preparation.
//!
//! A profile is chosen once per prepared layout, never per pixel of terminal
//! width. Only fields whose renderer allocates multi-line geometry are
//! rewritten. IDs, graph direction, connectivity, message order and markup
//! remain untouched. Unsupported label fields keep the upstream renderer's
//! behavior; the caller's legibility limit decides when to show source.

use std::sync::OnceLock;

use mermaid_svg::ast::{Diagram, NodeShape, NotePosition, SequenceItem};
use rustybuzz::{Face, UnicodeBuffer};
use unicode_segmentation::UnicodeSegmentation;

use super::{BASE_FONT_PX, FONT_REGULAR};

/// Two stable layouts are enough to accommodate long prose without doing
/// layout continuously as the terminal is resized.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Profile {
    Standard,
    Compact,
}

impl Profile {
    const fn width(self) -> f32 {
        match self {
            Self::Standard => 240.0,
            Self::Compact => 144.0,
        }
    }
}

const MAX_LABEL_GRAPHEMES: usize = 4096;
const MAX_LABEL_LINES: usize = 64;
/// Bound the total input fed to shaping, including repeated candidate lines.
/// This prevents a label containing thousands of combining marks from making
/// an otherwise small AST expensive to prepare.
const MAX_SHAPED_BYTES: usize = 4 * 1024 * 1024;

/// Wrap labels whose upstream layout supports multiple lines.
///
/// Returns whether any display label changed, allowing the caller to avoid
/// preparing an identical compact layout. Rich labels are deliberately kept
/// intact: their HTML/Markdown styles and entity decoding belong to the
/// renderer, not to this plain-text line breaker.
pub(super) fn prepare(diagram: &mut Diagram, profile: Profile) -> Result<bool, String> {
    let mut wrapper = Wrapper {
        shaped_bytes: 0,
        changed: false,
    };
    match diagram {
        Diagram::Flowchart(flow) => {
            for node in &mut flow.nodes {
                let custom_font = node
                    .style
                    .iter()
                    .chain(flow.class_defs.get("default").into_iter().flatten())
                    .chain(
                        node.classes
                            .iter()
                            .filter_map(|class| flow.class_defs.get(class))
                            .flatten(),
                    )
                    .any(|(key, _)| key.starts_with("font"));
                // Tapered/icon/stadium shapes need interior constraints.
                // A tall stadium's caps consume most of its rectangular box.
                // Do not pretend a rectangular label budget makes those safe.
                if matches!(
                    node.shape,
                    NodeShape::Rect
                        | NodeShape::Round
                        | NodeShape::Subroutine
                        | NodeShape::Cylinder
                ) && !custom_font
                {
                    let geometry = if node.shape == NodeShape::Subroutine {
                        LabelBox::Subroutine
                    } else {
                        LabelBox::Flow
                    };
                    wrapper.wrap(&mut node.text, profile.width(), BASE_FONT_PX, geometry)?;
                }
            }
        }
        Diagram::Sequence(sequence) => {
            for participant in &mut sequence.participants {
                wrapper.wrap(
                    &mut participant.display,
                    profile.width(),
                    BASE_FONT_PX,
                    LabelBox::Participant,
                )?;
            }
            // Iterative traversal avoids introducing recursion for nested
            // alt/par/loop/rect blocks in externally supplied diagrams.
            let mut pending: Vec<&mut SequenceItem> = sequence.items.iter_mut().collect();
            while let Some(item) = pending.pop() {
                match item {
                    SequenceItem::Note(note) => {
                        // A note over one participant has the smallest box;
                        // multiple-participant notes can safely use that same
                        // lower bound without depending on graph layout.
                        let width = if note.position == NotePosition::Over {
                            80.0
                        } else {
                            120.0
                        };
                        wrapper.wrap(&mut note.text, width, 12.0, LabelBox::Fixed)?;
                    }
                    SequenceItem::Alt(branches)
                    | SequenceItem::Par(branches)
                    | SequenceItem::Critical(branches) => {
                        for branch in branches {
                            pending.extend(branch.items.iter_mut());
                        }
                    }
                    SequenceItem::Loop(block)
                    | SequenceItem::Opt(block)
                    | SequenceItem::Break(block) => pending.extend(block.items.iter_mut()),
                    SequenceItem::Rect(block) => pending.extend(block.items.iter_mut()),
                    _ => {}
                }
            }
        }
        Diagram::Class(class) => {
            for note in &mut class.notes {
                wrapper.wrap(&mut note.text, profile.width(), 13.0, LabelBox::ClassNote)?;
            }
        }
        _ => {}
    }
    Ok(wrapper.changed)
}

#[derive(Clone, Copy)]
enum LabelBox {
    Flow,
    Subroutine,
    Participant,
    ClassNote,
    Fixed,
}

impl LabelBox {
    /// A conservative lower bound on the renderer's content area. Its
    /// approximate character metrics can underestimate wide Latin glyphs.
    /// Wrapping must fit both the requested profile AND this area, otherwise
    /// even a nominally short line such as "WWWWWWWW" can leave its node.
    ///
    /// Counting every scalar as one unit intentionally underestimates its
    /// East-Asian-wide handling, so this check errs toward extra wrapping.
    #[allow(clippy::cast_precision_loss)]
    fn allowance(self, text: &str) -> f32 {
        let count = text.chars().count() as f32;
        match self {
            Self::Flow => (count * 7.5 + 36.0).max(60.0) - 12.0,
            // The subroutine's divider lines are six pixels inside its
            // border. Keep ten pixels on either side of the text.
            Self::Subroutine => (count * 7.5 + 36.0).max(60.0) - 20.0,
            Self::Participant => (count * 8.0 + 40.0).max(110.0) - 16.0,
            // Class notes are left-aligned with eight pixels of padding.
            Self::ClassNote => (count * 7.5 + 16.0).max(60.0) - 20.0,
            Self::Fixed => f32::INFINITY,
        }
    }
}

struct Wrapper {
    shaped_bytes: usize,
    changed: bool,
}

impl Wrapper {
    fn wrap(
        &mut self,
        text: &mut String,
        width: f32,
        font_px: f32,
        geometry: LabelBox,
    ) -> Result<(), String> {
        let Some(hard_lines) = plain_lines(text) else {
            return Ok(());
        };
        if text.graphemes(true).count() > MAX_LABEL_GRAPHEMES {
            return Err("mermaid label exceeds the grapheme limit".into());
        }
        let mut lines = Vec::new();
        for hard_line in hard_lines {
            self.wrap_line(hard_line, width, font_px, geometry, &mut lines)?;
        }
        let prepared = lines.join("<br/>");
        // Do not normalize an already fitting label's author-supplied breaks.
        // Apart from avoiding unnecessary work, this keeps rich syntax and
        // explicit whitespace exactly as supplied on the no-op path.
        let originally = plain_lines(text).expect("checked above");
        if lines != originally {
            *text = prepared;
            self.changed = true;
        }
        Ok(())
    }

    fn wrap_line<'a>(
        &mut self,
        mut rest: &'a str,
        width: f32,
        font_px: f32,
        geometry: LabelBox,
        lines: &mut Vec<&'a str>,
    ) -> Result<(), String> {
        if self.fits(rest, width, font_px, geometry)? {
            if lines.len() >= MAX_LABEL_LINES {
                return Err("mermaid label needs too many lines to show legibly".into());
            }
            lines.push(rest);
            return Ok(());
        }
        loop {
            if lines.len() >= MAX_LABEL_LINES {
                return Err("mermaid label needs too many lines to show legibly".into());
            }
            let mut end = 0;
            let mut word_break = None;
            for (index, grapheme) in rest.grapheme_indices(true) {
                let next = index + grapheme.len();
                if !self.fits(&rest[..next], width, font_px, geometry)? {
                    break;
                }
                end = next;
                if grapheme.chars().all(char::is_whitespace) {
                    word_break = Some(index);
                }
            }
            if end == 0 {
                return Err("mermaid label has a grapheme too wide to show legibly".into());
            }
            if end == rest.len() {
                lines.push(rest);
                return Ok(());
            }
            let boundary = word_break.filter(|&index| index > 0).unwrap_or(end);
            lines.push(rest[..boundary].trim_end());
            rest = rest[boundary..].trim_start();
            if rest.is_empty() {
                return Ok(());
            }
        }
    }

    fn fits(
        &mut self,
        text: &str,
        width: f32,
        font_px: f32,
        geometry: LabelBox,
    ) -> Result<bool, String> {
        self.shaped_bytes = self.shaped_bytes.saturating_add(text.len());
        if self.shaped_bytes > MAX_SHAPED_BYTES {
            return Err("mermaid label shaping exceeds the work limit".into());
        }
        Ok(measure(text, font_px)? <= width.min(geometry.allowance(text)))
    }
}

/// Split only plain text and explicit line breaks. A label containing other
/// markup is not rewritten: splitting a Markdown fence or an entity in half
/// would change what the author wrote. The scanner is UTF-8 boundary-safe.
fn plain_lines(text: &str) -> Option<Vec<&str>> {
    if text.contains('`') || text.contains("$$") || text.contains('#') {
        return None;
    }
    let mut lines = Vec::new();
    let mut start = 0;
    let mut cursor = 0;
    while cursor < text.len() {
        let rest = &text[cursor..];
        let break_len = if rest.starts_with('\n') {
            1
        } else if rest.starts_with("\\n") {
            2
        } else if rest.starts_with('<') {
            let close = rest.find('>')?;
            let tag = rest[1..close].trim().trim_end_matches('/').trim();
            if !tag.eq_ignore_ascii_case("br") {
                return None;
            }
            close + 1
        } else {
            cursor += rest.chars().next()?.len_utf8();
            continue;
        };
        lines.push(text[start..cursor].trim());
        cursor += break_len;
        start = cursor;
    }
    lines.push(text[start..].trim());
    Some(lines)
}

/// Shape the exact face used by resvg. Include outline overhang in addition
/// to advance, so combining marks and italic-like glyphs do not escape a box
/// merely because their logical advance happens to fit.
#[allow(clippy::cast_precision_loss)]
fn measure(text: &str, font_px: f32) -> Result<f32, String> {
    static FACE: OnceLock<Option<Face<'static>>> = OnceLock::new();
    let face = FACE
        .get_or_init(|| Face::from_slice(FONT_REGULAR, 0))
        .as_ref()
        .ok_or("invalid bundled Mermaid font")?;
    let mut buffer = UnicodeBuffer::new();
    buffer.push_str(text);
    buffer.guess_segment_properties();
    let shaped = rustybuzz::shape(face, &[], buffer);
    let mut pen = 0_i64;
    let mut left = 0_i64;
    let mut right = 0_i64;
    for (glyph, position) in shaped.glyph_infos().iter().zip(shaped.glyph_positions()) {
        let id = u16::try_from(glyph.glyph_id).map_err(|_| "invalid Mermaid glyph id")?;
        if let Some(bounds) = face.glyph_bounding_box(rustybuzz::ttf_parser::GlyphId(id)) {
            let x = pen + i64::from(position.x_offset);
            left = left.min(x + i64::from(bounds.x_min));
            right = right.max(x + i64::from(bounds.x_max));
        }
        pen += i64::from(position.x_advance);
    }
    left = left.min(pen);
    right = right.max(pen);
    Ok((right - left) as f32 * font_px / face.units_per_em() as f32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use resvg::usvg;

    fn prepared(source: &str, profile: Profile) -> Diagram {
        let mut diagram = mermaid_svg::parse(source).expect("valid fixture");
        prepare(&mut diagram, profile).expect("label preparation");
        diagram
    }

    fn node_text(diagram: &Diagram) -> &str {
        let Diagram::Flowchart(flow) = diagram else {
            panic!("expected flowchart");
        };
        &flow.nodes[0].text
    }

    fn rendered(diagram: &Diagram) -> usvg::Tree {
        let svg = mermaid_svg::render_diagram_with(
            diagram,
            &mermaid_svg::Theme::default().with_font("Noto Sans"),
        )
        .expect("SVG layout");
        usvg::Tree::from_str(
            &svg,
            &usvg::Options {
                fontdb: super::super::fontdb(),
                ..usvg::Options::default()
            },
        )
        .expect("shaped SVG")
    }

    #[test]
    fn long_labels_wrap_without_changing_flow_semantics() {
        let source =
            "flowchart RL\nA[Download a very detailed customer configuration document] --> B[Done]";
        let Diagram::Flowchart(original) = mermaid_svg::parse(source).unwrap() else {
            unreachable!();
        };
        let compact = prepared(source, Profile::Compact);
        let Diagram::Flowchart(flow) = &compact else {
            unreachable!();
        };
        assert_eq!(flow.direction, original.direction);
        assert_eq!(flow.edges, original.edges);
        assert_eq!(flow.nodes[0].id, original.nodes[0].id);
        assert!(flow.nodes[0].text.contains("<br/>"));
        assert_eq!(
            flow.nodes[0].text.replace("<br/>", " "),
            original.nodes[0].text
        );
        let mut again = compact.clone();
        assert!(!prepare(&mut again, Profile::Compact).unwrap());
    }

    #[test]
    fn admitted_shapes_contain_wide_label_ink_without_crossing_their_outlines() {
        for (open, close) in [("[", "]"), ("(", ")"), ("[[", "]]"), ("[(", ")]")] {
            for count in [28, 84] {
                let source = format!("flowchart TD\nA{open}{}{close}", "W".repeat(count));
                for profile in [Profile::Standard, Profile::Compact] {
                    let diagram = prepared(&source, profile);
                    assert!(node_text(&diagram).contains("<br/>"));
                    assert_ink_inside_shape(&rendered(&diagram), &source);
                }
            }
        }
    }

    /// Sample real filled outlines and strokes at 2x resolution, not just
    /// axis-aligned bounding boxes. Including internal strokes catches text
    /// touching a cylinder cap or a subroutine's inset divider.
    fn assert_ink_inside_shape(tree: &usvg::Tree, source: &str) {
        use tiny_skia::{FillRule, Paint, Pixmap, Transform};

        let size = tree.size().to_int_size().scale_by(2.0).unwrap();
        let mut fill = Pixmap::new(size.width(), size.height()).unwrap();
        let mut outlines = fill.clone();
        let mut ink = fill.clone();
        let paint = Paint::default();
        let mut groups = vec![tree.root()];
        while let Some(group) = groups.pop() {
            for node in group.children() {
                match node {
                    usvg::Node::Group(group) => groups.push(group),
                    usvg::Node::Path(path) => {
                        let transform = path.abs_transform().post_scale(2.0, 2.0);
                        if path.fill().is_some() {
                            fill.fill_path(path.data(), &paint, FillRule::Winding, transform, None);
                        }
                        if let Some(stroke) = path.stroke() {
                            let mut stroke = stroke.to_tiny_skia();
                            // Require one pixel of space beyond the stroke,
                            // including the subroutine's internal dividers.
                            stroke.width += 2.0;
                            outlines.stroke_path(path.data(), &paint, &stroke, transform, None);
                        }
                    }
                    usvg::Node::Text(_) => {
                        let bbox = node.abs_layer_bounding_box().unwrap();
                        let transform =
                            Transform::from_translate(bbox.x(), bbox.y()).post_scale(2.0, 2.0);
                        resvg::render_node(node, transform, &mut ink.as_mut()).unwrap();
                    }
                    usvg::Node::Image(_) => {}
                }
            }
        }
        let mut ink_pixels = 0;
        for (index, ((ink, fill), outline)) in ink
            .pixels()
            .iter()
            .zip(fill.pixels())
            .zip(outlines.pixels())
            .enumerate()
        {
            if ink.alpha() > 16 {
                ink_pixels += 1;
                assert!(
                    fill.alpha() >= 250 && outline.alpha() <= 16,
                    "ink outside fill or touching outline at pixel {index}: {source}"
                );
            }
        }
        assert!(ink_pixels > 100, "expected rendered label ink: {source}");
    }

    #[test]
    fn stadium_labels_are_not_wrapped_into_tall_ovals() {
        for count in [28, 84] {
            let label = "W".repeat(count);
            let source = format!("flowchart TD\nA([{label}])");
            for profile in [Profile::Standard, Profile::Compact] {
                let mut diagram = mermaid_svg::parse(&source).unwrap();
                assert!(!prepare(&mut diagram, profile).unwrap());
                assert_eq!(node_text(&diagram), label);
            }
        }
    }

    #[test]
    fn identifiers_and_unicode_graphemes_are_never_dropped_or_split() {
        for original in [
            "aVeryLongUnbrokenConfigurationIdentifierWithoutAnyWordBreaks".repeat(3),
            "e\u{301}👩\u{200d}💻".repeat(30),
        ] {
            let diagram = prepared(
                &format!("flowchart TD\nA[\"{original}\"]"),
                Profile::Compact,
            );
            let label = node_text(&diagram);
            assert!(label.contains("<br/>"));
            assert_eq!(label.replace("<br/>", ""), original);
            let graphemes: Vec<_> = original.graphemes(true).collect();
            let after: Vec<_> = label
                .split("<br/>")
                .flat_map(|line| line.graphemes(true))
                .collect();
            assert_eq!(after, graphemes);
        }
    }

    #[test]
    fn explicit_breaks_and_markup_survive_preparation() {
        for label in [
            "First<BR />Second\\nThird",
            "<b>A long label that must preserve its HTML style intact</b>",
            "`A **bold** markdown string that must preserve its fences`",
            "Cost #35; and encoded #lt;tag#gt;",
        ] {
            let diagram = prepared(&format!("flowchart TD\nA[\"{label}\"]"), Profile::Compact);
            assert_eq!(node_text(&diagram), label);
        }
    }

    #[test]
    fn color_classes_wrap_but_custom_typography_is_preserved() {
        let text = "A long styled label which should only wrap with matching font metrics";
        for (style, should_wrap) in [("fill:#123", true), ("font-size:32px", false)] {
            let source = format!("flowchart TD\nA[{text}]:::accent\nclassDef accent {style}");
            let diagram = prepared(&source, Profile::Compact);
            assert_eq!(node_text(&diagram).contains("<br/>"), should_wrap);
        }
        let source = format!("flowchart TD\nA[{text}]\nclassDef default font-weight:bold");
        let diagram = prepared(&source, Profile::Compact);
        assert_eq!(node_text(&diagram), text);
    }

    #[test]
    fn nested_notes_break_long_tokens_at_their_fixed_box_width() {
        let mut diagram = prepared(
            "sequenceDiagram\nparticipant A\nloop work\nNote right of A: WWWWWWWWWWWWWWWWWWWWWWWWWW\nend",
            Profile::Standard,
        );
        let Diagram::Sequence(sequence) = &mut diagram else {
            unreachable!();
        };
        let SequenceItem::Loop(block) = &sequence.items[0] else {
            unreachable!();
        };
        let SequenceItem::Note(note) = &block.items[0] else {
            unreachable!();
        };
        assert!(note.text.contains("<br/>"));
        for line in note.text.split("<br/>") {
            assert!(measure(line, 12.0).unwrap() <= 120.0);
        }
        assert!(rendered(&diagram).size().height() > 100.0);
    }

    #[test]
    fn participant_and_class_note_ink_stays_in_their_rendered_boxes() {
        for source in [
            "sequenceDiagram\nparticipant A as WWWWWWWWWWWWWWWWWWWWWWWWWW",
            "classDiagram\nclass A\nnote for A \"WWWWWWWWWWWWWWWWWWWWWWWWWW\"",
        ] {
            let diagram = prepared(source, Profile::Compact);
            let tree = rendered(&diagram);
            let mut shapes = Vec::new();
            let mut labels = Vec::new();
            let mut groups = vec![tree.root()];
            while let Some(group) = groups.pop() {
                for node in group.children() {
                    match node {
                        usvg::Node::Group(group) => groups.push(group),
                        usvg::Node::Path(path) if path.fill().is_some() => {
                            shapes.push(path.abs_bounding_box());
                        }
                        usvg::Node::Text(text)
                            if text.chunks().iter().any(|chunk| chunk.text().contains('W')) =>
                        {
                            labels.push(text.abs_bounding_box());
                        }
                        _ => {}
                    }
                }
            }
            assert!(labels.len() > 1, "expected wrapped lines: {source}");
            for label in labels {
                assert!(
                    shapes.iter().any(|shape| {
                        label.left() >= shape.left() + 2.0
                            && label.right() <= shape.right() - 2.0
                            && label.top() >= shape.top()
                            && label.bottom() <= shape.bottom()
                    }),
                    "{label:?} escapes {shapes:?}: {source}"
                );
            }
        }
    }

    #[test]
    fn unsupported_geometry_is_not_rewritten() {
        let source = "stateDiagram-v2\nstate \"A state label whose geometry is only single line\" as A\nA --> B: a long transition description";
        let mut diagram = mermaid_svg::parse(source).unwrap();
        let before = format!("{diagram:?}");
        assert!(!prepare(&mut diagram, Profile::Compact).unwrap());
        assert_eq!(format!("{diagram:?}"), before);
    }

    #[test]
    fn pathological_labels_fail_without_truncation() {
        let original = "W".repeat(2048);
        let mut diagram = mermaid_svg::parse(&format!("flowchart TD\nA[{original}]")).unwrap();
        assert!(prepare(&mut diagram, Profile::Compact).is_err());
        assert_eq!(node_text(&diagram), original);
    }
}
