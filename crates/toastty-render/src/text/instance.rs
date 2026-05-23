//! Per-cell GPU instance buffer construction.
//!
//! `build_instances` walks the active grid plus the cursor and emits one
//! [`CellInstance`] per drawable cell. The cursor is rendered as part of
//! this pass — an inverted-color block at the current cursor position.
//!
//! Pure CPU: no GPU types in or out. The pipeline crate uploads the
//! resulting `Vec<CellInstance>` to the vertex buffer.

use bytemuck::{Pod, Zeroable};
use toastty_term::{Cell, Color as TColor, CursorShape, Style, Term};

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
    ///
    /// This is the default "block" cursor. For runtime cursor-shape
    /// switching (DECSCUSR), use [`CellInstance::cursor_for_shape`].
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

    /// Construct the cursor instance for a given runtime shape.
    ///
    /// `cell_pos` is the top-left of the *cell* the cursor occupies;
    /// `cell_size` is the full cell extent. The function clips down to
    /// the appropriate sub-rect for `Bar` (narrow strip on the left
    /// edge) and `Underline` (thin strip on the bottom edge).
    ///
    /// Bar / underline thickness is `max(2.0, cell_w * 0.15)` capped at
    /// 3.0 — gives 2 px on small fonts and ~2.4 px on `HiDPI` without
    /// becoming a second block.
    ///
    /// **Blink is not rendered yet.** DECSCUSR's blink flag is stored on
    /// `Term` but the animation tick lands with M9 (see
    /// `docs/milestones/m06-color-and-chrome.md`). For now this function
    /// emits the same quad whether the cursor is blinking or steady.
    #[must_use]
    pub fn cursor_for_shape(
        cell_pos: [f32; 2],
        cell_size: [f32; 2],
        shape: CursorShape,
        cursor_color: [f32; 4],
    ) -> Self {
        let (cell_w, cell_h) = (cell_size[0], cell_size[1]);
        // Same flag set for every shape: solid fill, no glyph sample.
        // The pos/size are what actually changes between block / bar /
        // underline.
        let (pos, size) = match shape {
            CursorShape::Block => (cell_pos, cell_size),
            CursorShape::Bar => {
                // Narrow vertical strip on the left edge.
                let thickness = cursor_bar_thickness(cell_w);
                (cell_pos, [thickness, cell_h])
            }
            CursorShape::Underline => {
                // Thin horizontal strip on the bottom edge. We position
                // it flush with the cell bottom so it lines up with
                // descenders the way xterm does.
                let thickness = cursor_bar_thickness(cell_w);
                let y = cell_pos[1] + cell_h - thickness;
                ([cell_pos[0], y], [cell_w, thickness])
            }
        };
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

/// Cursor bar / underline thickness in pixels. 2 px minimum so the
/// stripe is visible at small font sizes; scales to ~15% of cell width
/// on `HiDPI`, capped at 3 px so it doesn't morph into a second block.
fn cursor_bar_thickness(cell_w: f32) -> f32 {
    let scaled = cell_w * 0.15;
    scaled.clamp(2.0, 3.0)
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
    /// Pixel offset of the glyph quad within the cell. `glyph_offset.0`
    /// is the left-side bearing; `glyph_offset.1` is `baseline_y - top`.
    /// This is what stops every glyph from being stretched to fill the
    /// whole cell — the glyph quad is glyph-sized and positioned
    /// correctly inside the cell-sized background quad.
    pub glyph_offset: [f32; 2],
    /// Pixel size of the glyph quad (matches the atlas region size).
    pub glyph_size: [f32; 2],
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
            TColor::Indexed256(idx) => self.resolve_indexed256(idx),
            TColor::Rgb(r, g, b) => srgb_to_linear_rgba(r, g, b),
            named => self.palette[palette_index(named)],
        }
    }

    #[must_use]
    pub fn resolve_bg(&self, c: TColor) -> [f32; 4] {
        match c {
            TColor::Default => self.bg,
            TColor::Indexed256(idx) => self.resolve_indexed256(idx),
            TColor::Rgb(r, g, b) => srgb_to_linear_rgba(r, g, b),
            named => self.palette[palette_index(named)],
        }
    }

    /// Resolve an xterm 256-color index against this theme.
    ///
    /// - `0..16` aliases the 16-entry palette (so the user's theme colors apply).
    /// - `16..232` is the 6×6×6 RGB cube using the canonical xterm levels
    ///   `[0, 95, 135, 175, 215, 255]` (sRGB).
    /// - `232..256` is the 24-step grayscale ramp at sRGB values
    ///   `8 + 10*step` (8, 18, …, 238).
    #[must_use]
    pub fn resolve_indexed256(&self, idx: u8) -> [f32; 4] {
        if idx < 16 {
            return self.palette[idx as usize];
        }
        if idx < 232 {
            let n = idx - 16;
            let r = CUBE_LEVELS[(n / 36) as usize];
            let g = CUBE_LEVELS[((n / 6) % 6) as usize];
            let b = CUBE_LEVELS[(n % 6) as usize];
            return srgb_to_linear_rgba(r, g, b);
        }
        let step = idx - 232;
        let v = 8 + 10 * step;
        srgb_to_linear_rgba(v, v, v)
    }
}

/// xterm 6×6×6 cube levels in sRGB (8-bit). Same on every common implementation.
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// Convert one sRGB byte channel to linear-light float.
fn srgb_channel_to_linear(v: u8) -> f32 {
    let c = f32::from(v) / 255.0;
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn srgb_to_linear_rgba(r: u8, g: u8, b: u8) -> [f32; 4] {
    [
        srgb_channel_to_linear(r),
        srgb_channel_to_linear(g),
        srgb_channel_to_linear(b),
        1.0,
    ]
}

fn palette_index(c: TColor) -> usize {
    // Extended colors (`Indexed256`, `Rgb`) are resolved before reaching
    // this function inside `Theme::resolve_fg/bg`. If a future caller
    // forgets that, the fallback below maps them to palette[0] (Black)
    // rather than UB — clippy folds it into the `Default | Black` arm.
    match c {
        TColor::Default | TColor::Black | TColor::Indexed256(_) | TColor::Rgb(_, _, _) => 0,
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
    [0.000, 0.000, 0.000, 1.0], // Black
    [0.500, 0.020, 0.020, 1.0], // Red
    [0.020, 0.350, 0.020, 1.0], // Green
    [0.500, 0.350, 0.020, 1.0], // Yellow
    [0.020, 0.020, 0.500, 1.0], // Blue
    [0.350, 0.020, 0.350, 1.0], // Magenta
    [0.020, 0.350, 0.500, 1.0], // Cyan
    [0.700, 0.700, 0.700, 1.0], // White
    [0.250, 0.250, 0.250, 1.0], // BrightBlack (dark gray)
    [0.900, 0.150, 0.150, 1.0], // BrightRed
    [0.150, 0.750, 0.150, 1.0], // BrightGreen
    [0.900, 0.750, 0.150, 1.0], // BrightYellow
    [0.250, 0.350, 0.900, 1.0], // BrightBlue
    [0.700, 0.150, 0.700, 1.0], // BrightMagenta
    [0.150, 0.700, 0.750, 1.0], // BrightCyan
    [1.000, 1.000, 1.000, 1.0], // BrightWhite
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
    locate_glyph: F,
) -> Vec<CellInstance>
where
    F: FnMut(u16, u16, char, &Style) -> Option<GlyphSlot>,
{
    let (rows, cols) = term.size();
    let mut out: Vec<CellInstance> = Vec::with_capacity(usize::from(rows) * usize::from(cols));
    build_instances_into(&mut out, term, cell_size, theme, locate_glyph);
    out
}

/// Same as [`build_instances`] but appends into a caller-provided
/// `Vec` (which is `clear()`ed first). Reusing the buffer across frames
/// avoids per-frame allocations on the hot render path.
pub fn build_instances_into<F>(
    out: &mut Vec<CellInstance>,
    term: &Term,
    cell_size: (f32, f32),
    theme: &Theme,
    mut locate_glyph: F,
) where
    F: FnMut(u16, u16, char, &Style) -> Option<GlyphSlot>,
{
    out.clear();
    let (rows, cols) = term.size();
    let needed = usize::from(rows) * usize::from(cols);
    if out.capacity() < needed {
        out.reserve(needed - out.capacity());
    }

    let cell_w = cell_size.0;
    let cell_h = cell_size.1;

    for r in 0..rows {
        let row = term.row(r);
        for c in 0..cols {
            let Some(cell) = row.cells.get(c as usize) else {
                continue;
            };

            // Continuation cells are the second half of a width-2
            // cluster. The cluster's primary cell at `(r, c-1)` will
            // emit a glyph that spans both columns — drawing anything
            // here would over-paint the second half with a blank.
            if cell.is_continuation {
                continue;
            }

            if is_blank_for_render(cell) {
                continue;
            }

            let pos = [f32::from(c) * cell_w, f32::from(r) * cell_h];
            let (fg, bg) = resolve_cell_colors(cell, theme);

            // Always emit a cell-sized background quad. The glyph (if
            // present) is rendered as a separate, glyph-sized quad on
            // top of it — so a narrow `l` does not stretch to fill the
            // whole cell.
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

            if cell.ch.is_whitespace() {
                continue;
            }

            if let Some(slot) = locate_glyph(r, c, cell.ch, &cell.style) {
                let flags = if slot.is_color { FLAG_COLOR_GLYPH } else { 0 };
                // Glyph quad: position is cell + glyph bearing; size is
                // the glyph's own pixel extent (matches the atlas
                // region). The fragment shader's UV interpolation then
                // maps the quad 1:1 to the atlas region.
                out.push(CellInstance {
                    pos: [pos[0] + slot.glyph_offset[0], pos[1] + slot.glyph_offset[1]],
                    size: slot.glyph_size,
                    uv_min: slot.uv_min,
                    uv_max: slot.uv_max,
                    fg,
                    bg,
                    flags,
                    pad: [0; 3],
                });
            }
        }
    }

    // Append cursor as the last instance. Clamp position into the grid.
    // Shape comes from `Term::cursor_shape()` (set by config + DECSCUSR).
    // TODO(M9): respect `Term::cursor_blink()` once the animation tick
    // lands. For now blink is stored but not rendered — a blinking
    // cursor is drawn the same as a steady one.
    let cur = term.cursor();
    let cur_col = u16::min(cur.col, cols.saturating_sub(1));
    let cur_row = u16::min(cur.row, rows.saturating_sub(1));
    let pos = [f32::from(cur_col) * cell_w, f32::from(cur_row) * cell_h];
    out.push(CellInstance::cursor_for_shape(
        pos,
        [cell_w, cell_h],
        term.cursor_shape(),
        theme.cursor,
    ));
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
        instances
            .iter()
            .filter(|i| i.flags & FLAG_CURSOR == 0)
            .count()
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
                glyph_offset: [0.0, 0.0],
                glyph_size: [8.0, 16.0],
            })
        });
        let glyph_inst = v
            .iter()
            .find(|i| i.flags & FLAG_COLOR_GLYPH != 0)
            .expect("expected glyph instance");
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
        assert_ne!(
            theme.resolve_fg(TColor::Green),
            theme.resolve_fg(TColor::Red)
        );
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
        let bg_inst = v.iter().find(|i| i.flags & FLAG_CURSOR == 0).unwrap();
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
            TColor::Black,
            TColor::Red,
            TColor::Green,
            TColor::Yellow,
            TColor::Blue,
            TColor::Magenta,
            TColor::Cyan,
            TColor::White,
            TColor::BrightBlack,
            TColor::BrightRed,
            TColor::BrightGreen,
            TColor::BrightYellow,
            TColor::BrightBlue,
            TColor::BrightMagenta,
            TColor::BrightCyan,
            TColor::BrightWhite,
        ];
        for c in vs {
            let v = theme.resolve_fg(c);
            assert!(v[3] > 0.99);
        }
    }

    // ----- Extended-color resolution ---------------------------------

    #[test]
    fn resolve_indexed256_0_through_15_aliases_palette() {
        let theme = Theme::default_dark();
        for i in 0u8..16 {
            assert_eq!(
                theme.resolve_indexed256(i),
                theme.palette[i as usize],
                "indexed256({i}) should alias palette[{i}]"
            );
        }
    }

    #[test]
    fn resolve_indexed256_cube_corners_match_xterm_levels() {
        // Cube index 16 = (0,0,0) → black.
        let theme = Theme::default_dark();
        let black = theme.resolve_indexed256(16);
        assert!(black[0] < 1e-6);
        assert!(black[1] < 1e-6);
        assert!(black[2] < 1e-6);
        // Cube index 231 = (255,255,255) → white.
        let white = theme.resolve_indexed256(231);
        assert!((white[0] - 1.0).abs() < 1e-6);
        assert!((white[1] - 1.0).abs() < 1e-6);
        assert!((white[2] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn resolve_indexed256_grayscale_ramp_is_monotonic() {
        let theme = Theme::default_dark();
        let mut last = -1.0_f32;
        for i in 232u8..=255 {
            let g = theme.resolve_indexed256(i);
            // R == G == B for grayscale.
            assert!((g[0] - g[1]).abs() < 1e-6);
            assert!((g[1] - g[2]).abs() < 1e-6);
            assert!(g[0] > last, "grayscale[{i}] not strictly increasing");
            last = g[0];
        }
    }

    #[test]
    fn resolve_rgb_round_trips_pure_white_and_black() {
        let theme = Theme::default_dark();
        let white = theme.resolve_fg(TColor::Rgb(255, 255, 255));
        assert!((white[0] - 1.0).abs() < 1e-6);
        let black = theme.resolve_bg(TColor::Rgb(0, 0, 0));
        assert!(black[0] < 1e-6);
    }

    #[test]
    fn resolve_rgb_uses_srgb_to_linear() {
        let theme = Theme::default_dark();
        // 128 in sRGB → ~0.2159 linear (not 0.5).
        let mid = theme.resolve_fg(TColor::Rgb(128, 128, 128));
        assert!(mid[0] > 0.2 && mid[0] < 0.23, "linearised mid = {}", mid[0]);
    }

    #[test]
    fn palette_index_defensive_branch_for_extended_variants() {
        // `palette_index` is only invoked for the named-variant branch
        // inside `resolve_fg/bg`, but the defensive arm exists so a future
        // refactor can't UB. Exercise it directly.
        assert_eq!(super::palette_index(TColor::Indexed256(200)), 0);
        assert_eq!(super::palette_index(TColor::Rgb(1, 2, 3)), 0);
    }

    // ----- Runtime cursor shape (M6) ---------------------------------------

    /// Compute the cursor instance from a `Term`, by shape, for tests
    /// that only care about cursor geometry.
    fn cursor_for(shape: CursorShape, cell_size: (f32, f32)) -> CellInstance {
        let mut t = Term::new(1, 4, 0);
        t.set_cursor_default(shape, false);
        let v = build_instances(&t, cell_size, &Theme::default_dark(), |_, _, _, _| None);
        *cursor_instance(&v)
    }

    #[test]
    fn block_cursor_fills_full_cell() {
        let cur = cursor_for(CursorShape::Block, (8.0, 16.0));
        assert_eq!(cur.size, [8.0, 16.0]);
        assert_eq!(cur.pos, [0.0, 0.0]);
    }

    #[test]
    fn bar_cursor_has_narrow_width_full_height() {
        // Bar cursor: width < cell_w, height == cell_h, positioned at
        // the cell's top-left corner (left edge).
        let cell_w = 8.0;
        let cell_h = 16.0;
        let cur = cursor_for(CursorShape::Bar, (cell_w, cell_h));
        assert!(
            cur.size[0] < cell_w,
            "bar width {} should be < cell_w {cell_w}",
            cur.size[0],
        );
        assert!(cur.size[0] >= 2.0, "bar width must be at least 2 px");
        assert!(
            (cur.size[1] - cell_h).abs() < 1e-3,
            "bar height should equal cell height",
        );
        assert_eq!(cur.pos, [0.0, 0.0]);
    }

    #[test]
    fn underline_cursor_has_thin_height_full_width() {
        // Underline cursor: width == cell_w, height < cell_h,
        // positioned flush with the cell's bottom edge.
        let cell_w = 8.0;
        let cell_h = 16.0;
        let cur = cursor_for(CursorShape::Underline, (cell_w, cell_h));
        assert!(
            (cur.size[0] - cell_w).abs() < 1e-3,
            "underline width should equal cell width",
        );
        assert!(
            cur.size[1] < cell_h,
            "underline height {} should be < cell_h {cell_h}",
            cur.size[1],
        );
        assert!(cur.size[1] >= 2.0, "underline height must be at least 2 px");
        // Y should land near the bottom — exactly `cell_h - thickness`.
        let expected_y = cell_h - cur.size[1];
        assert!((cur.pos[1] - expected_y).abs() < 1e-3);
        assert!((cur.pos[0]).abs() < 1e-3);
    }

    #[test]
    fn cursor_for_shape_keeps_cursor_flag_and_no_glyph() {
        for shape in [CursorShape::Block, CursorShape::Bar, CursorShape::Underline] {
            let cur = cursor_for(shape, (8.0, 16.0));
            assert!(cur.flags & FLAG_CURSOR != 0, "shape {shape:?}");
            assert!(cur.flags & FLAG_NO_GLYPH != 0, "shape {shape:?}");
        }
    }

    #[test]
    fn cursor_thickness_scales_with_cell_width() {
        // Tiny cell — thickness floors at 2 px.
        let cur = cursor_for(CursorShape::Bar, (6.0, 12.0));
        assert!((cur.size[0] - 2.0).abs() < 1e-3, "tiny cell: width = {}", cur.size[0]);
        // Big cell (HiDPI) — thickness scales up to ~15% but caps at 3 px.
        let cur = cursor_for(CursorShape::Bar, (40.0, 80.0));
        assert!(
            cur.size[0] >= 2.0 && cur.size[0] <= 3.0,
            "big cell: width {} should be in [2, 3]",
            cur.size[0],
        );
    }

    #[test]
    fn continuation_cell_produces_no_instance() {
        // Print a CJK ideograph; the renderer should emit a single
        // background instance + a single glyph slot for the primary
        // cell, NOT a second instance for the continuation half at
        // col=1.
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, "你".as_bytes());
        // No glyph slots — locator returns None. We only count
        // background instances.
        let v = build_instances(&t, (8.0, 16.0), &Theme::default_dark(), |_, _, _, _| None);
        // Exactly one non-cursor instance (the wide cluster's
        // background quad), even though the cell grid records two
        // cells (one primary + one continuation).
        assert_eq!(count_non_cursor(&v), 1);
    }

    #[test]
    fn decscusr_runtime_switch_changes_cursor_geometry() {
        // End-to-end: feed DECSCUSR to Term, then verify build_instances
        // emits the right shape.
        let mut t = Term::new(1, 4, 0);
        // Default = block.
        let v = build_instances(&t, (8.0, 16.0), &Theme::default_dark(), |_, _, _, _| None);
        assert_eq!(cursor_instance(&v).size, [8.0, 16.0]);
        // Switch to bar (Ps=5 → bar, blinking).
        feed(&mut t, b"\x1b[5 q");
        let v = build_instances(&t, (8.0, 16.0), &Theme::default_dark(), |_, _, _, _| None);
        assert!(cursor_instance(&v).size[0] < 8.0, "bar after Ps=5");
        // Switch to underline (Ps=4).
        feed(&mut t, b"\x1b[4 q");
        let v = build_instances(&t, (8.0, 16.0), &Theme::default_dark(), |_, _, _, _| None);
        assert!(
            cursor_instance(&v).size[1] < 16.0,
            "underline after Ps=4",
        );
        // Back to block (Ps=2).
        feed(&mut t, b"\x1b[2 q");
        let v = build_instances(&t, (8.0, 16.0), &Theme::default_dark(), |_, _, _, _| None);
        assert_eq!(cursor_instance(&v).size, [8.0, 16.0]);
    }
}
