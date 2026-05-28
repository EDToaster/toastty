//! Per-cell GPU instance buffer construction.
//!
//! `build_instances` walks the active grid plus the cursor and emits one
//! [`CellInstance`] per drawable cell. The cursor is rendered as part of
//! this pass — an inverted-color block at the current cursor position.
//!
//! Pure CPU: no GPU types in or out. The pipeline crate uploads the
//! resulting `Vec<CellInstance>` to the vertex buffer.

use bytemuck::{Pod, Zeroable};
use toastty_term::{Cell, Color as TColor, CursorShape, Damage, PLACEHOLDER, Style, Term};

/// Flag bit: instance is the text-cursor block (forces inverse rendering,
/// no glyph sample).
pub const FLAG_CURSOR: u32 = 1 << 0;

/// Flag bit: glyph is sampled from the color (BGRA) atlas, not the mask
/// (R8) atlas. The shader uses this to pick the sampler & blend mode.
pub const FLAG_COLOR_GLYPH: u32 = 1 << 1;

/// Flag bit: this instance has no glyph at all (background-only fill).
pub const FLAG_NO_GLYPH: u32 = 1 << 2;

/// Flag bit: this instance is the underline strip for a cell flagged
/// with SGR underline or an active OSC 8 hyperlink. The shader doesn't
/// branch on this bit (current shader treats it the same as
/// `FLAG_NO_GLYPH`); we still flag it so future shader work can pick
/// it out without re-scanning the instance buffer.
pub const FLAG_UNDERLINE: u32 = 1 << 3;

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

/// Pixel-space cursor rect `[x_min, y_min, x_max, y_max]` for `term`.
///
/// Built from the same position math as the cursor instance appended by
/// [`build_instances_into`] / [`build_dirty_instances_into`], so the
/// shader's per-pixel "am I inside the cursor?" check agrees with the
/// cursor block actually painted in the bg pass.
///
/// The renderer plumbs this into the `Globals` UBO so the glyph pass can
/// invert the glyph color where it overlaps the cursor. When the cursor
/// is hidden the renderer passes an all-zero rect (degenerate) — the
/// shader's strict-inside test then never matches.
#[must_use]
pub fn cursor_pixel_rect(term: &Term, cell_size: (f32, f32)) -> [f32; 4] {
    let (rows, cols) = term.size();
    let cell_w = cell_size.0;
    let cell_h = cell_size.1;
    let view_pixel = term.view_offset_pixel();
    let pixel_extra: u16 = if view_pixel > 0.0 { 1 } else { 0 };
    let y_translate: f32 = if pixel_extra > 0 {
        view_pixel - cell_h
    } else {
        0.0
    };
    let cur = term.cursor();
    let cur_col = u16::min(cur.col, cols.saturating_sub(1));
    let cur_row = u16::min(cur.row, rows.saturating_sub(1));
    let cell_pos = [
        f32::from(cur_col) * cell_w,
        f32::from(cur_row) * cell_h + y_translate,
    ];
    let (pos, size) = match term.cursor_shape() {
        CursorShape::Block => (cell_pos, [cell_w, cell_h]),
        CursorShape::Bar => {
            let thickness = cursor_bar_thickness(cell_w);
            (cell_pos, [thickness, cell_h])
        }
        CursorShape::Underline => {
            let thickness = cursor_bar_thickness(cell_w);
            let y = cell_pos[1] + cell_h - thickness;
            ([cell_pos[0], y], [cell_w, thickness])
        }
    };
    [pos[0], pos[1], pos[0] + size[0], pos[1] + size[1]]
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
    /// Background tint applied to selected cells. The cell's own `fg`
    /// stays so text remains legible on the tint. Callers can set this
    /// explicitly; [`Theme::with_default_selection_bg`] derives a
    /// reasonable default by mixing `bg` toward `fg`.
    pub selection_bg: [f32; 4],
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
            selection_bg: derive_selection_bg(
                [0.07, 0.07, 0.09, 1.0],
                [0.85, 0.85, 0.85, 1.0],
            ),
        }
    }

    /// Recompute `selection_bg` from the current `fg`/`bg` by mixing
    /// the bg toward the fg. Idempotent — call after mutating the
    /// theme's fg/bg to refresh the derived selection tint.
    #[must_use]
    pub fn with_default_selection_bg(mut self) -> Self {
        self.selection_bg = derive_selection_bg(self.bg, self.fg);
        self
    }

    #[must_use]
    pub fn resolve_fg(&self, c: TColor, ext_palette: Option<&[[f32; 4]; 256]>) -> [f32; 4] {
        match c {
            TColor::Default => self.fg,
            TColor::Indexed256(idx) => self.resolve_indexed256(idx, ext_palette),
            TColor::Rgb(r, g, b) => srgb_to_linear_rgba(r, g, b),
            named => self.palette[palette_index(named)],
        }
    }

    #[must_use]
    pub fn resolve_bg(&self, c: TColor, ext_palette: Option<&[[f32; 4]; 256]>) -> [f32; 4] {
        match c {
            TColor::Default => self.bg,
            TColor::Indexed256(idx) => self.resolve_indexed256(idx, ext_palette),
            TColor::Rgb(r, g, b) => srgb_to_linear_rgba(r, g, b),
            named => self.palette[palette_index(named)],
        }
    }

    /// Resolve an xterm 256-color index against this theme.
    ///
    /// - `0..16` aliases the 16-entry palette (so the user's theme colors apply).
    /// - `16..232` is the 6×6×6 RGB cube using the canonical xterm levels
    ///   `[0, 95, 135, 175, 215, 255]` (sRGB), unless an OSC 4 override is in
    ///   effect (provided via `ext_palette`).
    /// - `232..256` is the 24-step grayscale ramp at sRGB values
    ///   `8 + 10*step` (8, 18, …, 238), again subject to override.
    ///
    /// `ext_palette`, when `Some`, is the renderer's cached linear-light
    /// 256-entry table built from the term's OSC 4 overrides (with the
    /// xterm defaults filled in for non-overridden indices). The theme's
    /// 16-entry `palette` still wins for `idx < 16` so a user theme
    /// keeps full control of the base ANSI colors. Pass `None` from
    /// pure-CPU code paths (snapshot tests, benches) that don't care
    /// about OSC 4 — the function then falls back to the xterm formulas.
    #[must_use]
    pub fn resolve_indexed256(
        &self,
        idx: u8,
        ext_palette: Option<&[[f32; 4]; 256]>,
    ) -> [f32; 4] {
        if idx < 16 {
            return self.palette[idx as usize];
        }
        if let Some(ext) = ext_palette {
            return ext[idx as usize];
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

/// Mix the background toward the foreground by ~35% to produce a
/// "selection tint" that reads as a highlight on both light and dark
/// themes. Alpha is preserved from `bg`.
fn derive_selection_bg(bg: [f32; 4], fg: [f32; 4]) -> [f32; 4] {
    const MIX: f32 = 0.35;
    [
        bg[0] * (1.0 - MIX) + fg[0] * MIX,
        bg[1] * (1.0 - MIX) + fg[1] * MIX,
        bg[2] * (1.0 - MIX) + fg[2] * MIX,
        bg[3],
    ]
}

/// Convert one sRGB byte channel to linear-light float.
pub(crate) fn srgb_channel_to_linear(v: u8) -> f32 {
    let c = f32::from(v) / 255.0;
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

pub(crate) fn srgb_to_linear_rgba(r: u8, g: u8, b: u8) -> [f32; 4] {
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
        // A hyperlinked cell needs the underline strip even when its
        // text is empty / styled the same as the surroundings.
        && cell.hyperlink_id.is_none()
}

/// Build an underline-strip instance for a cell. 2 px thick, flush with
/// the bottom of the cell, using `fg` as the color (same as the glyph).
/// Called for cells with SGR underline or an active OSC 8 hyperlink.
fn underline_instance(pos: [f32; 2], cell_size: [f32; 2], fg: [f32; 4]) -> CellInstance {
    let thickness = 2.0_f32.min(cell_size[1]);
    let y = pos[1] + cell_size[1] - thickness;
    CellInstance {
        pos: [pos[0], y],
        size: [cell_size[0], thickness],
        uv_min: [0.0, 0.0],
        uv_max: [0.0, 0.0],
        // Render as `bg = fg` so the underline shows up regardless of
        // whether the shader currently routes through the FLAG_UNDERLINE
        // branch — it'll fall through `FLAG_NO_GLYPH` and emit `bg`.
        fg,
        bg: fg,
        flags: FLAG_NO_GLYPH | FLAG_UNDERLINE,
        pad: [0; 3],
    }
}

/// Effective fg/bg for a cell after applying SGR `reverse` (mode 7).
///
/// `ext_palette` (when `Some`) is consulted for `Color::Indexed256` lookups
/// at `idx >= 16` — see [`Theme::resolve_indexed256`] for the OSC 4 path.
#[must_use]
pub fn resolve_cell_colors(
    cell: &Cell,
    theme: &Theme,
    ext_palette: Option<&[[f32; 4]; 256]>,
) -> ([f32; 4], [f32; 4]) {
    let fg = theme.resolve_fg(cell.style.fg, ext_palette);
    let bg = theme.resolve_bg(cell.style.bg, ext_palette);
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
/// - `ext_palette` — optional renderer-cached linear-light 256-entry
///   table (built from OSC 4 overrides + xterm defaults). When `Some`,
///   `Color::Indexed256(idx)` with `idx >= 16` resolves through this
///   table so palette overrides actually reach the rendered pixels
///   (M10-followup C1). Pure CPU paths (tests, benches) may pass `None`
///   to fall back to the built-in xterm cube/grayscale math.
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
    ext_palette: Option<&[[f32; 4]; 256]>,
    locate_glyph: F,
) -> Vec<CellInstance>
where
    F: FnMut(u16, u16, char, &Style) -> Option<GlyphSlot>,
{
    let (rows, cols) = term.size();
    let mut out: Vec<CellInstance> = Vec::with_capacity(usize::from(rows) * usize::from(cols));
    build_instances_into(
        &mut out,
        term,
        cell_size,
        theme,
        ext_palette,
        locate_glyph,
        |_, _| false,
    );
    out
}

/// Translate a render-loop row `r` (0-based, top of viewport) into the
/// stable `line_id` of the row at that position. Encapsulates the
/// `bottom_id - shift - (rows - 1 - r)` math and returns `None` when
/// the computed id would underflow (a defensive guard — under normal
/// operation `bottom_id` is initialized so this never happens).
fn line_id_for_render_row(
    bottom_id: u64,
    rows_total: u16,
    view_offset_lines: u32,
    pixel_extra: u16,
    r: u16,
) -> Option<u64> {
    // line_id = bottom_id - (visible_rows - 1 - r + pixel_extra + view_offset_lines)
    let above_bottom = u64::from(rows_total.saturating_sub(1))
        .saturating_sub(u64::from(r))
        .checked_add(u64::from(pixel_extra))?
        .checked_add(u64::from(view_offset_lines))?;
    bottom_id.checked_sub(above_bottom)
}

/// Same as [`build_instances`] but appends into a caller-provided
/// `Vec` (which is `clear()`ed first). Reusing the buffer across frames
/// avoids per-frame allocations on the hot render path.
pub fn build_instances_into<F, S>(
    out: &mut Vec<CellInstance>,
    term: &Term,
    cell_size: (f32, f32),
    theme: &Theme,
    ext_palette: Option<&[[f32; 4]; 256]>,
    mut locate_glyph: F,
    mut is_selected: S,
) where
    F: FnMut(u16, u16, char, &Style) -> Option<GlyphSlot>,
    S: FnMut(u64, u16) -> bool,
{
    out.clear();
    let (rows, cols) = term.size();
    let cell_w = cell_size.0;
    let cell_h = cell_size.1;

    // Sub-row scroll offset. When the user is fractionally scrolled
    // we render one extra row at the top (the partial row that hangs
    // above y=0) and y-translate every row by `view_offset_pixel -
    // cell_h`. At a whole-row offset (pixel == 0) no extra row is
    // needed and the y-translate is 0.
    let view_pixel = term.view_offset_pixel();
    let pixel_extra: u16 = if view_pixel > 0.0 { 1 } else { 0 };
    let rows_rendered = rows + pixel_extra;
    let y_translate: f32 = if pixel_extra > 0 {
        view_pixel - cell_h
    } else {
        0.0
    };

    let bottom_id = term.bottom_id();
    let view_offset_lines = term.view_offset_lines();
    let needed = usize::from(rows_rendered) * usize::from(cols);
    if out.capacity() < needed {
        out.reserve(needed - out.capacity());
    }

    for r in 0..rows_rendered {
        let row = term.view_row(r);
        // Compute the stable line id for this rendered row once. None
        // means "this row's id would underflow" — only happens at the
        // very top of an unscrolled tiny grid; the cell still renders
        // but selection is treated as off.
        let line_id =
            line_id_for_render_row(bottom_id, rows, view_offset_lines, pixel_extra, r);
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

            let selected = line_id.is_some_and(|l| is_selected(l, c));
            if !selected && is_blank_for_render(cell) {
                continue;
            }

            let pos = [f32::from(c) * cell_w, f32::from(r) * cell_h + y_translate];
            let (fg, bg) = resolve_cell_colors(cell, theme, ext_palette);
            let bg = if selected { theme.selection_bg } else { bg };

            // Width-2 (CJK) primaries: widen the bg quad to span both
            // columns so the continuation half isn't left showing the
            // previous frame / default bg.
            let bg_w = if row
                .cells
                .get(c as usize + 1)
                .is_some_and(|next| next.is_continuation)
            {
                2.0 * cell_w
            } else {
                cell_w
            };

            // Always emit a cell-sized background quad. The glyph (if
            // present) is rendered as a separate, glyph-sized quad on
            // top of it — so a narrow `l` does not stretch to fill the
            // whole cell.
            out.push(CellInstance {
                pos,
                size: [bg_w, cell_h],
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

            // Kitty Unicode placeholder cells (`U+10EEEE`) live in the
            // grid as cursor-motion shims; the image pipeline draws the
            // actual pixels. Skip the glyph emission so the rasterizer
            // doesn't draw `.notdef` tofu where the image will land.
            // (Keep the bg quad — placeholders still need to overpaint
            // whatever was there before the image arrived.)
            if cell.ch == PLACEHOLDER {
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

            // Underline strip for SGR underline or OSC 8 hyperlink.
            // M10-followup I6: peek the next cell — width-2 (CJK)
            // primaries underline the FULL cluster (both columns), not
            // just the primary column. Without this, hyperlinked CJK
            // text underlined only the leading half of every wide
            // glyph. (We do the same in `build_dirty_instances_into`,
            // and the bg quad above uses the same widening so the
            // continuation half is covered.)
            if cell.style.flags.underline || cell.hyperlink_id.is_some() {
                let strip_w = if row
                    .cells
                    .get(c as usize + 1)
                    .is_some_and(|next| next.is_continuation)
                {
                    2.0 * cell_w
                } else {
                    cell_w
                };
                out.push(underline_instance(pos, [strip_w, cell_h], fg));
            }
        }
    }

    // Append cursor as the last instance. Clamp position into the grid.
    // Shape comes from `Term::cursor_shape()` (set by config + DECSCUSR).
    // Blink visibility is gated by `build_dirty_instances_into` /
    // `Renderer::render_term`; the unconditional `build_instances_into`
    // path always emits the cursor (legacy behavior, used by tests and
    // the first frame). The y-translate is the same one applied to
    // every other instance so the cursor stays aligned with the live
    // grid while the viewport animates back to the bottom.
    let cur = term.cursor();
    let cur_col = u16::min(cur.col, cols.saturating_sub(1));
    let cur_row = u16::min(cur.row, rows.saturating_sub(1));
    let pos = [
        f32::from(cur_col) * cell_w,
        f32::from(cur_row) * cell_h + y_translate,
    ];
    out.push(CellInstance::cursor_for_shape(
        pos,
        [cell_w, cell_h],
        term.cursor_shape(),
        theme.cursor,
    ));
}

/// Build instances for only the dirty cells in `damage` against the
/// active grid, plus the cursor (gated on `cursor_visible`).
///
/// This is the M9 partial-redraw counterpart to [`build_instances_into`]:
/// it emits a background quad **for every dirty cell, including blanks**
/// so a `LoadOp::Load` pass overpaints whatever was previously there
/// (without the blank-cell bg quad, the old glyph would ghost through).
/// Glyph instances are emitted only for non-whitespace cells with an
/// atlas slot.
///
/// If `damage.all` is set, this delegates to [`build_instances_into`] —
/// the renderer cascades `damage.all` into `needs_full_clear` for the
/// frame, so emitting every cell is the right thing anyway.
///
/// Continuation cells (second half of a width-2 cluster) are skipped:
/// the cluster's primary cell at `(r, c-1)` is responsible for the full
/// multi-cell quad.
/// Either-iterator over dirty columns. Replaces `Box<dyn Iterator>`
/// inside the dirty-row loop so we don't heap-allocate per dirty row
/// on the render hot path.
enum DirtyCols<'a> {
    Range(core::ops::Range<u16>),
    Slice(core::slice::Iter<'a, u16>),
}

impl<'a> Iterator for DirtyCols<'a> {
    type Item = u16;
    fn next(&mut self) -> Option<u16> {
        match self {
            DirtyCols::Range(r) => r.next(),
            DirtyCols::Slice(it) => it.next().copied(),
        }
    }
}

#[allow(clippy::too_many_arguments)] // mirrors build_instances_into + adds damage/visibility/ext_palette
pub fn build_dirty_instances_into<F, S>(
    out: &mut Vec<CellInstance>,
    term: &Term,
    damage: &Damage,
    cell_size: (f32, f32),
    theme: &Theme,
    ext_palette: Option<&[[f32; 4]; 256]>,
    cursor_visible: bool,
    mut locate_glyph: F,
    mut is_selected: S,
) where
    F: FnMut(u16, u16, char, &Style) -> Option<GlyphSlot>,
    S: FnMut(u64, u16) -> bool,
{
    // damage.all → fall back to the full-build path. Same instance
    // count as a full frame, and the renderer's `needs_full_clear` is
    // already set to true for this frame.
    if damage.all {
        build_instances_into(
            out,
            term,
            cell_size,
            theme,
            ext_palette,
            locate_glyph,
            is_selected,
        );
        if !cursor_visible {
            // Drop the trailing cursor instance the full builder
            // unconditionally appends.
            out.pop();
        }
        return;
    }

    out.clear();
    let (rows, cols) = term.size();
    // Damage is usually small; reserve based on the dirty-row count.
    let dirty_row_count = damage.iter_rows().count();
    if dirty_row_count > 0 && out.capacity() < dirty_row_count {
        out.reserve(dirty_row_count);
    }

    let cell_w = cell_size.0;
    let cell_h = cell_size.1;

    // Sub-row scroll y-translation. See [`build_instances_into`] for
    // the geometry. Under steady-state scrollback (view_offset_lines >
    // 0, view_offset_pixel == 0) the dirty builder is still relevant
    // (e.g. cursor blink) and needs `view_row` semantics.
    let view_pixel = term.view_offset_pixel();
    let pixel_extra: u16 = if view_pixel > 0.0 { 1 } else { 0 };
    let y_translate: f32 = if pixel_extra > 0 {
        view_pixel - cell_h
    } else {
        0.0
    };
    let bottom_id = term.bottom_id();
    let view_offset_lines = term.view_offset_lines();

    for (r, row_damage) in damage.iter_rows() {
        let row = term.view_row(r);
        let line_id =
            line_id_for_render_row(bottom_id, rows, view_offset_lines, pixel_extra, r);
        // Stack-allocated iter enum instead of `Box<dyn Iterator>`:
        // boxing was a per-dirty-row heap allocation on the render
        // hot path. Same `all_cols` vs sparse-list dispatch as before.
        let dirty_cells = if row_damage.all_cols {
            DirtyCols::Range(0..cols)
        } else {
            DirtyCols::Slice(row_damage.cols.iter())
        };

        for c in dirty_cells {
            let Some(cell) = row.cells.get(c as usize) else {
                continue;
            };
            // Continuation cells: the cluster's primary at c-1 already
            // contributes a wide-cell quad. Drawing here would over-paint
            // the second half with a blank.
            if cell.is_continuation {
                continue;
            }

            let selected = line_id.is_some_and(|l| is_selected(l, c));
            let pos = [f32::from(c) * cell_w, f32::from(r) * cell_h + y_translate];
            let (fg, bg) = resolve_cell_colors(cell, theme, ext_palette);
            let bg = if selected { theme.selection_bg } else { bg };

            // Width-2 (CJK) primaries: widen the bg quad to span both
            // columns so the continuation half isn't left showing the
            // previous frame / default bg.
            let bg_w = if row
                .cells
                .get(c as usize + 1)
                .is_some_and(|next| next.is_continuation)
            {
                2.0 * cell_w
            } else {
                cell_w
            };

            // Always emit a background quad — even for blank cells —
            // under LoadOp::Load, so old glyphs / cursor blocks get
            // overpainted.
            out.push(CellInstance {
                pos,
                size: [bg_w, cell_h],
                uv_min: [0.0, 0.0],
                uv_max: [0.0, 0.0],
                fg,
                bg,
                flags: FLAG_NO_GLYPH,
                pad: [0; 3],
            });

            if cell.ch.is_whitespace() || cell.ch == '\0' {
                continue;
            }

            // Suppress glyph emission for Kitty Unicode placeholder
            // cells — see the matching note in `build_instances_into`.
            // The bg quad above is kept so old content stays
            // overpainted under LoadOp::Load.
            if cell.ch == PLACEHOLDER {
                continue;
            }

            if let Some(slot) = locate_glyph(r, c, cell.ch, &cell.style) {
                let flags = if slot.is_color {
                    FLAG_COLOR_GLYPH
                } else {
                    0
                };
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

            // Underline strip for SGR underline or OSC 8 hyperlink. Emit
            // *after* the glyph so it draws on top of any descender
            // (matches xterm). M10-followup I6: same width-2 widening
            // as the full builder above.
            if cell.style.flags.underline || cell.hyperlink_id.is_some() {
                let strip_w = if row
                    .cells
                    .get(c as usize + 1)
                    .is_some_and(|next| next.is_continuation)
                {
                    2.0 * cell_w
                } else {
                    cell_w
                };
                out.push(underline_instance(pos, [strip_w, cell_h], fg));
            }
        }
    }

    // Cursor is the last instance — guarantees it renders on top of
    // any cell at the same coordinates. Gated on visibility (blink),
    // which the renderer also ANDs with `!is_view_scrolled_back()` so
    // the cursor disappears while the user is in scrollback.
    if cursor_visible {
        let cur = term.cursor();
        let cur_col = u16::min(cur.col, cols.saturating_sub(1));
        let cur_row = u16::min(cur.row, rows.saturating_sub(1));
        let pos = [
            f32::from(cur_col) * cell_w,
            f32::from(cur_row) * cell_h + y_translate,
        ];
        out.push(CellInstance::cursor_for_shape(
            pos,
            [cell_w, cell_h],
            term.cursor_shape(),
            theme.cursor,
        ));
    }
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
        let v = build_instances(&t, (8.0, 16.0), &theme, None, |_, _, _, _| None);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].flags & FLAG_CURSOR, FLAG_CURSOR);
    }

    #[test]
    fn each_visible_char_produces_an_instance_plus_cursor() {
        let mut t = Term::new(2, 8, 0);
        feed(&mut t, b"hello");
        let theme = Theme::default_dark();
        let v = build_instances(&t, (8.0, 16.0), &theme, None, |_, _, _, _| None);
        // 5 character cells + cursor.
        assert_eq!(v.len(), 6);
        assert_eq!(count_non_cursor(&v), 5);
    }

    #[test]
    fn blank_cells_produce_no_instance() {
        // Empty grid: cells are spaces with default style; nothing should
        // be emitted apart from the cursor.
        let t = Term::new(2, 8, 0);
        let v = build_instances(&t, (8.0, 16.0), &Theme::default_dark(), None, |_, _, _, _| None);
        assert_eq!(count_non_cursor(&v), 0);
    }

    #[test]
    fn blank_cell_with_non_default_bg_still_emits() {
        let mut t = Term::new(1, 4, 0);
        // Set bg to red, then write a space.
        feed(&mut t, b"\x1b[41m ");
        let v = build_instances(&t, (8.0, 16.0), &Theme::default_dark(), None, |_, _, _, _| None);
        // 1 background-colored cell + cursor.
        assert_eq!(count_non_cursor(&v), 1);
    }

    #[test]
    fn cursor_position_follows_csi_h() {
        let mut t = Term::new(10, 10, 0);
        feed(&mut t, b"\x1b[5;10H"); // row 5 col 10 (1-based)
        let v = build_instances(&t, (8.0, 16.0), &Theme::default_dark(), None, |_, _, _, _| None);
        let cur = cursor_instance(&v);
        // 5,10 1-based → row 4, col 9 0-based.
        assert!((cur.pos[0] - 9.0 * 8.0).abs() < 1e-3);
        assert!((cur.pos[1] - 4.0 * 16.0).abs() < 1e-3);
    }

    #[test]
    fn cursor_clamped_into_grid() {
        let t = Term::new(2, 3, 0);
        let v = build_instances(&t, (8.0, 16.0), &Theme::default_dark(), None, |_, _, _, _| None);
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
        let v = build_instances(&t, (8.0, 16.0), &theme, None, |_, _, _, _| None);
        let normal = v
            .iter()
            .find(|i| i.flags & FLAG_CURSOR == 0)
            .expect("non-cursor instance");
        let normal_fg = normal.fg;
        let normal_bg = normal.bg;

        // Reverse: same red, but inverted onto bg.
        let mut t2 = Term::new(1, 4, 0);
        feed(&mut t2, b"\x1b[7;31mA");
        let v2 = build_instances(&t2, (8.0, 16.0), &theme, None, |_, _, _, _| None);
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
        let _ = build_instances(&t, (8.0, 16.0), &Theme::default_dark(), None, |_, _, ch, _| {
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
        let v = build_instances(&t, (8.0, 16.0), &Theme::default_dark(), None, |_, _, _, _| {
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
        assert_eq!(theme.resolve_fg(TColor::Default, None), theme.fg);
        assert_eq!(theme.resolve_bg(TColor::Default, None), theme.bg);
    }

    #[test]
    fn theme_resolves_palette_indexed_colors() {
        let theme = Theme::default_dark();
        let red = theme.resolve_fg(TColor::Red, None);
        let bright_red = theme.resolve_fg(TColor::BrightRed, None);
        assert_ne!(red, theme.fg);
        assert_ne!(red, bright_red);
        assert_ne!(
            theme.resolve_fg(TColor::Green, None),
            theme.resolve_fg(TColor::Red, None)
        );
    }

    #[test]
    fn cursor_instance_has_no_glyph_flag() {
        let t = Term::new(2, 2, 0);
        let v = build_instances(&t, (8.0, 16.0), &Theme::default_dark(), None, |_, _, _, _| None);
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
        // M10.5: a hyperlinked cell must NOT be considered blank — the
        // underline strip must still emit.
        let mut c = Cell::BLANK;
        c.hyperlink_id = std::num::NonZeroU16::new(1);
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
        let v = build_instances(&t, (8.0, 16.0), &Theme::default_dark(), None, |_, _, _, _| None);
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
        let (fg, bg) = resolve_cell_colors(&c, &theme, None);
        assert_eq!(fg, theme.bg);
        assert_eq!(bg, theme.resolve_fg(TColor::Red, None));
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
            let v = theme.resolve_fg(c, None);
            assert!(v[3] > 0.99);
        }
    }

    // ----- Extended-color resolution ---------------------------------

    #[test]
    fn resolve_indexed256_0_through_15_aliases_palette() {
        let theme = Theme::default_dark();
        for i in 0u8..16 {
            assert_eq!(
                theme.resolve_indexed256(i, None),
                theme.palette[i as usize],
                "indexed256({i}) should alias palette[{i}]"
            );
        }
    }

    #[test]
    fn resolve_indexed256_cube_corners_match_xterm_levels() {
        // Cube index 16 = (0,0,0) → black.
        let theme = Theme::default_dark();
        let black = theme.resolve_indexed256(16, None);
        assert!(black[0] < 1e-6);
        assert!(black[1] < 1e-6);
        assert!(black[2] < 1e-6);
        // Cube index 231 = (255,255,255) → white.
        let white = theme.resolve_indexed256(231, None);
        assert!((white[0] - 1.0).abs() < 1e-6);
        assert!((white[1] - 1.0).abs() < 1e-6);
        assert!((white[2] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn resolve_indexed256_grayscale_ramp_is_monotonic() {
        let theme = Theme::default_dark();
        let mut last = -1.0_f32;
        for i in 232u8..=255 {
            let g = theme.resolve_indexed256(i, None);
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
        let white = theme.resolve_fg(TColor::Rgb(255, 255, 255), None);
        assert!((white[0] - 1.0).abs() < 1e-6);
        let black = theme.resolve_bg(TColor::Rgb(0, 0, 0), None);
        assert!(black[0] < 1e-6);
    }

    #[test]
    fn resolve_rgb_uses_srgb_to_linear() {
        let theme = Theme::default_dark();
        // 128 in sRGB → ~0.2159 linear (not 0.5).
        let mid = theme.resolve_fg(TColor::Rgb(128, 128, 128), None);
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
        let v = build_instances(&t, cell_size, &Theme::default_dark(), None, |_, _, _, _| None);
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
    fn mixed_cjk_and_ascii_geometry_aligns_to_cell_boundaries() {
        // End-to-end geometry check (no GPU): print "a你b" → cells:
        //   0: 'a' (primary, ascii)
        //   1: '你' (primary, wide)
        //   2: continuation
        //   3: 'b' (primary, ascii)
        // build_instances must emit three background quads (skipping
        // the continuation), and each non-cursor instance's x-pos
        // must be a multiple of cell_w.
        let mut t = Term::new(1, 8, 0);
        feed(&mut t, "a你b".as_bytes());
        let v = build_instances(&t, (8.0, 16.0), &Theme::default_dark(), None, |_, _, _, _| None);
        let bgs: Vec<_> = v
            .iter()
            .filter(|i| i.flags & FLAG_CURSOR == 0)
            .collect();
        // Exactly 3 instances — one per non-continuation cell.
        assert_eq!(bgs.len(), 3);
        // Each landing on an integer cell boundary.
        for inst in &bgs {
            let col_f = inst.pos[0] / 8.0;
            let col = col_f.round();
            assert!((col_f - col).abs() < 1e-3, "x={} not on cell grid", inst.pos[0]);
        }
        // Columns 0 / 1 / 3 — the continuation column (2) is skipped.
        let cols: Vec<u16> = bgs
            .iter()
            .map(|i| (i.pos[0] / 8.0).round() as u16)
            .collect();
        assert!(cols.contains(&0), "'a' at col 0");
        assert!(cols.contains(&1), "'你' at col 1");
        assert!(cols.contains(&3), "'b' at col 3 (after continuation)");
        assert!(!cols.contains(&2), "col 2 (continuation) must be skipped");
    }

    // ----- OSC 8 hyperlink underline emission -----------------------------

    #[test]
    fn hyperlinked_cell_emits_underline_instance() {
        // A printed cell with an active OSC 8 hyperlink must produce
        // an underline strip in the instance buffer regardless of SGR
        // underline state.
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b]8;;https://example.com\x1b\\X");
        let v = build_instances(&t, (8.0, 16.0), &Theme::default_dark(), None, |_, _, _, _| None);
        let underline_count = v.iter().filter(|i| i.flags & FLAG_UNDERLINE != 0).count();
        assert_eq!(underline_count, 1, "hyperlinked cell must emit one underline strip");
    }

    #[test]
    fn sgr_underline_cell_emits_underline_instance() {
        // Same as above but via SGR mode 4 (\\x1b[4mX).
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[4mX");
        let v = build_instances(&t, (8.0, 16.0), &Theme::default_dark(), None, |_, _, _, _| None);
        let underline_count = v.iter().filter(|i| i.flags & FLAG_UNDERLINE != 0).count();
        assert_eq!(underline_count, 1);
    }

    #[test]
    fn no_underline_when_neither_sgr_nor_hyperlink_set() {
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"X");
        let v = build_instances(&t, (8.0, 16.0), &Theme::default_dark(), None, |_, _, _, _| None);
        let underline_count = v.iter().filter(|i| i.flags & FLAG_UNDERLINE != 0).count();
        assert_eq!(underline_count, 0);
    }

    #[test]
    fn kitty_unicode_placeholder_emits_bg_quad_but_no_glyph() {
        // M11a-followup C2: cells holding the Kitty Unicode placeholder
        // (`U+10EEEE`) must emit a background quad so old content gets
        // overpainted, but MUST NOT emit a glyph quad — the image
        // pipeline draws the real pixels, and a glyph quad would
        // render `.notdef` tofu through any gap in the image.
        let mut t = Term::new(1, 4, 0);
        // SGR Indexed256(1) primes the placeholder run (sets image_id_low
        // = 1) then the placeholder codepoint lands in cell (0, 0).
        feed(&mut t, "\x1b[38;5;1m\u{10EEEE}".as_bytes());

        // The locator panics if the renderer asks for a glyph at the
        // placeholder cell — that's the bug C2 fixes.
        let v = build_instances(
            &t,
            (8.0, 16.0),
            &Theme::default_dark(),
            None,
            |_, _, ch, _| {
                assert_ne!(ch, PLACEHOLDER, "locator must NOT be called for placeholder");
                None
            },
        );
        // Exactly one non-cursor instance: the bg quad. No glyph quad,
        // no underline strip.
        let non_cursor: Vec<_> = v.iter().filter(|i| i.flags & FLAG_CURSOR == 0).collect();
        assert_eq!(non_cursor.len(), 1, "expected 1 bg quad, got {non_cursor:?}");
        assert!(non_cursor[0].flags & FLAG_NO_GLYPH != 0);
        assert_eq!(non_cursor[0].pos, [0.0, 0.0]);
    }

    #[test]
    fn kitty_unicode_placeholder_emits_bg_quad_in_dirty_builder() {
        // Same as above but exercising the M9 partial-redraw path.
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, "\x1b[38;5;1m\u{10EEEE}".as_bytes());
        let damage = t.damage().clone();
        let mut out = Vec::new();
        super::build_dirty_instances_into(
            &mut out,
            &t,
            &damage,
            (8.0, 16.0),
            &Theme::default_dark(),
            None,
            true,
            |_, _, ch, _| {
                assert_ne!(ch, PLACEHOLDER, "locator must NOT be called for placeholder");
                None
            },
            |_, _| false,
        );
        // One bg quad for the placeholder cell + cursor.
        let non_cursor: Vec<_> = out.iter().filter(|i| i.flags & FLAG_CURSOR == 0).collect();
        assert!(
            non_cursor.iter().any(|i| i.pos == [0.0, 0.0] && i.flags & FLAG_NO_GLYPH != 0),
            "expected a bg quad at (0,0); got {non_cursor:?}",
        );
        // No glyph instance (no FLAG_NO_GLYPH cleared, no FLAG_COLOR_GLYPH).
        assert!(
            out.iter()
                .filter(|i| i.flags & FLAG_CURSOR == 0)
                .all(|i| i.flags & FLAG_NO_GLYPH != 0),
            "placeholder must not emit a glyph instance",
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
        let v = build_instances(&t, (8.0, 16.0), &Theme::default_dark(), None, |_, _, _, _| None);
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
        let v = build_instances(&t, (8.0, 16.0), &Theme::default_dark(), None, |_, _, _, _| None);
        assert_eq!(cursor_instance(&v).size, [8.0, 16.0]);
        // Switch to bar (Ps=5 → bar, blinking).
        feed(&mut t, b"\x1b[5 q");
        let v = build_instances(&t, (8.0, 16.0), &Theme::default_dark(), None, |_, _, _, _| None);
        assert!(cursor_instance(&v).size[0] < 8.0, "bar after Ps=5");
        // Switch to underline (Ps=4).
        feed(&mut t, b"\x1b[4 q");
        let v = build_instances(&t, (8.0, 16.0), &Theme::default_dark(), None, |_, _, _, _| None);
        assert!(
            cursor_instance(&v).size[1] < 16.0,
            "underline after Ps=4",
        );
        // Back to block (Ps=2).
        feed(&mut t, b"\x1b[2 q");
        let v = build_instances(&t, (8.0, 16.0), &Theme::default_dark(), None, |_, _, _, _| None);
        assert_eq!(cursor_instance(&v).size, [8.0, 16.0]);
    }

    // ----- M9 partial-redraw builder ----------------------------------------

    #[test]
    fn build_dirty_instances_empty_damage_emits_only_cursor() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"AB");
        t.clear_damage();
        let mut out = Vec::new();
        super::build_dirty_instances_into(
            &mut out,
            &t,
            t.damage(),
            (8.0, 16.0),
            &Theme::default_dark(),
            None,
            true,
            |_, _, _, _| None,
            |_, _| false,
        );
        // No dirty cells, cursor visible: only the cursor instance.
        assert_eq!(out.len(), 1);
        assert!(out[0].flags & FLAG_CURSOR != 0);
    }

    #[test]
    fn build_dirty_instances_single_cell_emits_bg_quad_and_cursor() {
        let mut t = Term::new(2, 8, 0);
        feed(&mut t, b"A"); // (0, 0) dirty
        let damage = t.damage().clone();
        let mut out = Vec::new();
        super::build_dirty_instances_into(
            &mut out,
            &t,
            &damage,
            (8.0, 16.0),
            &Theme::default_dark(),
            None,
            true,
            |_, _, _, _| None,
            |_, _| false,
        );
        // 1 bg quad + cursor.
        assert_eq!(out.len(), 2);
        assert!(out[1].flags & FLAG_CURSOR != 0);
        assert_eq!(out[0].pos, [0.0, 0.0]);
    }

    #[test]
    fn build_dirty_instances_blank_cell_still_emits_bg_quad() {
        // Manually mark cell (0, 0) dirty, but leave it blank. Under
        // LoadOp::Load we MUST emit a bg quad so any prior glyph at
        // that position is overpainted (the "ghost text" gotcha in
        // the M9 plan).
        let t = Term::new(1, 4, 0);
        // t.damage() starts with `all` flag set; produce a custom damage
        // with just one dirty cell.
        let mut damage = Damage::new(1);
        damage.clear();
        damage.rows[0].mark(0);
        let mut out = Vec::new();
        super::build_dirty_instances_into(
            &mut out,
            &t,
            &damage,
            (8.0, 16.0),
            &Theme::default_dark(),
            None,
            true,
            |_, _, _, _| None,
            |_, _| false,
        );
        // 1 bg quad (even though the cell is blank) + cursor.
        let bgs: Vec<_> = out.iter().filter(|i| i.flags & FLAG_CURSOR == 0).collect();
        assert_eq!(bgs.len(), 1, "blank dirty cell must emit a bg quad");
        // And it has the no-glyph flag.
        assert!(bgs[0].flags & FLAG_NO_GLYPH != 0);
    }

    #[test]
    fn build_dirty_instances_all_matches_full_build() {
        // damage.all → delegate to build_instances_into. Output must
        // match the full builder (sans any cursor visibility gating).
        let mut t = Term::new(2, 6, 0);
        feed(&mut t, b"hello"); // some content
        let mut full = Vec::new();
        super::build_instances_into(
            &mut full,
            &t,
            (8.0, 16.0),
            &Theme::default_dark(),
            None,
            |_, _, _, _| None,
            |_, _| false,
        );
        let mut dirty = Vec::new();
        super::build_dirty_instances_into(
            &mut dirty,
            &t,
            t.damage(),
            (8.0, 16.0),
            &Theme::default_dark(),
            None,
            true,
            |_, _, _, _| None,
            |_, _| false,
        );
        // Same instance count (cursor + per-cell quads).
        assert_eq!(full.len(), dirty.len());
    }

    #[test]
    fn build_dirty_instances_skips_continuation() {
        // CJK ideograph at col 0; mark both primary and continuation
        // dirty. The continuation must be skipped (no per-cell quad).
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, "你".as_bytes());
        let damage = t.damage().clone();
        let mut out = Vec::new();
        super::build_dirty_instances_into(
            &mut out,
            &t,
            &damage,
            (8.0, 16.0),
            &Theme::default_dark(),
            None,
            true,
            |_, _, _, _| None,
            |_, _| false,
        );
        // Only one non-cursor instance: the primary cell's bg quad
        // (continuation skipped).
        let bgs: Vec<_> = out.iter().filter(|i| i.flags & FLAG_CURSOR == 0).collect();
        assert_eq!(bgs.len(), 1);
        assert_eq!(bgs[0].pos[0], 0.0);
    }

    #[test]
    fn build_dirty_instances_cursor_visibility_gates_cursor_instance() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"A");
        let mut out = Vec::new();
        super::build_dirty_instances_into(
            &mut out,
            &t,
            t.damage(),
            (8.0, 16.0),
            &Theme::default_dark(),
            None,
            false, // cursor hidden (mid-blink off-phase)
            |_, _, _, _| None,
            |_, _| false,
        );
        // No cursor instance.
        assert!(out.iter().all(|i| i.flags & FLAG_CURSOR == 0));
    }

    #[test]
    fn build_dirty_instances_all_with_invisible_cursor_drops_cursor() {
        // When damage.all triggers the full-build delegate, the
        // builder still pops the cursor instance off if cursor_visible
        // is false. Test the off path.
        let mut t = Term::new(1, 2, 0);
        feed(&mut t, b"x");
        assert!(t.damage().all);
        let mut out = Vec::new();
        super::build_dirty_instances_into(
            &mut out,
            &t,
            t.damage(),
            (8.0, 16.0),
            &Theme::default_dark(),
            None,
            false,
            |_, _, _, _| None,
            |_, _| false,
        );
        // No cursor instance.
        assert!(out.iter().all(|i| i.flags & FLAG_CURSOR == 0));
    }

    // ----- M10-followup C1: OSC 4 ext_palette threads to rendered pixels --

    /// Build a renderer-style `ext_palette` mirroring the term's
    /// `palette_overrides + xterm defaults`, linearised. Mirrors
    /// `Renderer::rebuild_ext_palette`.
    fn rebuild_ext_palette_for_test(term: &Term) -> Box<[[f32; 4]; 256]> {
        let mut out: Box<[[f32; 4]; 256]> = Box::new([[0.0; 4]; 256]);
        for idx in 0u16..=255 {
            let idx_u8 = idx as u8;
            let rgb = term
                .palette_override(idx_u8)
                .unwrap_or_else(|| toastty_protocols::palette::default_xterm_256(idx_u8));
            out[idx as usize] = srgb_to_linear_rgba(rgb[0], rgb[1], rgb[2]);
        }
        out
    }

    /// Followup C1: OSC 4 palette overrides must reach the GPU. Without
    /// the fix, the instance builder consulted hard-coded xterm cube
    /// math and never looked at `palette_overrides`, so the override
    /// rebuilt into `ext_palette` was rendered-irrelevant. After the
    /// fix, the same color index resolves through `ext_palette` and the
    /// CellInstance's fg reflects the override.
    #[test]
    fn osc4_palette_override_reaches_cell_instance() {
        let theme = Theme::default_dark();

        // Print one cell using xterm index 50 (cube color: roughly cyan).
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[38;5;50mX");

        // Default build (no override yet). Snapshot the fg color of the
        // glyph instance for index 50. We pull the bg quad (FLAG_NO_GLYPH
        // set, not the underline strip) — its fg encodes the SGR foreground.
        let default_ext = rebuild_ext_palette_for_test(&t);
        let v_default = build_instances(
            &t,
            (8.0, 16.0),
            &theme,
            Some(&default_ext),
            |_, _, _, _| None,
        );
        let default_fg = v_default
            .iter()
            .find(|i| i.flags & FLAG_CURSOR == 0 && i.flags & FLAG_NO_GLYPH != 0)
            .expect("bg quad for printed cell")
            .fg;

        // Now drive an OSC 4 override that pins index 50 → pure red.
        // The term increments palette_revision and marks all dirty.
        feed(&mut t, b"\x1b]4;50;rgb:ff/00/00\x1b\\");
        let override_rgb = t.palette_override(50).expect("override set");
        assert_eq!(override_rgb, [0xff, 0x00, 0x00]);

        // Rebuild the cached ext_palette to mirror what the renderer
        // would do on `palette_revision` change, then re-emit instances.
        let after_ext = rebuild_ext_palette_for_test(&t);
        let v_after = build_instances(
            &t,
            (8.0, 16.0),
            &theme,
            Some(&after_ext),
            |_, _, _, _| None,
        );
        let after_fg = v_after
            .iter()
            .find(|i| i.flags & FLAG_CURSOR == 0 && i.flags & FLAG_NO_GLYPH != 0)
            .expect("bg quad for printed cell")
            .fg;

        // The fg must have changed — proving the override actually
        // reached the instance buffer.
        assert_ne!(default_fg, after_fg, "OSC 4 override must change rendered color");

        // And it must equal the linearised override (red).
        let expected = srgb_to_linear_rgba(0xff, 0x00, 0x00);
        for (i, (a, b)) in after_fg.iter().zip(expected.iter()).enumerate() {
            assert!(
                (a - b).abs() < 1e-6,
                "channel {i}: got {a}, expected {b} (override → linearised red)"
            );
        }
    }

    /// Followup C1: the theme's 16-entry `palette` continues to win for
    /// indices 0..16 even when `ext_palette` is supplied — so a user
    /// theme keeps full control of base ANSI colors.
    #[test]
    fn ext_palette_does_not_override_base_16_palette() {
        let theme = Theme::default_dark();
        // Build an ext_palette that's nonsense for idx 0..16 (all white).
        let mut ext: Box<[[f32; 4]; 256]> = Box::new([[1.0, 1.0, 1.0, 1.0]; 256]);
        // Index 1 (Red) — copy what the theme expects so the assertion
        // below is clearly anchored to the theme path, not ext_palette.
        ext[1] = [1.0, 1.0, 1.0, 1.0];
        // resolve_indexed256(1, Some(&ext)) must return theme.palette[1],
        // NOT ext[1].
        let v = theme.resolve_indexed256(1, Some(&ext));
        assert_eq!(v, theme.palette[1]);
        assert_ne!(v, ext[1], "base 16 must come from theme, not ext_palette");
    }

    // ----- M10-followup I6: width-2 hyperlink underline spans both columns -

    /// Followup I6: a width-2 (CJK) primary with an active OSC 8
    /// hyperlink must produce an underline strip spanning the FULL
    /// cluster — two columns — not just the primary column.
    #[test]
    fn width2_hyperlinked_cluster_underline_spans_two_columns() {
        // OSC 8 open + width-2 CJK char + OSC 8 close.
        let mut t = Term::new(1, 6, 0);
        feed(
            &mut t,
            "\x1b]8;;https://example.com\x1b\\你\x1b]8;;\x1b\\".as_bytes(),
        );
        let cell_w = 8.0_f32;
        let v = build_instances(&t, (cell_w, 16.0), &Theme::default_dark(), None, |_, _, _, _| None);
        let underline = v
            .iter()
            .find(|i| i.flags & FLAG_UNDERLINE != 0)
            .expect("hyperlinked width-2 cluster must emit one underline strip");
        // Width must be exactly 2 * cell_w — covers both the primary
        // column AND the continuation column.
        assert!(
            (underline.size[0] - 2.0 * cell_w).abs() < 1e-3,
            "width-2 underline strip width = {} (expected {})",
            underline.size[0],
            2.0 * cell_w,
        );
    }

    /// Followup I6: a width-1 (ASCII) hyperlinked cell underlines just
    /// its own column — the width-doubling logic must NOT misfire when
    /// the next cell isn't a continuation.
    #[test]
    fn width1_hyperlinked_cell_underline_is_one_column() {
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b]8;;https://example.com\x1b\\X");
        let cell_w = 8.0_f32;
        let v = build_instances(&t, (cell_w, 16.0), &Theme::default_dark(), None, |_, _, _, _| None);
        let underline = v
            .iter()
            .find(|i| i.flags & FLAG_UNDERLINE != 0)
            .expect("underline strip");
        assert!(
            (underline.size[0] - cell_w).abs() < 1e-3,
            "width-1 underline strip width = {} (expected {})",
            underline.size[0],
            cell_w,
        );
    }

    /// Followup I6: same widening must apply to the partial-redraw
    /// builder. Mark the width-2 primary dirty (continuation skipped),
    /// run the dirty builder, and assert the underline spans both
    /// columns.
    #[test]
    fn width2_hyperlinked_cluster_underline_in_dirty_builder() {
        let mut t = Term::new(1, 6, 0);
        feed(
            &mut t,
            "\x1b]8;;https://example.com\x1b\\你\x1b]8;;\x1b\\".as_bytes(),
        );
        let cell_w = 8.0_f32;
        let damage = t.damage().clone();
        let mut out = Vec::new();
        super::build_dirty_instances_into(
            &mut out,
            &t,
            &damage,
            (cell_w, 16.0),
            &Theme::default_dark(),
            None,
            true,
            |_, _, _, _| None,
            |_, _| false,
        );
        let underline = out
            .iter()
            .find(|i| i.flags & FLAG_UNDERLINE != 0)
            .expect("dirty builder must emit underline strip for width-2 hyperlink");
        assert!(
            (underline.size[0] - 2.0 * cell_w).abs() < 1e-3,
            "dirty width-2 underline strip width = {} (expected {})",
            underline.size[0],
            2.0 * cell_w,
        );
    }
}
