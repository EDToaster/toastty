//! Per-cell GPU instance buffer construction.
//!
//! `build_instances` walks the active grid plus the cursor and emits one
//! [`CellInstance`] per drawable cell. The cursor is rendered as part of
//! this pass — an inverted-color block at the current cursor position.
//!
//! Pure CPU: no GPU types in or out. The pipeline crate uploads the
//! resulting `Vec<CellInstance>` to the vertex buffer.

use bytemuck::{Pod, Zeroable};
use toastty_term::{Cell, Color as TColor, Style, Term};

/// Flag bit: instance is the text-cursor block (forces inverse rendering,
/// no glyph sample).
pub const FLAG_CURSOR: u32 = 1 << 0;

/// Flag bit: glyph is sampled from the color (BGRA) atlas, not the mask
/// (R8) atlas. The shader uses this to pick the sampler & blend mode.
pub const FLAG_COLOR_GLYPH: u32 = 1 << 1;

/// Flag bit: this instance has no glyph at all (background-only fill).
pub const FLAG_NO_GLYPH: u32 = 1 << 2;

/// GPU-side instance layout. `repr(C)` + `Pod + Zeroable` so it round-trips
/// through `bytemuck::cast_slice` cleanly.
///
/// Field order matches the WGSL vertex pulling layout in `shaders/text.wgsl`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct CellInstance {
    /// Pixel position of the cell's top-left corner.
    pub pos: [f32; 2],
    /// Cell size in pixels (width, height). Same for every instance in
    /// M4b (no proportional fonts).
    pub size: [f32; 2],
    /// UV min in atlas-pixel coords (use `0..atlas_w`, `0..atlas_h`).
    /// Set to `[0, 0]` for cursor / background-only instances.
    pub uv_min: [f32; 2],
    /// UV max in atlas-pixel coords. Set to `uv_min` to signal "no glyph"
    /// (and the shader will skip the texture sample).
    pub uv_max: [f32; 2],
    /// Foreground color, linear RGBA.
    pub fg: [f32; 4],
    /// Background color, linear RGBA.
    pub bg: [f32; 4],
    /// Bit flags — see `FLAG_*` constants.
    pub flags: u32,
    /// Padding to 16-byte alignment for WGSL. Unused by the shader.
    pub pad: [u32; 3],
}

impl CellInstance {
    /// Construct a glyphless background-only instance.
    #[must_use]
    pub fn background(pos: [f32; 2], size: [f32; 2], bg: [f32; 4]) -> Self {
        Self {
            pos,
            size,
            uv_min: [0.0, 0.0],
            uv_max: [0.0, 0.0],
            fg: [1.0, 1.0, 1.0, 1.0],
            bg,
            flags: FLAG_NO_GLYPH,
            pad: [0; 3],
        }
    }

    /// Construct the cursor instance: full block at `pos`, inverted fg/bg.
    /// The shader treats this as a full-cell solid fill.
    #[must_use]
    pub fn cursor(pos: [f32; 2], size: [f32; 2], cursor_color: [f32; 4]) -> Self {
        Self {
            pos,
            size,
            uv_min: [0.0, 0.0],
            uv_max: [0.0, 0.0],
            fg: [0.0, 0.0, 0.0, 1.0],
            bg: cursor_color,
            flags: FLAG_CURSOR | FLAG_NO_GLYPH,
            pad: [0; 3],
        }
    }
}

/// A glyph located in the atlas, ready to plug into a `CellInstance`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlyphSlot {
    /// UV min in atlas-pixel coords.
    pub uv_min: [f32; 2],
    /// UV max in atlas-pixel coords.
    pub uv_max: [f32; 2],
    /// True if the glyph is sampled from the color atlas.
    pub is_color: bool,
}

/// Default theme — used when [`TColor::Default`] is in play and the
/// terminal doesn't override.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub fg: [f32; 4],
    pub bg: [f32; 4],
    pub cursor: [f32; 4],
    pub palette: [[f32; 4]; 16],
}

impl Theme {
    /// Sensible default palette — VGA-ish, with a slate-blue cursor.
    #[must_use]
    pub fn default_dark() -> Self {
        Self {
            fg: [0.85, 0.85, 0.85, 1.0],
            bg: [0.07, 0.07, 0.09, 1.0],
            cursor: [0.95, 0.85, 0.30, 1.0],
            palette: DEFAULT_PALETTE_LINEAR,
        }
    }

    #[must_use]
    pub fn resolve_fg(&self, c: TColor) -> [f32; 4] {
        match c {
            TColor::Default => self.fg,
            other => self.palette[palette_index(other)],
        }
    }

    #[must_use]
    pub fn resolve_bg(&self, c: TColor) -> [f32; 4] {
        match c {
            TColor::Default => self.bg,
            other => self.palette[palette_index(other)],
        }
    }
}

fn palette_index(c: TColor) -> usize {
    match c {
        TColor::Default | TColor::Black => 0,
        TColor::Red => 1,
        TColor::Green => 2,
        TColor::Yellow => 3,
        TColor::Blue => 4,
        TColor::Magenta => 5,
        TColor::Cyan => 6,
        TColor::White => 7,
        TColor::BrightBlack => 8,
        TColor::BrightRed => 9,
        TColor::BrightGreen => 10,
        TColor::BrightYellow => 11,
        TColor::BrightBlue => 12,
        TColor::BrightMagenta => 13,
        TColor::BrightCyan => 14,
        TColor::BrightWhite => 15,
    }
}

/// Linear-light approximations of a VGA-ish palette. Values are
/// gamma-corrected sRGB → linear so the shader (writing into an sRGB
/// swapchain) produces faithful colors.
const DEFAULT_PALETTE_LINEAR: [[f32; 4]; 16] = [
    [0.000, 0.000, 0.000, 1.0],            // Black
    [0.500, 0.020, 0.020, 1.0],            // Red
    [0.020, 0.350, 0.020, 1.0],            // Green
    [0.500, 0.350, 0.020, 1.0],            // Yellow
    [0.020, 0.020, 0.500, 1.0],            // Blue
    [0.350, 0.020, 0.350, 1.0],            // Magenta
    [0.020, 0.350, 0.500, 1.0],            // Cyan
    [0.700, 0.700, 0.700, 1.0],            // White
    [0.250, 0.250, 0.250, 1.0],            // BrightBlack (dark gray)
    [0.900, 0.150, 0.150, 1.0],            // BrightRed
    [0.150, 0.750, 0.150, 1.0],            // BrightGreen
    [0.900, 0.750, 0.150, 1.0],            // BrightYellow
    [0.250, 0.350, 0.900, 1.0],            // BrightBlue
    [0.700, 0.150, 0.700, 1.0],            // BrightMagenta
    [0.150, 0.700, 0.750, 1.0],            // BrightCyan
    [1.000, 1.000, 1.000, 1.0],            // BrightWhite
];

/// True if the cell holds nothing worth drawing.
///
/// "Nothing worth drawing" = a space character on the default background,
/// no style flags set. A cell with a non-default `bg` still emits an
/// instance so the colored stripe shows up.
#[must_use]
pub fn is_blank_for_render(cell: &Cell) -> bool {
    cell.ch.is_whitespace()
        && cell.style.bg == TColor::Default
        && cell.style.fg == TColor::Default
        && !cell.style.flags.reverse
        && !cell.style.flags.underline
}

/// Effective fg/bg for a cell after applying SGR `reverse` (mode 7).
#[must_use]
pub fn resolve_cell_colors(cell: &Cell, theme: &Theme) -> ([f32; 4], [f32; 4]) {
    let fg = theme.resolve_fg(cell.style.fg);
    let bg = theme.resolve_bg(cell.style.bg);
    if cell.style.flags.reverse {
        (bg, fg)
    } else {
        (fg, bg)
    }
}

/// Build the GPU instance buffer for `term`.
///
/// - `cell_size` — `(width, height)` in pixels.
/// - `theme` — palette + cursor color.
/// - `locate_glyph` — closure called for each non-blank cell to look up
///   the cell's primary glyph in the atlas. Receives `(row, col, char,
///   style)`. Returns `None` if the glyph isn't atlassed yet (which the
///   caller treats as "skip" or "rasterize on demand"). Tests pass an
///   `|_, _, _, _| None` closure so the math is exercised without a
///   real atlas.
///
/// Cursor is the **last** instance in the returned vec — guarantees it
/// renders on top of any cell at the same coordinates.
pub fn build_instances<F>(
    term: &Term,
    cell_size: (f32, f32),
    theme: &Theme,
    mut locate_glyph: F,
) -> Vec<CellInstance>
where
    F: FnMut(u16, u16, char, &Style) -> Option<GlyphSlot>,
{
    let (rows, cols) = term.size();
    let mut out: Vec<CellInstance> = Vec::with_capacity(usize::from(rows) * usize::from(cols));

    let cell_w = cell_size.0;
    let cell_h = cell_size.1;

    for r in 0..rows {
        let row = term.row(r);
        for c in 0..cols {
            let Some(cell) = row.cells.get(c as usize) else {
                continue;
            };

            if is_blank_for_render(cell) {
                continue;
            }

            let pos = [f32::from(c) * cell_w, f32::from(r) * cell_h];
            let (fg, bg) = resolve_cell_colors(cell, theme);

            let glyph = if cell.ch.is_whitespace() {
                None
            } else {
                locate_glyph(r, c, cell.ch, &cell.style)
            };

            match glyph {
                Some(slot) => {
                    let flags = if slot.is_color {
                        FLAG_COLOR_GLYPH
                    } else {
                        0
                    };
                    out.push(CellInstance {
                        pos,
                        size: [cell_w, cell_h],
                        uv_min: slot.uv_min,
                        uv_max: slot.uv_max,
                        fg,
                        bg,
                        flags,
                        pad: [0; 3],
                    });
                }
                None => {
                    // No atlas slot yet — emit a background instance
                    // that still carries the resolved fg/bg so the
                    // dispatcher sees the SGR state. The glyph will
                    // appear on a later frame.
                    out.push(CellInstance {
                        pos,
                        size: [cell_w, cell_h],
                        uv_min: [0.0, 0.0],
                        uv_max: [0.0, 0.0],
                        fg,
                        bg,
                        flags: FLAG_NO_GLYPH,
                        pad: [0; 3],
                    });
                }
            }
        }
    }

    // Append cursor as the last instance. Clamp position into the grid.
    let cur = term.cursor();
    let cur_col = u16::min(cur.col, cols.saturating_sub(1));
    let cur_row = u16::min(cur.row, rows.saturating_sub(1));
    let pos = [f32::from(cur_col) * cell_w, f32::from(cur_row) * cell_h];
    out.push(CellInstance::cursor(pos, [cell_w, cell_h], theme.cursor));

    out
}

#[cfg(test)]
#[allow(clippy::float_cmp, clippy::similar_names)] // palette constants are exact bytes
mod tests {
    use super::*;
    use toastty_parser::Parser;
    use toastty_term::Term;

    fn feed(t: &mut Term, bytes: &[u8]) {
        let mut p = Parser::new();
        p.advance(t, bytes);
    }

    fn count_non_cursor(instances: &[CellInstance]) -> usize {
        instances.iter().filter(|i| i.flags & FLAG_CURSOR == 0).count()
    }

    fn cursor_instance(instances: &[CellInstance]) -> &CellInstance {
        instances
            .iter()
            .find(|i| i.flags & FLAG_CURSOR != 0)
            .expect("missing cursor instance")
    }

    #[test]
    fn empty_term_emits_only_cursor() {
        let t = Term::new(3, 5, 0);
        let theme = Theme::default_dark();
        let v = build_instances(&t, (8.0, 16.0), &theme, |_, _, _, _| None);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].flags & FLAG_CURSOR, FLAG_CURSOR);
    }

    #[test]
    fn each_visible_char_produces_an_instance_plus_cursor() {
        let mut t = Term::new(2, 8, 0);
        feed(&mut t, b"hello");
        let theme = Theme::default_dark();
        let v = build_instances(&t, (8.0, 16.0), &theme, |_, _, _, _| None);
        // 5 character cells + cursor.
        assert_eq!(v.len(), 6);
        assert_eq!(count_non_cursor(&v), 5);
    }

    #[test]
    fn blank_cells_produce_no_instance() {
        // Empty grid: cells are spaces with default style; nothing should
        // be emitted apart from the cursor.
        let t = Term::new(2, 8, 0);
        let v = build_instances(&t, (8.0, 16.0), &Theme::default_dark(), |_, _, _, _| None);
        assert_eq!(count_non_cursor(&v), 0);
    }

    #[test]
    fn blank_cell_with_non_default_bg_still_emits() {
        let mut t = Term::new(1, 4, 0);
        // Set bg to red, then write a space.
        feed(&mut t, b"\x1b[41m ");
        let v = build_instances(&t, (8.0, 16.0), &Theme::default_dark(), |_, _, _, _| None);
        // 1 background-colored cell + cursor.
        assert_eq!(count_non_cursor(&v), 1);
    }

    #[test]
    fn cursor_position_follows_csi_h() {
        let mut t = Term::new(10, 10, 0);
        feed(&mut t, b"\x1b[5;10H"); // row 5 col 10 (1-based)
        let v = build_instances(&t, (8.0, 16.0), &Theme::default_dark(), |_, _, _, _| None);
        let cur = cursor_instance(&v);
        // 5,10 1-based → row 4, col 9 0-based.
        assert!((cur.pos[0] - 9.0 * 8.0).abs() < 1e-3);
        assert!((cur.pos[1] - 4.0 * 16.0).abs() < 1e-3);
    }

    #[test]
    fn cursor_clamped_into_grid() {
        let t = Term::new(2, 3, 0);
        let v = build_instances(&t, (8.0, 16.0), &Theme::default_dark(), |_, _, _, _| None);
        let cur = cursor_instance(&v);
        assert!(cur.pos[0] < 3.0 * 8.0);
        assert!(cur.pos[1] < 2.0 * 16.0);
    }

    #[test]
    fn sgr_reverse_swaps_fg_and_bg() {
        let theme = Theme::default_dark();
        let mut t = Term::new(1, 4, 0);
        // Non-reverse: red fg.
        feed(&mut t, b"\x1b[31mA");
        let v = build_instances(&t, (8.0, 16.0), &theme, |_, _, _, _| None);
        let normal = v
            .iter()
            .find(|i| i.flags & FLAG_CURSOR == 0)
            .expect("non-cursor instance");
        let normal_fg = normal.fg;
        let normal_bg = normal.bg;

        // Reverse: same red, but inverted onto bg.
        let mut t2 = Term::new(1, 4, 0);
        feed(&mut t2, b"\x1b[7;31mA");
        let v2 = build_instances(&t2, (8.0, 16.0), &theme, |_, _, _, _| None);
        let rev = v2
            .iter()
            .find(|i| i.flags & FLAG_CURSOR == 0)
            .expect("non-cursor instance");
        // Fg/bg must swap.
        assert_eq!(rev.fg, normal_bg);
        assert_eq!(rev.bg, normal_fg);
    }

    #[test]
    fn glyph_locator_is_called_for_non_whitespace_only() {
        let mut t = Term::new(1, 8, 0);
        feed(&mut t, b"a b"); // "a", " ", "b" — space should not call the locator
        let mut seen = Vec::new();
        let _ = build_instances(&t, (8.0, 16.0), &Theme::default_dark(), |_, _, ch, _| {
            seen.push(ch);
            None
        });
        assert!(seen.contains(&'a'));
        assert!(seen.contains(&'b'));
        assert!(!seen.contains(&' '));
    }

    #[test]
    fn color_glyph_flag_is_set_when_slot_is_color() {
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"X");
        let v = build_instances(&t, (8.0, 16.0), &Theme::default_dark(), |_, _, _, _| {
            Some(GlyphSlot {
                uv_min: [0.0, 0.0],
                uv_max: [8.0, 16.0],
                is_color: true,
            })
        });
        let glyph_inst = v
            .iter()
            .find(|i| i.flags & FLAG_CURSOR == 0)
            .expect("expected glyph instance");
        assert!(glyph_inst.flags & FLAG_COLOR_GLYPH != 0);
        assert!(glyph_inst.flags & FLAG_NO_GLYPH == 0);
    }

    #[test]
    fn theme_resolves_default_colors_to_theme_values() {
        let theme = Theme::default_dark();
        assert_eq!(theme.resolve_fg(TColor::Default), theme.fg);
        assert_eq!(theme.resolve_bg(TColor::Default), theme.bg);
    }

    #[test]
    fn theme_resolves_palette_indexed_colors() {
        let theme = Theme::default_dark();
        let red = theme.resolve_fg(TColor::Red);
        let bright_red = theme.resolve_fg(TColor::BrightRed);
        assert_ne!(red, theme.fg);
        assert_ne!(red, bright_red);
        assert_ne!(theme.resolve_fg(TColor::Green), theme.resolve_fg(TColor::Red));
    }

    #[test]
    fn cursor_instance_has_no_glyph_flag() {
        let t = Term::new(2, 2, 0);
        let v = build_instances(&t, (8.0, 16.0), &Theme::default_dark(), |_, _, _, _| None);
        let cur = cursor_instance(&v);
        assert!(cur.flags & FLAG_NO_GLYPH != 0);
        assert!(cur.flags & FLAG_CURSOR != 0);
    }

    #[test]
    fn is_blank_for_render_truth_table() {
        assert!(is_blank_for_render(&Cell::BLANK));
        let mut c = Cell::BLANK;
        c.style.bg = TColor::Red;
        assert!(!is_blank_for_render(&c));
        let mut c = Cell::BLANK;
        c.style.flags.reverse = true;
        assert!(!is_blank_for_render(&c));
        let mut c = Cell::BLANK;
        c.style.flags.underline = true;
        assert!(!is_blank_for_render(&c));
        let mut c = Cell::BLANK;
        c.ch = 'a';
        assert!(!is_blank_for_render(&c));
    }

    #[test]
    fn instances_are_pod_and_round_trip_through_bytemuck() {
        let i = CellInstance::cursor([0.0, 0.0], [8.0, 16.0], [1.0, 0.0, 0.0, 1.0]);
        let bytes = bytemuck::bytes_of(&i);
        // Size should be a multiple of 16 for WGSL alignment.
        assert_eq!(bytes.len() % 16, 0);
        let round_trip: CellInstance = *bytemuck::from_bytes(bytes);
        assert_eq!(round_trip, i);
    }

    #[test]
    fn glyph_locator_returning_none_emits_background_instance() {
        // A character with no atlas slot should still produce one
        // instance per character (background fill); the glyph will fill
        // in on a later frame.
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"a");
        let v = build_instances(&t, (8.0, 16.0), &Theme::default_dark(), |_, _, _, _| None);
        assert_eq!(count_non_cursor(&v), 1);
        let bg_inst = v
            .iter()
            .find(|i| i.flags & FLAG_CURSOR == 0)
            .unwrap();
        assert!(bg_inst.flags & FLAG_NO_GLYPH != 0);
    }

    #[test]
    fn resolve_cell_colors_swaps_under_reverse() {
        let theme = Theme::default_dark();
        let mut c = Cell::BLANK;
        c.style.fg = TColor::Red;
        c.style.bg = TColor::Default;
        c.style.flags.reverse = true;
        let (fg, bg) = resolve_cell_colors(&c, &theme);
        assert_eq!(fg, theme.bg);
        assert_eq!(bg, theme.resolve_fg(TColor::Red));
    }

    #[test]
    fn palette_indexes_cover_all_color_variants() {
        // Exercise the lookup so any unhandled enum variant panics on
        // the slice index. (Defensive — `TColor::Default` returns the
        // theme fg/bg, not palette[0], inside resolve_fg/bg.)
        let theme = Theme::default_dark();
        let vs = [
            TColor::Black, TColor::Red, TColor::Green, TColor::Yellow, TColor::Blue,
            TColor::Magenta, TColor::Cyan, TColor::White,
            TColor::BrightBlack, TColor::BrightRed, TColor::BrightGreen, TColor::BrightYellow,
            TColor::BrightBlue, TColor::BrightMagenta, TColor::BrightCyan, TColor::BrightWhite,
        ];
        for c in vs {
            let v = theme.resolve_fg(c);
            assert!(v[3] > 0.99);
        }
    }
}
