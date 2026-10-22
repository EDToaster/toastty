//! Close-confirmation dialog ("a program is still running").
//!
//! A modal card drawn over a dimmed window: an icon disc, a two-line
//! message and two buttons (`Cancel [Esc]`, `Close [↵]`). Like the
//! scroll-to-bottom chip ([`crate::chip`]) the shapes are not built from
//! terminal cells. They are drawn by a small instanced SDF pipeline
//! ([`pipeline::UiPipeline`], `shaders/ui.wgsl`), so the rounded corners,
//! hairline rings, soft shadow and icon are anti-aliased at device
//! resolution. The text is ordinary terminal-font glyphs, placed at pixel
//! offsets and drawn by the text pipeline after the panel.
//!
//! This module holds the pure, unit-tested parts: the copy
//! ([`prompt_copy`]), the geometry ([`dialog_layout`]), the theme-derived
//! colours ([`dialog_colors`]), the shape list ([`dialog_rects`]) and
//! button hit-testing ([`DialogLayout::button_at`]). All sizes are logical
//! px multiplied by the window scale, except the text-related ones, which
//! come from the cell metrics so the dialog follows the font size.

pub mod pipeline;

use toastty_protocols::unicode_core::char_cell_width;

use crate::chip::{linear_to_srgb, srgb_to_linear};
use pipeline::{UI_KIND_PROMPT_ICON, UI_KIND_RETURN_ICON, UI_KIND_SHADOW, UiRect};

/// Preferred panel width, logical px (grows to fit large fonts, shrinks
/// to fit small windows).
pub const PANEL_WIDTH: f32 = 420.0;
/// Panel inner padding, logical px.
pub const PANEL_PADDING: f32 = 18.0;
/// Panel corner radius, logical px.
pub const PANEL_RADIUS: f32 = 14.0;
/// Minimum gap between the panel and the content edges, logical px.
pub const PANEL_MARGIN: f32 = 16.0;
/// Icon disc diameter, logical px.
pub const ICON_DIAMETER: f32 = 28.0;
/// Gap between the icon disc and the text column, logical px.
pub const ICON_GAP: f32 = 14.0;
/// Icon stroke width, logical px.
pub const ICON_STROKE: f32 = 1.6;
/// Return-arrow keycap icon stroke width, logical px.
pub const RETURN_STROKE: f32 = 1.4;
/// Gap between the text and the button row, logical px.
pub const BUTTON_ROW_GAP: f32 = 18.0;
/// Gap between the two buttons, logical px.
pub const BUTTON_GAP: f32 = 8.0;
/// Minimum button height, logical px (taller when the font is).
pub const BUTTON_HEIGHT: f32 = 32.0;
/// Button corner radius, logical px.
pub const BUTTON_RADIUS: f32 = 8.0;
/// Space before a button's label, logical px.
pub const BUTTON_PAD_LEFT: f32 = 12.0;
/// Space after a button's keycap, logical px.
pub const BUTTON_PAD_RIGHT: f32 = 8.0;
/// Gap between a button's label and its keycap, logical px.
pub const LABEL_KEYCAP_GAP: f32 = 8.0;
/// Keycap corner radius, logical px.
pub const KEYCAP_RADIUS: f32 = 4.0;
/// Keycap horizontal padding around its text, logical px.
pub const KEYCAP_PAD_X: f32 = 5.0;
/// Default-button focus ring width and its gap to the button, logical px.
pub const FOCUS_RING_WIDTH: f32 = 2.0;
pub const FOCUS_RING_OFFSET: f32 = 2.0;
/// Drop shadow vertical offset and blur, logical px.
pub const SHADOW_OFFSET_Y: f32 = 12.0;
pub const SHADOW_BLUR: f32 = 32.0;
/// Opacity of the black scrim over the window, as a designer means it
/// (blended in sRGB; see [`black_overlay_alpha`]).
pub const SCRIM_ALPHA: f32 = 0.45;
/// Opacity of the panel's black drop shadow (sRGB terms, like the scrim).
pub const SHADOW_ALPHA: f32 = 0.5;

/// Second line of the prompt when a program is running.
pub const DETAIL_TEXT: &str = "Close the window and stop it?";

/// What the close prompt is about. The binary fills this in; the copy is
/// derived from it by [`prompt_copy`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClosePrompt {
    /// Short name of the foreground program (e.g. `"vim"`), when known.
    pub program: Option<String>,
    /// A program other than the shell is running in the foreground.
    pub running: bool,
}

/// The dialog's two buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogButton {
    /// Dismiss the prompt and keep the window (Esc / `n`).
    Cancel,
    /// Close the window and stop the program (Enter / `y`, the default).
    Close,
}

/// How strongly a text span is drawn. There's no bold face, so colour
/// carries the emphasis: `Strong` is the full text colour of its surface,
/// `Muted` is 72% of the way there from the surface colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Strong,
    Muted,
}

/// A run of text in one [`Tone`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub tone: Tone,
}

impl Span {
    fn new(text: impl Into<String>, tone: Tone) -> Self {
        Self {
            text: text.into(),
            tone,
        }
    }
}

/// The dialog's message: a headline and an optional second line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptCopy {
    pub headline: Vec<Span>,
    pub detail: Option<Span>,
}

/// The message for `prompt`:
/// - a known program: `<name> is still running` (the name emphasised)
///   plus [`DETAIL_TEXT`];
/// - something running but no name: `A program is still running` plus
///   [`DETAIL_TEXT`];
/// - nothing running (`confirm_close = "always"` at a bare shell):
///   `Close this window?` alone.
#[must_use]
pub fn prompt_copy(prompt: &ClosePrompt) -> PromptCopy {
    let name: Option<String> = prompt
        .program
        .as_deref()
        .map(|n| n.chars().filter(|c| !c.is_control()).collect::<String>())
        .map(|n| n.trim().to_owned())
        .filter(|n| !n.is_empty());
    let detail = Some(Span::new(DETAIL_TEXT, Tone::Muted));
    match name {
        Some(name) => PromptCopy {
            headline: vec![
                Span::new(name, Tone::Strong),
                Span::new(" is still running", Tone::Muted),
            ],
            detail,
        },
        None if prompt.running => PromptCopy {
            headline: vec![Span::new("A program is still running", Tone::Strong)],
            detail,
        },
        None => PromptCopy {
            headline: vec![Span::new("Close this window?", Tone::Strong)],
            detail: None,
        },
    }
}

/// Terminal columns `s` occupies.
fn cols(s: &str) -> usize {
    s.chars()
        .map(|c| usize::from(char_cell_width(c, false)))
        .sum()
}

fn spans_cols(spans: &[Span]) -> usize {
    spans.iter().map(|s| cols(&s.text)).sum()
}

/// The longest prefix of `s` that fits in `max` columns.
fn take_cols(s: &str, max: usize) -> String {
    let mut used = 0;
    s.chars()
        .take_while(|&c| {
            used += usize::from(char_cell_width(c, false));
            used <= max
        })
        .collect()
}

const ELLIPSIS: &str = "…";

/// Fit `spans` into `max` columns, ending with an ellipsis when cut. A
/// two-span headline (`<name>` + ` is still running`) shortens the name
/// first so the sentence stays readable; otherwise the line is cut at
/// the end.
fn fit_spans(spans: &[Span], max: usize) -> Vec<Span> {
    if spans_cols(spans) <= max {
        return spans.to_vec();
    }
    if max == 0 {
        return Vec::new();
    }
    if let [first, rest @ ..] = spans
        && !rest.is_empty()
    {
        let rest_cols = spans_cols(rest);
        // Keep at least one char of the name before the ellipsis.
        if rest_cols + 2 <= max {
            let name = take_cols(&first.text, max - rest_cols - 1);
            let mut out = vec![Span::new(name + ELLIPSIS, first.tone)];
            out.extend_from_slice(rest);
            return out;
        }
    }
    let mut out = Vec::new();
    let mut left = max - 1;
    for span in spans {
        let w = cols(&span.text);
        if w <= left {
            out.push(span.clone());
            left -= w;
        } else {
            out.push(Span::new(take_cols(&span.text, left), span.tone));
            break;
        }
    }
    if let Some(last) = out.last_mut() {
        last.text.push_str(ELLIPSIS);
    }
    out
}

/// Inputs to [`dialog_layout`], all physical px.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DialogMetrics {
    /// Full surface size (the scrim covers it, padding gutter included).
    pub surface: (f32, f32),
    /// Content area top-left (inside the window padding).
    pub content_origin: [f32; 2],
    /// Content area size.
    pub content: (f32, f32),
    /// Terminal cell size; dialog text uses the terminal font.
    pub cell: (f32, f32),
    /// Physical px per logical px.
    pub scale: f32,
}

/// One line of text: spans laid out cell by cell from `origin`, the
/// top-left of the first cell (physical px, integer-aligned).
#[derive(Debug, Clone, PartialEq)]
pub struct TextRun {
    pub origin: [f32; 2],
    pub spans: Vec<Span>,
}

/// What a keycap shows.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyGlyph {
    /// A key name in the terminal font (`Esc`).
    Text(TextRun),
    /// The return-arrow icon, drawn by the UI pipeline in this box. (The
    /// font's `↵` is a wide fallback glyph that overflows a one-column
    /// keycap and renders thin.)
    Return([f32; 4]),
}

/// A button's keycap hint.
#[derive(Debug, Clone, PartialEq)]
pub struct Keycap {
    pub rect: [f32; 4],
    pub glyph: KeyGlyph,
}

/// A button's geometry, physical px. Rects are `[x0, y0, x1, y1]`.
#[derive(Debug, Clone, PartialEq)]
pub struct ButtonLayout {
    pub kind: DialogButton,
    pub rect: [f32; 4],
    pub label: TextRun,
    /// `None` when the window is too narrow for the keycaps.
    pub keycap: Option<Keycap>,
}

/// Where everything goes, physical px (absolute framebuffer coords).
#[derive(Debug, Clone, PartialEq)]
pub struct DialogLayout {
    pub scrim: [f32; 4],
    pub panel: [f32; 4],
    /// Bounding box of the icon disc.
    pub icon: [f32; 4],
    pub lines: Vec<TextRun>,
    pub cancel: ButtonLayout,
    pub close: ButtonLayout,
    /// Physical px per logical px the layout was built for.
    pub scale: f32,
}

impl DialogLayout {
    /// The button under `(x, y)` (physical px), if any.
    #[must_use]
    pub fn button_at(&self, x: f32, y: f32) -> Option<DialogButton> {
        let inside = |r: [f32; 4]| x >= r[0] && x < r[2] && y >= r[1] && y < r[3];
        [&self.cancel, &self.close]
            .into_iter()
            .find(|b| inside(b.rect))
            .map(|b| b.kind)
    }

    /// Both buttons, Cancel first.
    #[must_use]
    pub fn buttons(&self) -> [&ButtonLayout; 2] {
        [&self.cancel, &self.close]
    }
}

/// A keycap's key.
#[derive(Debug, Clone, Copy)]
enum Key {
    Text(&'static str),
    Return,
}

/// How the button row fits the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowFit {
    /// Natural widths with keycaps.
    Full,
    /// Natural widths, keycaps dropped.
    Compact,
    /// Two equal buttons filling the row, labels truncated to fit.
    Squeezed,
}

/// Button sizing for one scale / cell size, physical px.
struct ButtonMetrics {
    scale: f32,
    cell: (f32, f32),
    height: f32,
    keycap_h: f32,
}

#[allow(clippy::cast_precision_loss)]
impl ButtonMetrics {
    fn new(scale: f32, cell: (f32, f32)) -> Self {
        let height = (BUTTON_HEIGHT * scale).max(cell.1 + 12.0 * scale).round();
        Self {
            scale,
            cell,
            height,
            keycap_h: (cell.1 + 2.0 * scale).round().min(height),
        }
    }

    fn text_w(&self, s: &str) -> f32 {
        cols(s) as f32 * self.cell.0
    }

    /// Width of the keycap's content: the key name, or the return icon
    /// (sized from the cell height so it follows the font).
    fn key_w(&self, key: Key) -> f32 {
        match key {
            Key::Text(t) => self.text_w(t),
            Key::Return => (self.cell.1 * 0.75).round(),
        }
    }

    fn keycap_w(&self, key: Key) -> f32 {
        (2.0 * KEYCAP_PAD_X * self.scale + self.key_w(key)).round()
    }

    /// Natural width with (`Full`) or without (`Compact`) the keycap.
    fn width(&self, label: &str, key: Key, fit: RowFit) -> f32 {
        let s = self.scale;
        if fit == RowFit::Full {
            (BUTTON_PAD_LEFT * s + self.text_w(label) + LABEL_KEYCAP_GAP * s + BUTTON_PAD_RIGHT * s)
                .round()
                + self.keycap_w(key)
        } else {
            (2.0 * BUTTON_PAD_LEFT * s + self.text_w(label)).round()
        }
    }

    /// The button filling `rect`. With keycaps the label sits at the left
    /// padding; without, it is centred (and truncated to fit when
    /// `Squeezed`).
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn place(
        &self,
        kind: DialogButton,
        label: &str,
        key: Key,
        rect: [f32; 4],
        fit: RowFit,
    ) -> ButtonLayout {
        let s = self.scale;
        let w = rect[2] - rect[0];
        let y = rect[1];
        let text_top = (y + (self.height - self.cell.1) * 0.5).round();
        let mut spans = vec![Span::new(label, Tone::Strong)];
        if fit == RowFit::Squeezed {
            let room = (w - 2.0 * BUTTON_PAD_LEFT * s).max(0.0);
            spans = fit_spans(&spans, (room / self.cell.0).floor() as usize);
        }
        let label_w = self.text_w(&spans.iter().map(|sp| sp.text.as_str()).collect::<String>());
        if fit != RowFit::Full {
            return ButtonLayout {
                kind,
                rect,
                label: TextRun {
                    origin: [(rect[0] + (w - label_w) * 0.5).round(), text_top],
                    spans,
                },
                keycap: None,
            };
        }
        let label_x = (rect[0] + BUTTON_PAD_LEFT * s).round();
        let kc_x = (label_x + label_w + LABEL_KEYCAP_GAP * s).round();
        let kc_y = (y + (self.height - self.keycap_h) * 0.5).round();
        let kc_w = self.keycap_w(key);
        let content_x = (kc_x + (kc_w - self.key_w(key)) * 0.5).round();
        let glyph = match key {
            Key::Text(t) => KeyGlyph::Text(TextRun {
                origin: [content_x, text_top],
                spans: vec![Span::new(t, Tone::Muted)],
            }),
            Key::Return => {
                let side = self.key_w(key);
                let top = (kc_y + (self.keycap_h - side) * 0.5).round();
                KeyGlyph::Return([content_x, top, content_x + side, top + side])
            }
        };
        ButtonLayout {
            kind,
            rect,
            label: TextRun {
                origin: [label_x, text_top],
                spans,
            },
            keycap: Some(Keycap {
                rect: [kc_x, kc_y, kc_x + kc_w, kc_y + self.keycap_h],
                glyph,
            }),
        }
    }

    /// Lay the two buttons out right-aligned in `[x0, x1]` at `y`,
    /// shedding keycaps and then squeezing the buttons so the row never
    /// leaves that span.
    fn row(
        &self,
        buttons: &[(DialogButton, &str, Key); 2],
        x0: f32,
        x1: f32,
        y: f32,
    ) -> [ButtonLayout; 2] {
        let gap_full = (BUTTON_GAP * self.scale).round();
        let avail = (x1 - x0).max(0.0);
        let row_w = |fit| {
            buttons
                .iter()
                .map(|b| self.width(b.1, b.2, fit))
                .sum::<f32>()
                + gap_full
        };
        let fit = if row_w(RowFit::Full) <= avail {
            RowFit::Full
        } else if row_w(RowFit::Compact) <= avail {
            RowFit::Compact
        } else {
            RowFit::Squeezed
        };
        let gap = gap_full.min(avail);
        let squeezed_w = ((avail - gap) * 0.5).floor().max(0.0);
        let mut right = x0 + avail;
        let mut place = |&(kind, label, key): &(DialogButton, &str, Key)| {
            let w = if fit == RowFit::Squeezed {
                squeezed_w
            } else {
                self.width(label, key, fit)
            };
            let rect = [right - w, y, right, y + self.height];
            right = rect[0] - gap;
            self.place(kind, label, key, rect, fit)
        };
        let close = place(&buttons[1]);
        let cancel = place(&buttons[0]);
        [cancel, close]
    }
}

/// Lay the dialog for `copy` out in `m`. `None` only for degenerate
/// metrics (non-finite or non-positive scale / cell size); a window too
/// small for the panel gets a squeezed panel with truncated text, never a
/// panic.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn dialog_layout(copy: &PromptCopy, m: &DialogMetrics) -> Option<DialogLayout> {
    let s = m.scale;
    let (cw, ch) = m.cell;
    let finite = [
        s,
        cw,
        ch,
        m.surface.0,
        m.surface.1,
        m.content.0,
        m.content.1,
    ]
    .iter()
    .chain(m.content_origin.iter())
    .all(|v| v.is_finite());
    if !(finite && s > 0.0 && cw > 0.0 && ch > 0.0) {
        return None;
    }
    let pad = (PANEL_PADDING * s).round();
    let icon_d = (ICON_DIAMETER * s).round();
    let icon_gap = (ICON_GAP * s).round();
    let bm = ButtonMetrics::new(s, m.cell);
    let buttons = [
        (DialogButton::Cancel, "Cancel", Key::Text("Esc")),
        (DialogButton::Close, "Close", Key::Return),
    ];
    let row_w: f32 = buttons
        .iter()
        .map(|b| bm.width(b.1, b.2, RowFit::Full))
        .sum::<f32>()
        + (BUTTON_GAP * s).round();

    let lines_src: Vec<&[Span]> = std::iter::once(copy.headline.as_slice())
        .chain(copy.detail.as_ref().map(std::slice::from_ref))
        .collect();
    let text_cols = lines_src.iter().map(|l| spans_cols(l)).max().unwrap_or(0);
    let natural_inner = (icon_d + icon_gap + text_cols as f32 * cw).max(row_w);
    let max_w = (m.content.0 - 2.0 * PANEL_MARGIN * s).max(0.0);
    let panel_w = (PANEL_WIDTH * s)
        .max(natural_inner + 2.0 * pad)
        .min(max_w)
        .floor();

    let text_w = (panel_w - 2.0 * pad - icon_d - icon_gap).max(0.0);
    let fit_cols = (text_w / cw).floor() as usize;
    let lines: Vec<Vec<Span>> = lines_src.iter().map(|l| fit_spans(l, fit_cols)).collect();
    let text_h = lines.len() as f32 * ch;
    let header_h = icon_d.max(text_h);
    let panel_h = (2.0 * pad + header_h + BUTTON_ROW_GAP * s + bm.height).round();

    let x0 = (m.content_origin[0] + (m.content.0 - panel_w) * 0.5).round();
    let y0 = (m.content_origin[1] + ((m.content.1 - panel_h) * 0.5).max(0.0)).round();
    let panel = [x0, y0, x0 + panel_w, y0 + panel_h];

    let icon_y = (y0 + pad + (header_h - icon_d) * 0.5).round();
    let icon = [x0 + pad, icon_y, x0 + pad + icon_d, icon_y + icon_d];
    let text_x = x0 + pad + icon_d + icon_gap;
    let text_y = (y0 + pad + (header_h - text_h) * 0.5).round();
    let lines = lines
        .into_iter()
        .enumerate()
        .map(|(i, spans)| TextRun {
            origin: [text_x, (text_y + i as f32 * ch).round()],
            spans,
        })
        .collect();

    let btn_y = (y0 + pad + header_h + BUTTON_ROW_GAP * s).round();
    let inner_x0 = (x0 + pad).min(panel[2]);
    let inner_x1 = (panel[2] - pad).max(inner_x0);
    let [cancel, close] = bm.row(&buttons, inner_x0, inner_x1, btn_y);
    Some(DialogLayout {
        scrim: [0.0, 0.0, m.surface.0.max(0.0), m.surface.1.max(0.0)],
        panel,
        icon,
        lines,
        cancel,
        close,
        scale: s,
    })
}

/// Colours of one button (straight alpha, linear light).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ButtonColors {
    pub fill: [f32; 4],
    pub ring: [f32; 4],
    /// Label text ([`Tone::Strong`]), opaque.
    pub label: [f32; 4],
    pub keycap_ring: [f32; 4],
    /// Keycap text ([`Tone::Muted`]), opaque.
    pub keycap_text: [f32; 4],
}

impl ButtonColors {
    /// Text colour for `tone` on this button.
    #[must_use]
    pub fn text(&self, tone: Tone) -> [f32; 4] {
        match tone {
            Tone::Strong => self.label,
            Tone::Muted => self.keycap_text,
        }
    }
}

/// Every colour the dialog uses (straight alpha, linear light). Text
/// colours are pre-mixed opaque against the surface they sit on, so
/// "72% fg" means 72% in sRGB, as a designer would read it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DialogColors {
    pub scrim: [f32; 4],
    pub shadow: [f32; 4],
    pub panel_fill: [f32; 4],
    pub panel_ring: [f32; 4],
    pub icon_fill: [f32; 4],
    pub icon_ring: [f32; 4],
    pub icon_stroke: [f32; 4],
    pub text_strong: [f32; 4],
    pub text_muted: [f32; 4],
    pub cancel: ButtonColors,
    pub close: ButtonColors,
    pub focus_ring: [f32; 4],
}

impl DialogColors {
    /// Panel text colour for `tone`.
    #[must_use]
    pub fn text(&self, tone: Tone) -> [f32; 4] {
        match tone {
            Tone::Strong => self.text_strong,
            Tone::Muted => self.text_muted,
        }
    }

    /// Colours of `button`.
    #[must_use]
    pub fn button(&self, button: DialogButton) -> &ButtonColors {
        match button {
            DialogButton::Cancel => &self.cancel,
            DialogButton::Close => &self.close,
        }
    }
}

/// Opaque `a` mixed `k` of the way toward `b`, in sRGB (`k > 1`
/// extrapolates past `b`, clamped to the gamut).
fn mix(from: [f32; 4], to: [f32; 4], k: f32) -> [f32; 4] {
    let rgb: [f32; 3] = std::array::from_fn(|i| {
        let x = linear_to_srgb(from[i]);
        let y = linear_to_srgb(to[i]);
        srgb_to_linear(x + (y - x) * k)
    });
    [rgb[0], rgb[1], rgb[2], 1.0]
}

/// The linear-light alpha that makes black over anything look like black
/// at `alpha` blended in sRGB (what "45% black" means in a design tool).
/// The framebuffer blends in linear light, where 45% black only takes
/// an sRGB 217 down to 166 instead of 119.
#[must_use]
pub fn black_overlay_alpha(alpha: f32) -> f32 {
    1.0 - srgb_to_linear(1.0 - alpha.clamp(0.0, 1.0))
}

fn with_alpha(c: [f32; 4], a: f32) -> [f32; 4] {
    [c[0], c[1], c[2], a]
}

/// Derive the dialog colours from the (linear-light) theme, in the
/// chip's visual language: surfaces just off the background, faint
/// foreground hairlines, and a foreground-filled default button.
/// `hover` lifts that button: Cancel moves further toward fg (like the
/// chip), and Close pushes past fg away from bg (more contrast in both
/// dark and light themes) with a brighter focus ring.
#[must_use]
pub fn dialog_colors(bg: [f32; 4], fg: [f32; 4], hover: Option<DialogButton>) -> DialogColors {
    let fg = with_alpha(fg, 1.0);
    let bg = with_alpha(bg, 1.0);
    let panel_fill = mix(bg, fg, 0.12);
    let icon_fill = mix(bg, fg, 0.22);
    let cancel_hover = hover == Some(DialogButton::Cancel);
    let close_hover = hover == Some(DialogButton::Close);
    let cancel_fill = mix(bg, fg, if cancel_hover { 0.24 } else { 0.175 });
    let close_fill = if close_hover { mix(bg, fg, 1.12) } else { fg };
    DialogColors {
        scrim: [0.0, 0.0, 0.0, black_overlay_alpha(SCRIM_ALPHA)],
        shadow: [0.0, 0.0, 0.0, black_overlay_alpha(SHADOW_ALPHA)],
        panel_fill,
        panel_ring: with_alpha(fg, 0.12),
        icon_fill,
        icon_ring: with_alpha(fg, 0.22),
        icon_stroke: mix(icon_fill, fg, 0.85),
        text_strong: fg,
        text_muted: mix(panel_fill, fg, 0.72),
        cancel: ButtonColors {
            fill: cancel_fill,
            ring: with_alpha(fg, if cancel_hover { 0.22 } else { 0.12 }),
            label: fg,
            keycap_ring: with_alpha(fg, 0.22),
            keycap_text: mix(cancel_fill, fg, 0.72),
        },
        close: ButtonColors {
            fill: close_fill,
            ring: with_alpha(close_fill, 0.0),
            label: bg,
            keycap_ring: with_alpha(bg, 0.30),
            keycap_text: mix(close_fill, bg, 0.72),
        },
        focus_ring: with_alpha(fg, if close_hover { 0.55 } else { 0.35 }),
    }
}

/// The dialog's shapes, back to front: scrim, shadow, panel, icon disc,
/// icon, then each button (the default one with its focus ring) and its
/// keycap (plus the return icon in Close's).
#[must_use]
#[allow(clippy::many_single_char_names)]
pub fn dialog_rects(l: &DialogLayout, c: &DialogColors) -> Vec<UiRect> {
    let s = l.scale;
    let hair = s.max(1.0);
    let clear = [0.0; 4];
    let offset = SHADOW_OFFSET_Y * s;
    let half_icon = (l.icon[2] - l.icon[0]) * 0.5;
    let mut v = vec![
        UiRect::rect(l.scrim, c.scrim, clear, 0.0, 0.0),
        UiRect {
            kind: UI_KIND_SHADOW,
            softness: SHADOW_BLUR * 0.5 * s,
            ..UiRect::rect(
                [
                    l.panel[0],
                    l.panel[1] + offset,
                    l.panel[2],
                    l.panel[3] + offset,
                ],
                c.shadow,
                clear,
                PANEL_RADIUS * s,
                0.0,
            )
        },
        UiRect::rect(l.panel, c.panel_fill, c.panel_ring, PANEL_RADIUS * s, hair),
        UiRect::rect(l.icon, c.icon_fill, c.icon_ring, half_icon, hair),
        UiRect {
            kind: UI_KIND_PROMPT_ICON,
            ..UiRect::rect(l.icon, c.icon_stroke, clear, half_icon, ICON_STROKE * s)
        },
    ];
    for b in l.buttons() {
        let bc = c.button(b.kind);
        if b.kind == DialogButton::Close {
            let o = (FOCUS_RING_OFFSET + FOCUS_RING_WIDTH) * s;
            let r = b.rect;
            v.push(UiRect::rect(
                [r[0] - o, r[1] - o, r[2] + o, r[3] + o],
                clear,
                c.focus_ring,
                BUTTON_RADIUS * s + o,
                FOCUS_RING_WIDTH * s,
            ));
        }
        v.push(UiRect::rect(
            b.rect,
            bc.fill,
            bc.ring,
            BUTTON_RADIUS * s,
            hair,
        ));
        if let Some(kc) = &b.keycap {
            v.push(UiRect::rect(
                kc.rect,
                clear,
                bc.keycap_ring,
                KEYCAP_RADIUS * s,
                hair,
            ));
            if let KeyGlyph::Return(icon) = kc.glyph {
                v.push(UiRect {
                    kind: UI_KIND_RETURN_ICON,
                    ..UiRect::rect(icon, bc.keycap_text, clear, 0.0, RETURN_STROKE * s)
                });
            }
        }
    }
    v
}

#[cfg(test)]
// Geometry is pixel-snapped and the expected values are exact.
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    fn srgb8(c: [f32; 4]) -> [u8; 3] {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        std::array::from_fn(|i| (linear_to_srgb(c[i]) * 255.0).round() as u8)
    }

    fn lin(hex: u32) -> [f32; 4] {
        let ch = |sh: u32| srgb_to_linear(f32::from(((hex >> sh) & 0xff) as u8) / 255.0);
        [ch(16), ch(8), ch(0), 1.0]
    }

    fn text(spans: &[Span]) -> String {
        spans.iter().map(|s| s.text.as_str()).collect()
    }

    fn metrics(w: f32, h: f32) -> DialogMetrics {
        DialogMetrics {
            surface: (w, h),
            content_origin: [0.0, 0.0],
            content: (w, h),
            cell: (10.0, 19.0),
            scale: 1.0,
        }
    }

    fn prompt(program: Option<&str>, running: bool) -> ClosePrompt {
        ClosePrompt {
            program: program.map(str::to_owned),
            running,
        }
    }

    #[test]
    fn copy_names_the_program() {
        let c = prompt_copy(&prompt(Some("sleep"), true));
        assert_eq!(text(&c.headline), "sleep is still running");
        assert_eq!(c.headline[0].tone, Tone::Strong);
        assert_eq!(c.headline[1].tone, Tone::Muted);
        assert_eq!(c.detail.as_ref().unwrap().text, DETAIL_TEXT);
    }

    #[test]
    fn copy_falls_back_without_a_name() {
        let c = prompt_copy(&prompt(None, true));
        assert_eq!(text(&c.headline), "A program is still running");
        assert_eq!(c.detail.as_ref().unwrap().text, DETAIL_TEXT);
        // Blank / control-only names count as unknown.
        assert_eq!(prompt_copy(&prompt(Some(" \t"), true)), c);
        assert_eq!(prompt_copy(&prompt(Some("\x1b"), true)), c);
    }

    #[test]
    fn copy_for_a_bare_shell() {
        let c = prompt_copy(&prompt(None, false));
        assert_eq!(text(&c.headline), "Close this window?");
        assert!(c.detail.is_none());
    }

    #[test]
    fn copy_strips_control_chars_from_the_name() {
        let c = prompt_copy(&prompt(Some("v\x07im\n"), true));
        assert_eq!(c.headline[0].text, "vim");
    }

    #[test]
    fn fit_spans_shortens_the_name_first() {
        let spans = vec![
            Span::new("averyveryverylongprogramname", Tone::Strong),
            Span::new(" is still running", Tone::Muted),
        ];
        let f = fit_spans(&spans, 25);
        assert_eq!(text(&f), "averyve… is still running");
        assert_eq!(spans_cols(&f), 25);
        assert_eq!(f[1].tone, Tone::Muted);
        // Too narrow for even that: cut at the end.
        let f = fit_spans(&spans, 10);
        assert_eq!(text(&f), "averyvery…");
        assert_eq!(spans_cols(&f), 10);
        assert!(fit_spans(&spans, 0).is_empty());
        // Fits: unchanged.
        assert_eq!(fit_spans(&spans, 100), spans);
    }

    #[test]
    fn fit_spans_counts_wide_chars_as_two_columns() {
        let spans = vec![Span::new("日本語のプログラム", Tone::Strong)];
        let f = fit_spans(&spans, 7);
        assert_eq!(text(&f), "日本語…");
        assert!(spans_cols(&f) <= 7);
    }

    #[test]
    fn layout_centres_a_420px_panel() {
        let copy = prompt_copy(&prompt(Some("sleep"), true));
        let l = dialog_layout(&copy, &metrics(1280.0, 800.0)).unwrap();
        assert_eq!(l.panel[2] - l.panel[0], 420.0);
        assert_eq!(l.panel[0], 430.0);
        let h = l.panel[3] - l.panel[1];
        // 18 + max(28, 2 × 19) + 18 + 32 + 18.
        assert_eq!(h, 18.0 + 38.0 + 18.0 + 32.0 + 18.0);
        assert_eq!(l.panel[1], ((800.0 - h) / 2.0_f32).round());
        assert_eq!(l.scrim, [0.0, 0.0, 1280.0, 800.0]);
        // Icon on the left, text after the gap.
        assert_eq!(l.icon[0], l.panel[0] + 18.0);
        assert_eq!(l.icon[2] - l.icon[0], 28.0);
        assert_eq!(l.lines.len(), 2);
        assert_eq!(l.lines[0].origin[0], l.icon[2] + 14.0);
        assert_eq!(l.lines[1].origin[1] - l.lines[0].origin[1], 19.0);
        assert_eq!(text(&l.lines[0].spans), "sleep is still running");
    }

    #[test]
    fn layout_buttons_right_aligned_with_keycaps() {
        let copy = prompt_copy(&prompt(None, true));
        let l = dialog_layout(&copy, &metrics(1280.0, 800.0)).unwrap();
        let (cancel, close) = (&l.cancel, &l.close);
        assert_eq!(close.rect[2], l.panel[2] - 18.0);
        assert_eq!(close.rect[0] - cancel.rect[2], 8.0);
        assert_eq!(close.rect[3] - close.rect[1], 32.0);
        assert_eq!(cancel.rect[1], close.rect[1]);
        // 18 below the text block's header row.
        assert_eq!(close.rect[1], l.panel[1] + 18.0 + 38.0 + 18.0);
        // Cancel: 12 + "Cancel" + 8 + keycap(5 + "Esc" + 5) + 8.
        assert_eq!(
            cancel.rect[2] - cancel.rect[0],
            12.0 + 60.0 + 8.0 + 40.0 + 8.0
        );
        let kc = cancel.keycap.as_ref().unwrap();
        assert_eq!(kc.rect[2] - kc.rect[0], 40.0);
        assert_eq!(kc.rect[2], cancel.rect[2] - 8.0);
        let KeyGlyph::Text(esc) = &kc.glyph else {
            panic!("Esc is text")
        };
        assert_eq!(esc.origin[1], cancel.label.origin[1]);
        assert_eq!(close.label.origin[0], close.rect[0] + 12.0);
        // Close's keycap holds the return icon, sized from the cell
        // height (0.75 × 19 → 14) and centred in the keycap.
        let kc = close.keycap.as_ref().unwrap();
        let KeyGlyph::Return(icon) = kc.glyph else {
            panic!("Close shows the return icon")
        };
        assert_eq!(icon[2] - icon[0], 14.0);
        assert_eq!(kc.rect[2] - kc.rect[0], 24.0);
        assert!(icon[0] >= kc.rect[0] && icon[2] <= kc.rect[2]);
        assert!(icon[1] >= kc.rect[1] && icon[3] <= kc.rect[3]);
        // Keycaps sit inside their button.
        for b in l.buttons() {
            let kc = b.keycap.as_ref().unwrap();
            assert!(kc.rect[0] > b.rect[0] && kc.rect[2] <= b.rect[2]);
            assert!(kc.rect[1] >= b.rect[1] && kc.rect[3] <= b.rect[3]);
        }
    }

    /// Everything in the button row stays inside the panel's padding,
    /// and every label inside its button, at any width.
    #[allow(clippy::cast_precision_loss)]
    fn assert_row_inside(l: &DialogLayout, cell_w: f32) {
        // (A panel narrower than its padding collapses the row to its edge.)
        let x0 = (l.panel[0] + 18.0).min(l.panel[2]);
        let x1 = (l.panel[2] - 18.0).max(x0);
        for b in l.buttons() {
            assert!(
                b.rect[0] >= x0 - 0.01 && b.rect[2] <= x1 + 0.01,
                "{b:?} in {x0}..{x1}"
            );
            assert!(b.rect[0] <= b.rect[2]);
            let w = spans_cols(&b.label.spans) as f32 * cell_w;
            if w > 0.0 {
                assert!(b.label.origin[0] >= b.rect[0], "{b:?}");
                assert!(b.label.origin[0] + w <= b.rect[2] + 0.5, "{b:?}");
            }
            if let Some(kc) = &b.keycap {
                assert!(kc.rect[0] >= b.rect[0] && kc.rect[2] <= b.rect[2]);
            }
        }
        assert!(l.cancel.rect[2] <= l.close.rect[0], "no overlap");
    }

    #[test]
    fn narrow_windows_drop_keycaps_then_squeeze_the_buttons() {
        let copy = prompt_copy(&prompt(Some("sleep"), true));
        // 330 wide: room for the keycaps.
        let l = dialog_layout(&copy, &metrics(330.0, 220.0)).unwrap();
        assert!(l.cancel.keycap.is_some() && l.close.keycap.is_some());
        assert_row_inside(&l, 10.0);
        // 260 wide: the full row (238) doesn't fit the 192 inner width,
        // so the keycaps go and the labels are centred.
        let l = dialog_layout(&copy, &metrics(260.0, 160.0)).unwrap();
        assert_eq!(l.panel[2] - l.panel[0], 228.0);
        assert!(l.cancel.keycap.is_none() && l.close.keycap.is_none());
        assert_eq!(text(&l.cancel.label.spans), "Cancel");
        assert_eq!(l.cancel.rect[2] - l.cancel.rect[0], 84.0);
        assert_row_inside(&l, 10.0);
        // 150 wide: even that doesn't fit; two equal buttons, truncated.
        let l = dialog_layout(&copy, &metrics(150.0, 160.0)).unwrap();
        assert!(l.cancel.keycap.is_none());
        let wc = l.cancel.rect[2] - l.cancel.rect[0];
        assert_eq!(wc, l.close.rect[2] - l.close.rect[0]);
        assert!(text(&l.cancel.label.spans).ends_with('…'));
        assert_row_inside(&l, 10.0);
        // And nothing ever leaves the panel, down to nothing at all.
        for w in (0..=1300).step_by(5) {
            #[allow(clippy::cast_precision_loss)]
            let l = dialog_layout(&copy, &metrics(w as f32, 300.0)).unwrap();
            assert_row_inside(&l, 10.0);
        }
    }

    #[test]
    fn single_line_copy_centres_text_on_the_icon() {
        let copy = prompt_copy(&prompt(None, false));
        let l = dialog_layout(&copy, &metrics(1280.0, 800.0)).unwrap();
        assert_eq!(l.lines.len(), 1);
        let icon_mid = f32::midpoint(l.icon[1], l.icon[3]);
        let text_mid = l.lines[0].origin[1] + 9.5;
        assert!((icon_mid - text_mid).abs() <= 0.5);
        assert_eq!(l.panel[3] - l.panel[1], 18.0 + 28.0 + 18.0 + 32.0 + 18.0);
    }

    #[test]
    fn layout_grows_with_the_font_and_scale() {
        let copy = prompt_copy(&prompt(Some("sleep"), true));
        let mut m = metrics(2560.0, 1600.0);
        m.cell = (20.0, 38.0);
        m.scale = 2.0;
        let l = dialog_layout(&copy, &m).unwrap();
        // 2 × 420, still enough for 29 cols × 20 + icon + gap + padding.
        assert_eq!(l.panel[2] - l.panel[0], 840.0);
        assert_eq!(l.close.rect[3] - l.close.rect[1], 64.0);
        // A big font at scale 1 widens the panel to fit the text.
        m.scale = 1.0;
        m.cell = (16.0, 30.0);
        let l = dialog_layout(&copy, &m).unwrap();
        assert!(l.panel[2] - l.panel[0] >= 18.0 * 2.0 + 28.0 + 14.0 + 29.0 * 16.0);
        assert_eq!(text(&l.lines[1].spans), DETAIL_TEXT);
        assert!(l.close.rect[3] - l.close.rect[1] >= 30.0 + 12.0);
    }

    #[test]
    fn layout_applies_the_content_origin() {
        let copy = prompt_copy(&prompt(None, true));
        let mut m = metrics(1000.0, 700.0);
        m.content_origin = [20.0, 30.0];
        m.content = (960.0, 640.0);
        let l = dialog_layout(&copy, &m).unwrap();
        assert_eq!(l.panel[0], 20.0 + (960.0 - 420.0) / 2.0);
        assert_eq!(l.scrim, [0.0, 0.0, 1000.0, 700.0]);
    }

    #[test]
    fn small_windows_squeeze_and_truncate() {
        let copy = prompt_copy(&prompt(Some("a-rather-long-program-name"), true));
        let l = dialog_layout(&copy, &metrics(300.0, 120.0)).unwrap();
        assert_eq!(l.panel[2] - l.panel[0], 300.0 - 32.0);
        assert_row_inside(&l, 10.0);
        let avail = ((268.0 - 36.0 - 28.0 - 14.0) / 10.0_f32).floor() as usize;
        for line in &l.lines {
            assert!(spans_cols(&line.spans) <= avail, "{:?}", line.spans);
        }
        assert!(text(&l.lines[0].spans).ends_with("… is still running"));
        // Taller than the window: pinned to the top, not negative.
        assert_eq!(l.panel[1], 0.0);
    }

    #[test]
    fn tiny_or_degenerate_metrics_never_panic() {
        let copy = prompt_copy(&prompt(Some("vim"), true));
        for (w, h) in [(0.0, 0.0), (1.0, 1.0), (10.0, 500.0), (500.0, 5.0)] {
            let l = dialog_layout(&copy, &metrics(w, h)).unwrap();
            assert!(l.panel[2] >= l.panel[0]);
            let _ = dialog_rects(&l, &dialog_colors([0.0; 4], [1.0; 4], None));
            let _ = l.button_at(0.0, 0.0);
        }
        let mut m = metrics(800.0, 600.0);
        m.scale = 0.0;
        assert!(dialog_layout(&copy, &m).is_none());
        m.scale = f32::NAN;
        assert!(dialog_layout(&copy, &m).is_none());
        let mut m = metrics(800.0, 600.0);
        m.cell = (0.0, 19.0);
        assert!(dialog_layout(&copy, &m).is_none());
        let mut m = metrics(f32::INFINITY, 600.0);
        m.cell = (10.0, 19.0);
        assert!(dialog_layout(&copy, &m).is_none());
    }

    #[test]
    fn hit_testing_matches_the_button_rects() {
        let copy = prompt_copy(&prompt(None, true));
        let l = dialog_layout(&copy, &metrics(1280.0, 800.0)).unwrap();
        let mid = |r: [f32; 4]| (f32::midpoint(r[0], r[2]), f32::midpoint(r[1], r[3]));
        let (x, y) = mid(l.close.rect);
        assert_eq!(l.button_at(x, y), Some(DialogButton::Close));
        let (x, y) = mid(l.cancel.rect);
        assert_eq!(l.button_at(x, y), Some(DialogButton::Cancel));
        // Edges: x0 inclusive, x1 exclusive; the gap between is neither.
        let r = l.close.rect;
        assert_eq!(l.button_at(r[0], r[1]), Some(DialogButton::Close));
        assert_eq!(l.button_at(r[2], r[1]), None);
        assert_eq!(l.button_at(r[0] - 4.0, y), None);
        // The panel itself and the scrim aren't buttons.
        assert_eq!(l.button_at(l.panel[0] + 2.0, l.panel[1] + 2.0), None);
        assert_eq!(l.button_at(1.0, 1.0), None);
    }

    #[test]
    fn default_theme_colours_match_the_design() {
        let bg = lin(0x12_12_14);
        let fg = lin(0xd9_d9_d9);
        let c = dialog_colors(bg, fg, None);
        let near = |got: [u8; 3], want: [u8; 3]| {
            assert!(
                got.iter().zip(want).all(|(g, w)| g.abs_diff(w) <= 1),
                "{got:02x?} vs {want:02x?}"
            );
        };
        near(srgb8(c.panel_fill), [0x2a, 0x2a, 0x2c]);
        near(srgb8(c.cancel.fill), [0x35, 0x35, 0x37]);
        near(srgb8(c.close.fill), [0xd9, 0xd9, 0xd9]);
        near(srgb8(c.close.label), [0x12, 0x12, 0x14]);
        near(srgb8(c.icon_fill), [0x3e, 0x3e, 0x40]);
        assert_eq!(c.panel_ring[3], 0.12);
        assert_eq!(c.icon_ring[3], 0.22);
        assert_eq!(c.cancel.keycap_ring[3], 0.22);
        assert_eq!(c.close.keycap_ring[3], 0.30);
        assert_eq!(c.focus_ring[3], 0.35);
        // 45% black in sRGB terms: fg text under the scrim ends up at
        // 55% of its sRGB value.
        assert_eq!(&c.scrim[..3], &[0.0; 3]);
        let dimmed = linear_to_srgb(fg[0] * (1.0 - c.scrim[3]));
        assert!(
            (dimmed - 0.55 * linear_to_srgb(fg[0])).abs() < 0.01,
            "{dimmed}"
        );
        // 72% from the panel toward fg, in sRGB: 42 + 0.72 × (217 − 42).
        assert_eq!(srgb8(c.text_muted)[0], 168);
        assert_eq!(c.text(Tone::Strong), fg);
        for col in [c.panel_fill, c.text_muted, c.cancel.fill, c.close.fill] {
            assert_eq!(col[3], 1.0, "surfaces and text are opaque");
        }
    }

    #[test]
    fn hover_lifts_the_hovered_button_only() {
        let bg = lin(0x12_12_14);
        let fg = lin(0xd9_d9_d9);
        let n = dialog_colors(bg, fg, None);
        let hc = dialog_colors(bg, fg, Some(DialogButton::Cancel));
        assert!(hc.cancel.fill[0] > n.cancel.fill[0]);
        assert!(hc.cancel.ring[3] > n.cancel.ring[3]);
        assert_eq!(hc.close, n.close);
        let hx = dialog_colors(bg, fg, Some(DialogButton::Close));
        assert!(
            hx.close.fill[0] > n.close.fill[0],
            "brighter on a dark theme"
        );
        assert!(hx.focus_ring[3] > n.focus_ring[3]);
        assert_eq!(hx.cancel, n.cancel);
        // On a light theme the Close lift goes darker: more contrast.
        let lx = dialog_colors(fg, bg, Some(DialogButton::Close));
        let ln = dialog_colors(fg, bg, None);
        assert!(lx.close.fill[0] < ln.close.fill[0]);
    }

    #[test]
    fn transparent_theme_bg_still_gives_an_opaque_panel() {
        let c = dialog_colors([0.0, 0.0, 0.0, 0.3], [1.0; 4], None);
        assert_eq!(c.panel_fill[3], 1.0);
        assert_eq!(c.close.label[3], 1.0);
    }

    #[test]
    fn rects_stack_back_to_front() {
        let copy = prompt_copy(&prompt(Some("sleep"), true));
        let l = dialog_layout(&copy, &metrics(1280.0, 800.0)).unwrap();
        let c = dialog_colors(lin(0x12_12_14), lin(0xd9_d9_d9), None);
        let r = dialog_rects(&l, &c);
        assert_eq!(r.len(), 11);
        assert_eq!(r[0].rect, l.scrim);
        assert_eq!(r[1].kind, UI_KIND_SHADOW);
        assert_eq!(r[1].rect[1], l.panel[1] + 12.0);
        assert_eq!(r[2].rect, l.panel);
        assert_eq!(r[2].radius, 14.0);
        assert_eq!(r[4].kind, UI_KIND_PROMPT_ICON);
        // Cancel, its keycap, Close's focus ring, Close, its keycap and
        // the return icon in it.
        let kc = |b: &ButtonLayout| b.keycap.as_ref().unwrap().rect;
        assert_eq!(r[5].rect, l.cancel.rect);
        assert_eq!(r[6].rect, kc(&l.cancel));
        assert_eq!(r[7].rect[0], l.close.rect[0] - 4.0);
        assert_eq!(r[7].border_width, 2.0);
        assert_eq!(r[8].rect, l.close.rect);
        assert_eq!(r[9].rect, kc(&l.close));
        assert_eq!(r[10].kind, UI_KIND_RETURN_ICON);
        assert_eq!(r[10].fill, c.close.keycap_text);
        // Without keycaps (narrow), no keycap rects or icon.
        let l = dialog_layout(&copy, &metrics(260.0, 160.0)).unwrap();
        assert_eq!(dialog_rects(&l, &c).len(), 8);
    }
}
