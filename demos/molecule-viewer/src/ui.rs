//! ratatui widgets for the molecule viewer.
//!
//! Draws the chrome — input box, bordered viewport region (left blank so
//! the RGP molecule shows through), candidate disambiguation list, and
//! the status/help line. The viewport `Rect` is converted to an RGP
//! anchor (1-based centre cell + cell span) by `viewport_anchor`.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};

use crate::pubchem::Candidate;

/// Application mode forwarded from `app` so we know what chrome to draw.
pub enum DrawMode<'a> {
    /// Plain input — show an empty viewport.
    Input,
    /// Fetching from network.
    Loading,
    /// Show a disambiguation list overlaid on the viewport area.
    Disambiguate {
        candidates: &'a [Candidate],
        list_state: &'a mut ListState,
    },
    /// A molecule is placed — viewport stays blank (RGP renders through it).
    Viewing,
}

/// RGP anchor derived from the viewport rect.
///
/// `col` and `row` are **1-based** centre cells; `w`/`h` are the cell span.
pub struct RgpAnchor {
    pub col: u16,
    pub row: u16,
    pub w: u16,
    pub h: u16,
}

/// Convert a viewport [`Rect`] to an RGP anchor.
///
/// The centre cell is `(area.x + area.width / 2 + 1, area.y + area.height / 2 + 1)`
/// (1-based terminal coordinates).  The cell span is `area.width × area.height`.
pub fn viewport_anchor(area: Rect) -> RgpAnchor {
    RgpAnchor {
        col: area.x + area.width / 2 + 1,
        row: area.y + area.height / 2 + 1,
        w: area.width,
        h: area.height,
    }
}

/// Split the frame into the three regions and render chrome.
///
/// Returns the inner `Rect` of the viewport block (excluding its border) so
/// that the caller can compute the RGP anchor.
pub fn draw(frame: &mut Frame<'_>, input: &str, mode: DrawMode<'_>) -> Rect {
    // ── vertical layout ──────────────────────────────────────────────────
    // 1. Input box  — 3 rows (border + 1 content line + border)
    // 2. Viewport   — fills remaining space
    // 3. Status bar — 1 row
    let [input_area, viewport_area, status_area] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    // ── input box ────────────────────────────────────────────────────────
    let input_block = Block::bordered().title(" Formula / name / CID ");
    let input_inner = input_block.inner(input_area);
    frame.render_widget(input_block, input_area);

    let input_widget = Paragraph::new(input);
    frame.render_widget(input_widget, input_inner);

    // Show a blinking cursor at end of text in Input/Loading states.
    match mode {
        DrawMode::Input | DrawMode::Loading => {
            let x = input_inner.x + input.len() as u16;
            let y = input_inner.y;
            if x < input_inner.x + input_inner.width {
                frame.set_cursor_position((x, y));
            }
        }
        _ => {}
    }

    // ── status help string (determined before mode is consumed) ──────────
    let help = match &mode {
        DrawMode::Input => " [Enter] search   [paste] ok   [Esc] quit",
        DrawMode::Loading => " Searching PubChem…",
        DrawMode::Disambiguate { .. } => " [↑/↓] navigate   [Enter] pick   [q/Esc] cancel",
        DrawMode::Viewing => {
            " [drag] rotate   [scroll] zoom   [a] animate   [r] reset   [/] new search   [q] quit"
        }
    };

    // ── viewport ──────────────────────────────────────────────────────────
    let viewport_block = Block::bordered().title(" 3D View ");
    let viewport_inner = viewport_block.inner(viewport_area);

    match mode {
        DrawMode::Disambiguate {
            candidates,
            list_state,
        } => {
            // Overlay the viewport with a candidate list.
            let items: Vec<ListItem<'_>> = candidates
                .iter()
                .map(|c| {
                    ListItem::new(Line::from(format!(
                        "{} — {} ({})",
                        c.title, c.formula, c.iupac
                    )))
                })
                .collect();

            let list = List::new(items)
                .block(Block::bordered().title(" Did you mean… "))
                .highlight_style(
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("▶ ");

            frame.render_stateful_widget(list, viewport_area, list_state);
        }
        DrawMode::Loading => {
            frame.render_widget(viewport_block, viewport_area);
            let spinner = Paragraph::new("  Fetching from PubChem…").style(Style::default().dim());
            frame.render_widget(spinner, viewport_inner);
        }
        _ => {
            // Input or Viewing: leave viewport interior blank.
            frame.render_widget(viewport_block, viewport_area);
        }
    }

    // ── status / help ────────────────────────────────────────────────────
    let status_widget = Paragraph::new(help).style(Style::default().dim());
    frame.render_widget(status_widget, status_area);

    viewport_inner
}
