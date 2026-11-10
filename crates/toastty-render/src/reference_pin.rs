//! An ephemeral, bounded text reference anchored to the viewport.

use toastty_protocols::unicode_core::cluster_cell_width;
use unicode_segmentation::UnicodeSegmentation;

use crate::dialog::pipeline::UiRect;
use crate::dialog::{Span, TextRun, Tone, dialog_colors};

const MAX_BYTES: usize = 16 * 1024;

/// A snapshot, independent of the selection and terminal scrollback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReferencePin {
    text: String,
    truncated: bool,
}

impl ReferencePin {
    /// Retain at most 16 KiB, cutting only at a grapheme boundary.
    /// Whitespace is preserved; wholly blank selections are not pinned.
    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        if text.trim().is_empty() {
            return None;
        }
        let end = text
            .grapheme_indices(true)
            .take_while(|(i, g)| i + g.len() <= MAX_BYTES)
            .last()
            .map_or(0, |(i, g)| i + g.len());
        if end == 0 {
            return None;
        }
        Some(Self {
            text: text[..end].to_owned(),
            truncated: end < text.len(),
        })
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// True when the retained snapshot, not merely its preview, was cut.
    #[must_use]
    pub fn truncated(&self) -> bool {
        self.truncated
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinButton {
    Copy,
    Dismiss,
}

pub(crate) struct PinMetrics {
    pub origin: [f32; 2],
    pub content: (f32, f32),
    pub cell: (f32, f32),
    pub scale: f32,
}

pub(crate) struct PinLayout {
    pub panel: [f32; 4],
    pub lines: Vec<TextRun>,
    pub copy: [f32; 4],
    pub dismiss: [f32; 4],
    pub scale: f32,
}

fn inside(r: [f32; 4], x: f32, y: f32) -> bool {
    x >= r[0] && x < r[2] && y >= r[1] && y < r[3]
}

impl PinLayout {
    pub(crate) fn contains(&self, x: f32, y: f32) -> bool {
        inside(self.panel, x, y)
    }
    pub(crate) fn button_at(&self, x: f32, y: f32) -> Option<PinButton> {
        if inside(self.copy, x, y) {
            Some(PinButton::Copy)
        } else if inside(self.dismiss, x, y) {
            Some(PinButton::Dismiss)
        } else {
            None
        }
    }
}

/// Printable preview only. Copy always uses the unmodified snapshot.
fn preview(text: &str, cols: usize, rows: usize) -> (Vec<String>, bool) {
    let mut lines = vec![String::new()];
    let mut used = 0;
    for g in text.graphemes(true) {
        if g == "\n" || g == "\r\n" {
            if lines.len() == rows {
                return (lines, true);
            }
            lines.push(String::new());
            used = 0;
            continue;
        }
        let printable = if g == "\t" { "    " } else { g };
        if printable.chars().any(char::is_control) {
            continue;
        }
        let width = if g == "\t" {
            4
        } else {
            usize::from(cluster_cell_width(g, true))
        };
        if used + width > cols {
            if lines.len() == rows {
                return (lines, true);
            }
            lines.push(String::new());
            used = 0;
        }
        if width > cols {
            return (lines, true);
        }
        lines
            .last_mut()
            .expect("one preview line")
            .push_str(printable);
        used += width;
    }
    (lines, false)
}

/// Compact top-right card, never wider than 60% or taller than 50%.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
pub(crate) fn pin_layout(pin: &ReferencePin, m: &PinMetrics) -> Option<PinLayout> {
    if ![m.content.0, m.content.1, m.cell.0, m.cell.1, m.scale]
        .into_iter()
        .all(|v| v.is_finite() && v > 0.0)
        || !m.origin.into_iter().all(f32::is_finite)
    {
        return None;
    }
    let pad = 12.0 * m.scale;
    let margin = 12.0 * m.scale;
    let width = (52.0 * m.cell.0 + 2.0 * pad).min(m.content.0 * 0.6);
    let cols = ((width - 2.0 * pad) / m.cell.0).floor().max(0.0) as usize;
    let button_h = (28.0 * m.scale).max(m.cell.1 + 8.0 * m.scale);
    let fixed_h = 3.0 * pad + 2.0 * m.cell.1 + button_h;
    let rows = ((m.content.1 * 0.5 - fixed_h) / m.cell.1)
        .floor()
        .clamp(0.0, 6.0) as usize;
    if cols < 24 || rows == 0 {
        return None;
    }
    let (body, clipped) = preview(pin.text(), cols, rows);
    let height = fixed_h + body.len() as f32 * m.cell.1;
    let x = (m.origin[0] + m.content.0 - margin - width).floor();
    let y = (m.origin[1] + margin).floor();
    let mut lines = Vec::new();
    let mut line = |text: String, origin: [f32; 2], tone| {
        lines.push(TextRun {
            origin,
            spans: vec![Span { text, tone }],
        });
    };
    line("REFERENCE PIN".into(), [x + pad, y + pad], Tone::Muted);
    for (i, text) in body.into_iter().enumerate() {
        line(
            text,
            [x + pad, y + pad + (i + 1) as f32 * m.cell.1],
            Tone::Strong,
        );
    }
    let status_y = y + height - 2.0 * pad - button_h - m.cell.1;
    let status = if pin.truncated() {
        "Capped at 16 KiB"
    } else if clipped {
        "Preview clipped; Copy"
    } else {
        "Snapshot; not live"
    };
    line(status.into(), [x + pad, status_y], Tone::Muted);
    let button_y = y + height - pad - button_h;
    let copy_w = 6.0 * m.cell.0;
    let dismiss_w = 9.0 * m.cell.0;
    let dismiss = [
        x + width - pad - dismiss_w,
        button_y,
        x + width - pad,
        button_y + button_h,
    ];
    let copy = [
        dismiss[0] - pad - copy_w,
        button_y,
        dismiss[0] - pad,
        button_y + button_h,
    ];
    let label_y = button_y + (button_h - m.cell.1) * 0.5;
    line("Copy".into(), [copy[0] + m.cell.0, label_y], Tone::Strong);
    line(
        "Dismiss".into(),
        [dismiss[0] + m.cell.0, label_y],
        Tone::Strong,
    );
    Some(PinLayout {
        panel: [x, y, x + width, y + height],
        lines,
        copy,
        dismiss,
        scale: m.scale,
    })
}

pub(crate) fn pin_rects(
    l: &PinLayout,
    bg: [f32; 4],
    fg: [f32; 4],
    hover: Option<PinButton>,
) -> Vec<UiRect> {
    let colors = dialog_colors(bg, fg, None);
    let mut rects = vec![UiRect::rect(
        l.panel,
        colors.panel_fill,
        colors.text_muted,
        10.0 * l.scale,
        l.scale,
    )];
    for (button, rect) in [(PinButton::Copy, l.copy), (PinButton::Dismiss, l.dismiss)] {
        let c = dialog_colors(
            bg,
            fg,
            (hover == Some(button)).then_some(crate::DialogButton::Cancel),
        );
        rects.push(UiRect::rect(
            rect,
            c.cancel.fill,
            c.cancel.ring,
            5.0 * l.scale,
            l.scale,
        ));
    }
    rects
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics() -> PinMetrics {
        PinMetrics {
            origin: [10.0, 20.0],
            content: (1000.0, 700.0),
            cell: (10.0, 20.0),
            scale: 1.0,
        }
    }

    #[test]
    fn snapshot_preserves_whitespace_and_cuts_at_graphemes() {
        assert!(ReferencePin::new(" \n\t").is_none());
        assert_eq!(ReferencePin::new("  x\n").unwrap().text(), "  x\n");
        let input = format!("{}👩‍💻tail", "a".repeat(MAX_BYTES - 1));
        let pin = ReferencePin::new(&input).unwrap();
        assert_eq!(pin.text(), "a".repeat(MAX_BYTES - 1));
        assert!(pin.truncated());
    }

    #[test]
    fn preview_wraps_wide_clusters_and_preserves_copy() {
        let pin = ReferencePin::new("你好e\u{301}👩‍💻\nnext\tline").unwrap();
        let (lines, cut) = preview(pin.text(), 4, 3);
        assert_eq!(lines, ["你好", "e\u{301}👩‍💻", "next"]);
        assert!(cut);
        assert_eq!(pin.text(), "你好e\u{301}👩‍💻\nnext\tline");
        assert!(!pin.truncated());
    }

    #[test]
    fn all_text_fits_the_narrowest_card_at_multiple_scales() {
        let pin = ReferencePin::new(&"x".repeat(500)).unwrap();
        for scale in [1.0, 2.0] {
            let m = PinMetrics {
                content: (440.0 * scale, 400.0 * scale),
                cell: (10.0 * scale, 19.0 * scale),
                scale,
                ..metrics()
            };
            let l = pin_layout(&pin, &m).unwrap();
            for run in &l.lines {
                let cells: u16 = run
                    .spans
                    .iter()
                    .flat_map(|span| span.text.graphemes(true))
                    .map(|g| u16::from(cluster_cell_width(g, true)))
                    .sum();
                let right = run.origin[0] + f32::from(cells) * m.cell.0;
                assert!(
                    right <= l.panel[2] - 12.0 * scale + 0.01,
                    "text exceeds the card's content area at scale {scale}"
                );
            }
        }
    }

    #[test]
    fn card_stays_bounded_and_buttons_match_drawn_rects() {
        let pin = ReferencePin::new(&"row\n".repeat(30)).unwrap();
        let m = metrics();
        let l = pin_layout(&pin, &m).unwrap();
        assert!(l.panel[2] - l.panel[0] <= m.content.0 * 0.6);
        assert!(l.panel[3] - l.panel[1] <= m.content.1 * 0.5);
        assert!(
            l.lines
                .iter()
                .any(|r| r.spans[0].text == "Preview clipped; Copy")
        );
        assert_eq!(
            l.button_at(l.copy[0] + 1.0, l.copy[1] + 1.0),
            Some(PinButton::Copy)
        );
        assert_eq!(
            l.button_at(l.dismiss[0] + 1.0, l.dismiss[1] + 1.0),
            Some(PinButton::Dismiss)
        );
        assert!(l.contains(l.panel[0] + 1.0, l.panel[1] + 1.0));
        assert_eq!(l.button_at(l.panel[0], l.panel[1]), None);
        assert!(!l.contains(l.panel[2], l.panel[3]));
    }

    #[test]
    fn tiny_or_invalid_windows_hide_card() {
        let pin = ReferencePin::new("hello").unwrap();
        for content in [(100.0, 700.0), (1000.0, 100.0), (f32::NAN, 700.0)] {
            assert!(
                pin_layout(
                    &pin,
                    &PinMetrics {
                        content,
                        ..metrics()
                    }
                )
                .is_none()
            );
        }
    }
}
