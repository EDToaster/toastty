//! wgpu renderer.
//!
//! Layout: pure-function submodules are coverage-gated like the logic
//! crates; pipeline / device / submit code is covered by snapshot and
//! property tests under `tests/`. Shaders under `shaders/` are validated
//! at build time by `build.rs`.
//!
//! See [`docs/decisions/text-stack.md`](../../docs/decisions/text-stack.md),
//! [`docs/decisions/rgp-3d-path.md`](../../docs/decisions/rgp-3d-path.md),
//! [`docs/decisions/redraw-policy.md`](../../docs/decisions/redraw-policy.md),
//! [`docs/decisions/shader-pipeline.md`](../../docs/decisions/shader-pipeline.md).
//!
//! ## Scope (M4a)
//!
//! Device + surface + clear-color path only. Text, glyph atlas, RGP, and
//! post-process come in M4b and beyond.
//!
//! ## Validation gates
//!
//! Validation + debug instance flags are **on** under `cfg(test)` (and
//! exposed for the snapshot harness via [`instance_flags_for_tests`])
//! and **off** in normal builds, per decision §8.

#![forbid(unsafe_code)]

pub mod chip;
pub mod color;
pub mod dialog;
pub mod image;
pub mod rgp;
pub mod surface_format;
#[cfg(test)]
mod test_gpu;
pub mod text;

use std::time::{Duration, Instant};

use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use thiserror::Error;
use toastty_protocols::unicode_core::cluster_cell_width;
use toastty_term::Term;
use wgpu::{
    BackendOptions, Backends, CompositeAlphaMode, Device, DeviceDescriptor, Instance,
    InstanceDescriptor, InstanceFlags, MemoryBudgetThresholds, PowerPreference, PresentMode, Queue,
    RequestAdapterOptions, Surface, SurfaceConfiguration, TextureFormat, TextureUsages,
};

use crate::chip::ChipFade;
use crate::chip::pipeline::{ChipPipeline, ChipUniform};
use crate::dialog::pipeline::UiPipeline;
pub use crate::dialog::{ClosePrompt, DialogButton};
use crate::image::atlas::ImageTextureCache;
use crate::image::instance::{
    ImageInstance, ImageView, Layer, build_image_instances, classify_layer, split_layers,
};
use crate::image::pipeline::ImagePipeline;
use crate::rgp::{GpuAssetCache, Rgp3dPipeline};
use crate::text::glyph_rasterizer::{DEFAULT_LINE_HEIGHT_RATIO, GlyphRasterizer};
use crate::text::instance::{CellInstance, CellRenderOpts, Theme};
use crate::text::pipeline::{GlobalsUbo, TextPipeline};
use crate::text::row_shaper::RowShaper;

/// True when a placement that actually draws sits in the kitty
/// `z < INT32_MIN/2` band, which draws under non-default cell
/// backgrounds (SIZE-9). Uses the same skips as `build_image_instances`
/// (image missing from the registry, empty span), so a leftover
/// placement doesn't force full redraws for nothing.
fn has_below_cell_bg_placements(
    grid: &toastty_term::ImageGrid,
    registry: &toastty_term::ImageRegistry<toastty_term::ImageKey>,
) -> bool {
    grid.iter().any(|p| {
        classify_layer(p.z) == Layer::BelowCellBg
            && registry.contains(p.key)
            && p.cols != 0
            && p.rows != 0
            && !p.clip_rows.is_empty()
    })
}

/// Whether this frame must be a full redraw for the below-cell-bg band:
/// while such a placement is on screen, and for the one frame after it
/// goes (to repaint the cells it showed through). Updates `drawn_last`.
fn below_bg_forces_full_redraw(active: bool, drawn_last: &mut bool) -> bool {
    let force = active || *drawn_last;
    *drawn_last = active;
    force
}

/// Post-process the full-build cell instances: drop the trailing cursor
/// instance when the cursor is hidden.
///
/// While a `z < INT32_MIN/2` image is on screen the builder runs with
/// [`CellRenderOpts::skip_default_bg`], so that image (drawn before the
/// bg pass) shows through default-bg cells while non-default backgrounds
/// (SGR colors, reverse video, selection) still cover it (SIZE-9).
fn finish_full_build(instances: &mut Vec<CellInstance>, cursor_visible: bool) {
    if !cursor_visible {
        instances.pop();
    }
}

/// Append instances for `term` into `out`. The closure pulls glyph
/// slots from the line cache; missing entries fall through to a
/// background-only instance (the next frame, after re-shape, will fill
/// in the glyph).
#[allow(clippy::too_many_arguments)]
fn build_term_instances_into(
    out: &mut Vec<CellInstance>,
    term: &Term,
    cell_size: (f32, f32),
    theme: &Theme,
    ext_palette: &[[f32; 4]; 256],
    row_glyphs: &RowShaper,
    bleed: EdgeBleed<'_>,
    content_h: f32,
    opts: &CellRenderOpts,
) {
    crate::text::instance::build_instances_into_opts(
        out,
        term,
        cell_size,
        theme,
        Some(ext_palette),
        |row, col, ch, _style| row_glyphs.glyph(row, col, ch),
        |line_id, col| term.is_cell_selected(line_id, col),
        bleed,
        content_h,
        opts,
    );
}

/// Append partial-redraw instances for `term` into `out` using the
/// per-cell damage signal. Backed by
/// [`crate::text::instance::build_dirty_instances_into`].
#[allow(clippy::too_many_arguments)]
fn build_term_dirty_instances_into(
    out: &mut Vec<CellInstance>,
    term: &Term,
    cell_size: (f32, f32),
    theme: &Theme,
    ext_palette: &[[f32; 4]; 256],
    cursor_visible: bool,
    row_glyphs: &RowShaper,
    bleed: EdgeBleed<'_>,
    opts: &CellRenderOpts,
) {
    crate::text::instance::build_dirty_instances_into_opts(
        out,
        term,
        term.damage(),
        cell_size,
        theme,
        Some(ext_palette),
        cursor_visible,
        |row, col, ch, _style| row_glyphs.glyph(row, col, ch),
        |line_id, col| term.is_cell_selected(line_id, col),
        bleed,
        opts,
    );
}

/// Mark every visible SGR-blink cell dirty (text-blink tick). Rows come
/// from the renderer's `row_blink` cache (render-row space, which is
/// the damage space when not fractionally scrolled). During a
/// fractional scroll the frame is a full redraw anyway, so just mark
/// everything.
/// Clip rect for image instances, `[x0, y0, x1, y1]` relative to the
/// content origin, matching what the text pass covers so images and text
/// are cut at the same edges. At the live bottom that is the grid itself
/// (`cols × rows` cells, not the sub-cell sliver below the last row).
/// While scrolled back or mid smooth-scroll the text pass also draws the
/// partial row hanging above the top (into the top padding) and the rows
/// below the grid that fill the content area, so images get the same span.
#[allow(clippy::cast_precision_loss)]
fn image_clip(term: &Term, cell: (f32, f32), content_h: f32) -> [f32; 4] {
    let (rows, cols) = term.size();
    let grid_w = f32::from(cols) * cell.0;
    let view_pixel = term.view_offset_pixel();
    if term.view_offset_lines() == 0 && view_pixel <= 0.0 {
        return [0.0, 0.0, grid_w, f32::from(rows) * cell.1];
    }
    let y_translate = if view_pixel > 0.0 {
        view_pixel - cell.1
    } else {
        0.0
    };
    let drawn = crate::text::instance::rows_to_render(rows, view_pixel, cell.1, content_h);
    [
        0.0,
        y_translate.min(0.0),
        grid_w,
        y_translate + f32::from(drawn) * cell.1,
    ]
}

fn mark_blinking_cells_dirty(term: &mut Term, row_blink: &[bool]) {
    if term.view_offset_pixel() > 0.0 {
        term.mark_all_dirty();
        return;
    }
    let (rows, _) = term.size();
    let mut cols: Vec<u16> = Vec::new();
    for (r, _) in row_blink.iter().enumerate().filter(|(_, b)| **b) {
        let Ok(r) = u16::try_from(r) else { break };
        if r >= rows {
            break;
        }
        cols.clear();
        cols.extend(
            term.view_row(r)
                .cells()
                .iter()
                .enumerate()
                .filter(|(_, c)| c.style.flags.blink())
                .filter_map(|(c, _)| u16::try_from(c).ok()),
        );
        for &c in &cols {
            term.mark_cell_dirty(r, c);
        }
    }
}

/// Normalize an IME preedit argument into stored state: `None` or an
/// empty string both clear the overlay (yield `None`); any non-empty
/// string is owned. Factored out of [`Renderer::set_preedit`] so the
/// empty-string normalization is unit-testable without a GPU.
fn normalize_preedit(text: Option<&str>) -> Option<String> {
    match text {
        Some(s) if !s.is_empty() => Some(s.to_owned()),
        _ => None,
    }
}

/// Bundled fallback font: `FiraMono Medium` (OFL).
///
/// Even when the host has system monospace fonts, embedding one makes
/// snapshot tests deterministic across machines.
const BUNDLED_FONT: &[u8] = include_bytes!("../fonts/FiraMono-Medium.ttf");

/// Default font pixel size for the demo and snapshot tests.
pub const DEFAULT_FONT_SIZE_PX: f32 = 16.0;

/// Re-export the renderer's default line-height ratio so callers can
/// build the matching `with_font_ex` invocation without depending on the
/// `text` submodule directly.
pub use crate::text::glyph_rasterizer::DEFAULT_LINE_HEIGHT_RATIO as DEFAULT_LINE_HEIGHT;

/// Outcome of a [`Renderer::render_term`] call.
///
/// The pause gate for DECSET 2026 returns [`RenderOutcome::Skipped`]; a
/// frame that actually went through encoder + submit returns
/// [`RenderOutcome::Rendered`]. Callers use this to gate the "clear
/// dirty bitset + clear BSU force-flushed flag" cleanup so the signals
/// survive across a skipped frame (followup C2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum RenderOutcome {
    /// Renderer emitted a frame: the dirty list and any
    /// `sync_output_force_flushed` flag were consumed and should be
    /// cleared by the caller.
    Rendered,
    /// Renderer skipped this frame (currently: DECSET 2026 paused). The
    /// caller must NOT clear the dirty list or the BSU force-flushed
    /// flag — both must persist so the next non-skipped frame still
    /// performs the corrective full redraw.
    Skipped,
}

/// Errors from [`Renderer`] construction or rendering.
#[derive(Debug, Error)]
pub enum RenderError {
    #[error("failed to create surface: {0}")]
    Surface(#[from] wgpu::CreateSurfaceError),
    #[error("no compatible GPU adapter found")]
    NoAdapter,
    #[error("device request failed: {0}")]
    RequestDevice(#[from] wgpu::RequestDeviceError),
    #[error("surface has no supported formats — bug in wgpu/driver")]
    NoSurfaceFormat,
    #[error("surface lost; recreate")]
    SurfaceLost,
    #[error("font not configured — call Renderer::with_font first")]
    FontNotConfigured,
}

/// Instance flags to use in tests / the snapshot harness.
///
/// Exposed publicly so the integration test under `tests/snapshots.rs` can
/// share exactly the same flags as `Renderer::new` does under `cfg(test)`.
#[must_use]
pub fn instance_flags_for_tests() -> InstanceFlags {
    InstanceFlags::VALIDATION | InstanceFlags::DEBUG
}

/// Instance flags for release builds: empty (validation off — it has a
/// real perf cost on the hot path).
#[must_use]
pub fn instance_flags_for_release() -> InstanceFlags {
    InstanceFlags::empty()
}

/// Build an `InstanceDescriptor` with sensible defaults for toastty.
///
/// Public so the snapshot harness can use it for a headless instance.
#[must_use]
pub fn instance_descriptor(flags: InstanceFlags) -> InstanceDescriptor {
    InstanceDescriptor {
        backends: Backends::PRIMARY,
        flags,
        backend_options: BackendOptions::default(),
        memory_budget_thresholds: MemoryBudgetThresholds::default(),
        display: None,
    }
}

/// The renderer.
///
/// Owns the wgpu device, queue, surface, and current surface config. M4a
/// scope: clear-to-color only. M4b will add text/cell pipelines on top.
#[derive(Debug)]
pub struct Renderer {
    device: Device,
    queue: Queue,
    surface: Surface<'static>,
    config: SurfaceConfiguration,
    clear_color: [f32; 4],
    /// Text pipeline lives lazily — `with_font` initializes it; `render`
    /// (M4a path) tolerates its absence; `render_term` requires it.
    text: Option<TextState>,
    /// Theme used by `render_term` when emitting instances: `base_theme`,
    /// or its fg/bg swap while the terminal is in reverse video
    /// (DECSCNM).
    theme: Theme,
    /// Theme as configured via [`Renderer::set_theme`].
    base_theme: Theme,
    /// DECSCNM state `theme` was last derived for.
    reverse_video: bool,
    /// True when the next frame must use `LoadOp::Clear` instead of
    /// `LoadOp::Load`. Set on construction (so the very first frame
    /// fully paints), on resize / theme / font swap, and whenever the
    /// term reports `damage.all` (M8 corrective-flush path).
    needs_full_clear: bool,
    /// Cursor blink state machine.
    blink: CursorBlink,
    /// SGR 5 / 6 text-blink phase + `[text] blink` switch.
    text_blink: TextBlink,
    /// Cached linear-light extended palette derived from the term's
    /// OSC 4 overrides and the built-in xterm 256-color table. Rebuilt
    /// when the term's `palette_revision()` differs from
    /// `palette_revision_seen`.
    ext_palette: Box<[[f32; 4]; 256]>,
    /// Last `Term::palette_revision()` we incorporated into
    /// `ext_palette`. `u32::MAX` marks "never seen" so the first frame
    /// always rebuilds.
    palette_revision_seen: u32,
    /// Renderer-owned offscreen "framebuffer" we render every frame into.
    /// Same format and size as the swapchain back-buffer. The damage-
    /// tracked partial-redraw path requires a stable previous-frame
    /// target; the swapchain rotates buffers under us, so this texture
    /// is the persistent target. After each render pass we
    /// `copy_texture_to_texture` from this into the swapchain frame.
    scratch_texture: wgpu::Texture,
    /// Cached view of [`Self::scratch_texture`], the actual render-pass
    /// color attachment.
    scratch_view: wgpu::TextureView,
    /// M11a: image-drawing pipeline. Lazy-initialized in
    /// [`Renderer::with_font_ex`] so headless tests that don't touch
    /// images can construct a `Renderer` without paying the cost.
    image_pipeline: Option<ImagePipeline>,
    /// CPU-side mirror of the texture cache: which kitty images are
    /// resident, byte-budgeted with LRU-by-last-draw eviction.
    image_tex_cache: ImageTextureCache,
    /// Last `Term::image_revision` we synced.
    image_revision_seen: u32,
    /// Reusable storage for the below-cell-bg / below-text / above-text
    /// instance vectors. Cleared every frame; allocation survives across
    /// frames. The below-cell-bg band (z < INT32_MIN/2) draws beneath the
    /// text background pass; below-text (INT32_MIN/2 <= z < 0) above bg
    /// but below glyphs; above-text (z >= 0) on top.
    image_instances_below_bg: Vec<ImageInstance>,
    image_instances_below: Vec<ImageInstance>,
    image_instances_above: Vec<ImageInstance>,
    /// Rich blocks (OSC 7700): GPU textures per block image.
    rich_tex_cache: crate::image::rich::RichTextureCache,
    /// Rich-block slices + selection tint drawn this frame.
    rich_instances: Vec<ImageInstance>,
    /// Rich slices were drawn last frame (a frame that stops drawing
    /// them must repaint everything).
    rich_drawn_last: bool,
    /// A `z < INT32_MIN/2` placement was on screen last frame. Those
    /// frames drop default-bg quads, which needs a full clear.
    below_bg_drawn_last: bool,
    /// SIZE-12: the last image resolve deferred mip work (per-frame cap
    /// hit). Keeps frames coming (`next_redraw_deadline`, skip gate)
    /// until every drawn-downscaled image has its levels.
    mip_work_pending: bool,
    /// Last viewport offset rendered. When it changes we force a full
    /// clear — partial redraw can't reliably overpaint the previous
    /// frame at the new y-translation (blank cells skip emission, so
    /// old non-blank content would leak through).
    last_view_offset: (u32, f32),
    // ----- M12d: RGP 3D pass -----
    /// Depth attachment shared by the RGP, text, and image pipelines.
    /// Same dims as the scratch color texture; recreated on resize.
    /// Format: `Depth32Float`. RGP renders into this with
    /// `depth_compare: LessEqual`; text/image both write z=0.5 via
    /// their shaders so 3D objects can occlude or sit underneath.
    scratch_depth_texture: wgpu::Texture,
    scratch_depth_view: wgpu::TextureView,
    /// 3D pipeline + draw-uniform slot pool. Lazy-initialised
    /// alongside the text pipeline in `with_font_ex`.
    rgp_pipeline: Option<Rgp3dPipeline>,
    /// GPU mesh cache keyed by RGP asset id.
    rgp_cache: GpuAssetCache,
    /// Last `Term::rgp_revision()` we repainted at. Drives the 3D
    /// layer repaint + cell re-emit.
    rgp_revision_seen: u32,
    /// Last `Term::rgp_asset_revision()` we uploaded GPU meshes for.
    /// Drives the mesh-cache re-upload; kept separate so transform-only
    /// `u` updates repaint without re-uploading geometry.
    rgp_asset_revision_seen: u32,
    /// Optional debug overlay text rendered at the top-right of the
    /// surface every frame (e.g. an FPS counter). When `Some`, the
    /// skip-submit gate is bypassed so the overlay refreshes even when
    /// the term grid is idle.
    debug_overlay: Option<String>,
    /// Optional multi-line full-width error banner rendered at the top
    /// of the surface every frame. Used by the binary to surface config
    /// parse/validation failures inside the running window so the user
    /// notices without scanning stderr. When `Some`, bypasses the
    /// skip-submit gate so the banner shows up even on an idle grid.
    error_banner: Option<String>,
    /// Close-confirmation dialog ([`crate::dialog`]): a modal card over a
    /// dimmed window, drawn on top of everything while `Some`. Set by the
    /// binary when the user tries to close the window while a program is
    /// still running. When `Some`, bypasses the skip-submit gate and forces
    /// full repaints (the scrim is translucent, so it must not stack on a
    /// `LoadOp::Load` frame).
    close_prompt: Option<ClosePrompt>,
    /// Close-dialog button under the pointer (lifts it).
    close_dialog_hover: Option<DialogButton>,
    /// SDF pipeline for the close dialog's shapes. Lazily built
    /// alongside the text pipeline.
    ui_pipeline: Option<UiPipeline>,
    /// Optional IME preedit (in-progress composition) string drawn inline
    /// at the terminal cursor every frame. Fed by the platform IME
    /// (e.g. fcitx5) while the user composes Hangul/CJK; it is NOT part of
    /// the terminal grid and is overlaid each frame until the IME commits
    /// or clears it. Drawn underlined to distinguish it from committed
    /// text. When `Some`, bypasses the skip-submit gate so the overlay
    /// refreshes even on an idle grid (same as `debug_overlay`).
    preedit: Option<String>,
    /// `TOASTTY_TRACE_RENDER` env var sampled once at construction.
    /// Sampling per-frame was a measurable syscall on the hot path.
    trace_render: bool,
    /// True when [`Self::scratch_texture`] holds stale pixels (its last
    /// write was a direct-to-swapchain frame that bypassed it). The
    /// partial-redraw path needs `LoadOp::Load` to read a current
    /// previous frame, so when `scratch_stale` is set and a partial
    /// redraw is requested we cascade to a full clear instead. Cleared
    /// when a scratch-path frame restores the texture.
    scratch_stale: bool,
    /// Scroll-to-bottom button: `Some(corner)` enables it (the binary maps
    /// the `[scroll_button]` config here), `None` disables it. When
    /// enabled, a small round chip ([`crate::chip`]) is painted in
    /// `corner` while the view is scrolled back.
    scroll_button: Option<ScrollButtonCorner>,
    /// SDF pipeline that draws the scroll-to-bottom chip. Lazily built
    /// alongside the text pipeline.
    chip_pipeline: Option<ChipPipeline>,
    /// Window scale factor (physical px per logical px) used to size
    /// pixel-based UI such as the chip. Pushed by the binary.
    ui_scale: f32,
    /// Pointer is over the chip's hit rect (brightens it).
    chip_hover: bool,
    /// Fade-in / fade-out state of the chip.
    chip_fade: ChipFade,
    /// The chip was composited into the previous frame, so its pixels
    /// must be cleared even if it is now fully faded out.
    chip_drawn_last: bool,
    /// Rich block "Copy Markdown" chip: the view row where the hovered
    /// block's first visible row is drawn (`None` = hidden).
    rich_chip: Option<u16>,
    /// Pointer is over the "Copy Markdown" chip.
    rich_chip_hover: bool,
    /// Separate chip pipeline (own uniform buffer) for the copy chip, so
    /// it can be drawn in the same pass as the scroll chip.
    rich_chip_pipeline: Option<ChipPipeline>,
    /// Window-padding insets in **physical px** (the binary pre-scales
    /// logical px by `scale_factor`). The content grid is inset by these
    /// from the full surface (`config.width`/`config.height`, which always
    /// stay full-surface — see the surface-size contract). The content
    /// origin is `(pad_left, pad_top)`, fed into every pipeline's
    /// `content_origin` uniform.
    pad_top: u32,
    pad_right: u32,
    pad_bottom: u32,
    pad_left: u32,
    /// Global gate for edge-cell background extension ("overscan/bleed").
    /// Resolved against `Term::is_alt_active()` per frame into the
    /// `active` flag of the instance builders' [`EdgeBleed`].
    extend_background_when: ExtendBackgroundWhen,
    /// Per-axis edge-extension rule (left/right vs top/bottom). Fed into
    /// the builders' [`EdgeBleed`] alongside `extend_background_when`.
    extend_background: ExtendBackground,
    /// How the cell grid is aligned within the content area when the
    /// window isn't a whole number of cells (the floor-divide leftover).
    /// Shifts `content_origin` and the per-edge bleed split.
    grid_align: GridAlign,
    /// Previous frame's per-row "solid bg" flags (content-row space),
    /// cached so a flip in a row's horizontal `SolidLine` status can force
    /// a full clear (the dirty path can't see non-dirty edge cells). Empty
    /// unless `extend_background.horizontal == SolidLine`.
    solid_rows_prev: Vec<bool>,
    /// Previous frame's per-column "solid bg" flags. Same role as
    /// `solid_rows_prev` for vertical `SolidLine`. Also reused as the
    /// `col_fills` slice handed to the builders. Empty unless
    /// `extend_background.vertical == SolidLine`.
    solid_cols_prev: Vec<bool>,
}

/// Which corner the scroll-to-bottom button is anchored to. Render-side
/// mirror of `toastty_config::ScrollButtonPosition` (the render crate does
/// not depend on the config crate — the binary bridges the two, same as it
/// does for `Theme`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollButtonCorner {
    /// Bottom-right corner.
    BottomRight,
    /// Bottom-left corner.
    BottomLeft,
}

/// Global gate for edge-cell background extension ("overscan/bleed").
/// Render-side mirror of `toastty_config::ExtendBackgroundWhen` (the render
/// crate does not depend on the config crate — the binary bridges the two,
/// same as it does for `Theme` / [`ScrollButtonCorner`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtendBackgroundWhen {
    /// Never bleed edge-cell backgrounds into the padding gutter.
    Never,
    /// Bleed whenever the per-axis [`ExtendBackground`] rule allows.
    Always,
    /// Bleed only while the alternate screen is active (full-page TUIs).
    AltScreen,
}

impl ExtendBackgroundWhen {
    /// Resolve to a per-frame `bool` gate. `alt` is `Term::is_alt_active()`.
    #[must_use]
    fn active(self, alt: bool) -> bool {
        matches!(self, ExtendBackgroundWhen::Always)
            || (matches!(self, ExtendBackgroundWhen::AltScreen) && alt)
    }
}

/// Per-axis edge-background extension rule. Render-side mirror of
/// `toastty_config::ExtendBackground`. Combined with
/// [`ExtendBackgroundWhen`]: an edge cell bleeds along an axis iff the gate
/// is active AND that axis's [`ExtendCondition`] is met.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExtendBackground {
    /// Left/right gutters, decided per-row.
    pub horizontal: ExtendCondition,
    /// Top/bottom gutters, decided per-column.
    pub vertical: ExtendCondition,
}

/// How the cell grid is aligned within the content area when the window
/// isn't an exact multiple of the cell size. Render-side mirror of
/// `toastty_config::GridAlign` (the binary bridges the two, same as
/// [`ExtendBackground`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GridAlign {
    /// Pin the grid to the content origin (`pad_left`, `pad_top`); the
    /// floor-divide leftover sits on the right/bottom edges.
    #[default]
    TopLeft,
    /// Center the grid in the content area; the leftover is split evenly
    /// across opposite edges.
    Centered,
}

impl GridAlign {
    /// Fraction of the per-axis leftover placed on the *leading* (left /
    /// top) edge. The trailing edge gets the rest. `TopLeft` → 0 (all
    /// trailing); `Centered` → 0.5 (split evenly).
    #[must_use]
    fn leading_fraction(self) -> f32 {
        match self {
            GridAlign::TopLeft => 0.0,
            GridAlign::Centered => 0.5,
        }
    }
}

/// Pure geometry behind [`Renderer::grid_overflow`].
///
/// Returns `(rem, lead)`: `rem = [rem_w, rem_h]` is the sub-cell leftover
/// the floor-divided grid (`floor(content / cell)` cells) leaves over each
/// axis, and `lead = floor(rem * leading_fraction)` is the share assigned
/// to the leading (left/top) edge. The floor keeps the grid origin on a
/// whole pixel so glyphs stay crisp (centered alignment would otherwise
/// put every glyph on a half pixel for odd leftovers); the trailing edge
/// absorbs the extra pixel. A non-positive cell dimension yields zero
/// leftover for that axis (degenerate guard — no division).
#[must_use]
fn grid_overflow_px(
    content: (f32, f32),
    cell: (f32, f32),
    leading_fraction: f32,
) -> ([f32; 2], [f32; 2]) {
    let rem = |c: f32, cell: f32| -> f32 {
        if cell > 0.0 {
            c - (c / cell).floor() * cell
        } else {
            0.0
        }
    };
    let rem_w = rem(content.0, cell.0);
    let rem_h = rem(content.1, cell.1);
    (
        [rem_w, rem_h],
        [
            // Force integer offset and remainder
            (rem_w * leading_fraction).floor(),
            (rem_h * leading_fraction).floor(),
        ],
    )
}

/// Re-export the edge-bleed parameter struct so the binary and tests can
/// name it. Defined next to `CellInstance` in `text::instance`.
pub use crate::text::instance::{EdgeBleed, ExtendCondition};

/// Build the scratch render target. Same dims/format as the surface;
/// `RENDER_ATTACHMENT` so we can draw into it, `COPY_SRC` so we can
/// blit it to the swapchain back-buffer.
/// Pick the surface `CompositeAlphaMode`.
///
/// When `transparent` is true we prefer `PreMultiplied` so a sub-1.0 theme
/// background alpha shows the desktop through. If the surface/compositor
/// doesn't advertise `PreMultiplied`, we warn and fall back to the opaque
/// preference below. When `transparent` is false we keep the historical
/// behavior exactly: prefer `Opaque`, else the first advertised mode.
fn pick_alpha_mode(modes: &[CompositeAlphaMode], transparent: bool) -> CompositeAlphaMode {
    if transparent {
        if let Some(m) = modes
            .iter()
            .copied()
            .find(|m| *m == CompositeAlphaMode::PreMultiplied)
        {
            return m;
        }
        tracing::warn!(
            "transparency requested but the surface/compositor does not support \
             premultiplied-alpha compositing (CompositeAlphaMode::PreMultiplied); \
             falling back to an opaque surface"
        );
    }
    modes
        .iter()
        .copied()
        .find(|m| *m == CompositeAlphaMode::Opaque)
        .unwrap_or(modes[0])
}

/// Premultiply a straight-alpha RGBA color into the `(rgb * a, a)` form
/// expected by a premultiplied-alpha surface, returning a `wgpu::Color`.
///
/// No-op when `a == 1.0` (rgb * 1 == rgb), so this is mathematically
/// identical to the previous straight-alpha clear for opaque backgrounds;
/// it only enables premultiplied transparency when `a < 1.0`.
fn premultiplied_color(c: [f32; 4]) -> wgpu::Color {
    let a = f64::from(c[3]);
    wgpu::Color {
        r: f64::from(c[0]) * a,
        g: f64::from(c[1]) * a,
        b: f64::from(c[2]) * a,
        a,
    }
}

fn create_scratch(
    device: &Device,
    width: u32,
    height: u32,
    format: TextureFormat,
) -> (wgpu::Texture, wgpu::TextureView) {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("toastty-render scratch framebuffer"),
        size: wgpu::Extent3d {
            width: width.max(1),
            height: height.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    (texture, view)
}

/// Build the depth attachment that pairs with the scratch FB.
/// `Depth32Float`, same dims, `RENDER_ATTACHMENT` only.
fn create_scratch_depth(
    device: &Device,
    width: u32,
    height: u32,
) -> (wgpu::Texture, wgpu::TextureView) {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("toastty-render scratch depth"),
        size: wgpu::Extent3d {
            width: width.max(1),
            height: height.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Depth32Float,
        usage: TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    (texture, view)
}

/// Default cursor blink half-cycle (matches gnome-terminal / kitty).
pub const DEFAULT_CURSOR_BLINK_INTERVAL: Duration = Duration::from_millis(530);

/// Standalone cursor-blink state machine. Lives on `Renderer` but is
/// pulled out so the blink logic can be tested without a GPU.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CursorBlink {
    pub last_at: Instant,
    pub visible: bool,
    pub interval: Duration,
    /// Last observed value of the term's `cursor_blink` flag.
    /// Used by [`CursorBlink::sync_enabled`] to detect the false→true
    /// edge (DECSCUSR Ps=2/4/6 → Ps=1/3/5 / blink restore) and reset
    /// the phase so the next tick fires `interval` from the
    /// re-enable, not from a stale `last_at` (followup I1).
    pub prev_enabled: bool,
}

impl CursorBlink {
    fn new(now: Instant) -> Self {
        Self {
            last_at: now,
            visible: true,
            interval: DEFAULT_CURSOR_BLINK_INTERVAL,
            prev_enabled: true,
        }
    }

    /// True iff a tick is due. Always false when `blink_enabled` is
    /// false (DECSCUSR Ps=2/4/6 → steady cursor).
    fn animation_due(&self, blink_enabled: bool, now: Instant) -> bool {
        if !blink_enabled {
            return false;
        }
        now.saturating_duration_since(self.last_at) >= self.interval
    }

    /// Time until the next tick. `None` when blink is disabled.
    fn next_deadline(&self, blink_enabled: bool, now: Instant) -> Option<Duration> {
        if !blink_enabled {
            return None;
        }
        let next = self.last_at + self.interval;
        Some(next.saturating_duration_since(now))
    }

    /// Toggle visibility and stamp `last_at`. Called by `render_term`
    /// when `animation_due` returns true.
    fn tick(&mut self, now: Instant) {
        self.visible = !self.visible;
        self.last_at = now;
    }

    /// Force visibility on. Used when the term has blink disabled
    /// mid-cycle.
    fn force_visible(&mut self) {
        self.visible = true;
    }

    /// Followup I1: observe the term's current `cursor_blink` flag.
    /// On the false→true edge (steady → blinking), reset the phase by
    /// stamping `last_at = now` and forcing `visible = true`, so the
    /// first tick after re-enable fires a full `interval` later — not
    /// instantly (which would visually flicker the cursor).
    ///
    /// Must be called before [`CursorBlink::animation_due`] each
    /// frame so the edge detection runs at most once per frame.
    fn sync_enabled(&mut self, enabled: bool, now: Instant) {
        if enabled && !self.prev_enabled {
            // false → true: fresh cycle.
            self.last_at = now;
            self.visible = true;
        }
        self.prev_enabled = enabled;
    }
}

/// SGR 5 / 6 text-blink state. Mirrors the cursor blink: same
/// [`CursorBlink`] phase machine and half-cycle, but its own phase, and
/// only *armed* while blinking cells are on screen, so a screen with no
/// blinking text keeps skipping idle frames and schedules no wake-ups
/// (see [`Renderer::next_redraw_deadline`]).
#[derive(Debug, Clone, Copy)]
pub(crate) struct TextBlink {
    phase: CursorBlink,
    /// `[text] blink` config switch.
    enabled: bool,
    /// `enabled` and a blinking cell was visible at the last frame.
    active: bool,
}

impl TextBlink {
    fn new(now: Instant) -> Self {
        let mut phase = CursorBlink::new(now);
        // Start disarmed so the first blinking cell sees a false→true
        // edge and gets a full on-phase.
        phase.prev_enabled = false;
        Self {
            phase,
            enabled: true,
            active: false,
        }
    }

    /// Apply the config switch. Returns true if it changed. Disabling
    /// forces the visible phase so blinking text is drawn steadily.
    fn set_enabled(&mut self, enabled: bool) -> bool {
        let changed = self.enabled != enabled;
        self.enabled = enabled;
        if !enabled {
            self.active = false;
            self.phase.prev_enabled = false;
            self.phase.force_visible();
        }
        changed
    }

    /// True iff a phase flip is due this frame.
    fn due(&self, now: Instant) -> bool {
        self.phase.animation_due(self.active, now)
    }

    /// Time until the next flip, or `None` when disarmed.
    fn next_deadline(&self, now: Instant) -> Option<Duration> {
        self.phase.next_deadline(self.active, now)
    }

    fn tick(&mut self, now: Instant) {
        self.phase.tick(now);
    }

    /// Re-arm from whether any blinking cell is on screen this frame.
    /// A fresh arm starts a full visible half-cycle (same edge rule as
    /// the cursor); disarming leaves the phase visible.
    fn observe(&mut self, any_blinking: bool, now: Instant) {
        let active = self.enabled && any_blinking;
        self.phase.sync_enabled(active, now);
        if !active {
            self.phase.force_visible();
        }
        self.active = active;
    }

    /// Current phase: true = blinking text visible.
    fn blink_on(&self) -> bool {
        self.phase.visible
    }
}

struct TextState {
    rasterizer: GlyphRasterizer,
    pipeline: TextPipeline,
    bind_group: wgpu::BindGroup,
    /// Row-shape cache + the per-frame shaping loop.
    rows: RowShaper,
    /// Reusable instance buffer. Cleared (not freed) at the start of
    /// every `render_term` so the allocation survives across frames.
    instances_scratch: Vec<CellInstance>,
}

impl std::fmt::Debug for TextState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextState")
            .field("rasterizer", &self.rasterizer)
            .field("pipeline", &self.pipeline)
            .finish_non_exhaustive()
    }
}

/// Append glyph instances for one line of close-dialog text. `run`'s
/// origin is absolute (physical px); instances are content-relative, so
/// `content_origin` (which the vertex shader adds back) is subtracted.
/// Columns come from the shaper, so wide characters land where it put
/// them; each glyph takes the colour of the span covering its column.
#[allow(clippy::cast_precision_loss)]
fn push_dialog_text(
    queue: &Queue,
    text: &mut TextState,
    instances: &mut Vec<CellInstance>,
    run: &dialog::TextRun,
    color: impl Fn(dialog::Tone) -> [f32; 4],
    cell: (f32, f32),
    content_origin: [f32; 2],
) {
    use crate::text::instance::{FLAG_COLOR_GLYPH, FLAG_OVERLAY};
    use toastty_protocols::unicode_core::char_cell_width;
    let mut line = String::new();
    // (end column, tone) per span, for colouring by column.
    let mut tones: Vec<(usize, dialog::Tone)> = Vec::with_capacity(run.spans.len());
    let mut end = 0;
    for span in &run.spans {
        line.push_str(&span.text);
        end += span
            .text
            .chars()
            .map(|c| usize::from(char_cell_width(c, false)))
            .sum::<usize>();
        tones.push((end, span.tone));
    }
    if line.is_empty() {
        return;
    }
    let tone_at = |col: usize| {
        tones
            .iter()
            .find(|(end, _)| col < *end)
            .or(tones.last())
            .map_or(dialog::Tone::Strong, |t| t.1)
    };
    let lg = text.rasterizer.shape_line(queue, &line, false);
    let x0 = run.origin[0] - content_origin[0];
    let y0 = run.origin[1] - content_origin[1];
    let primary = lg
        .by_column
        .iter()
        .enumerate()
        .filter_map(|(col, slot)| slot.as_ref().map(|(ch, slot)| (col, Some(*ch), *slot)));
    let secondary = lg
        .secondary
        .iter()
        .map(|(col, slot)| (usize::from(*col), None, *slot));
    for (col, ch, slot) in primary.chain(secondary) {
        if ch.is_some_and(char::is_whitespace) {
            continue;
        }
        let fg = color(tone_at(col));
        let flags = if slot.is_color {
            FLAG_COLOR_GLYPH | FLAG_OVERLAY
        } else {
            FLAG_OVERLAY
        };
        instances.push(CellInstance {
            pos: [
                x0 + col as f32 * cell.0 + slot.glyph_offset[0],
                y0 + slot.glyph_offset[1],
            ],
            size: slot.glyph_size,
            uv_min: slot.uv_min,
            uv_max: slot.uv_max,
            fg,
            bg: [0.0; 4],
            flags,
            pad: [0; 3],
        });
    }
}

impl Renderer {
    /// Create a renderer attached to `window`.
    ///
    /// `size` is the physical pixel size of the window (use
    /// `Window::inner_size()` on the winit side, surfaced via
    /// `WindowRef::physical_size`).
    ///
    /// # Lifetime
    ///
    /// `window` is moved into the renderer's surface. Use an
    /// `Arc<Window>` (or anything that derefs to a window handle) so the
    /// caller can keep a copy for `request_redraw` etc. The example shows
    /// the pattern.
    ///
    /// When `transparent` is true, the surface is configured for
    /// premultiplied-alpha compositing (`CompositeAlphaMode::PreMultiplied`)
    /// so that a sub-1.0 theme background alpha shows the desktop through
    /// the terminal. When false (the default), the surface prefers
    /// `CompositeAlphaMode::Opaque` and the window is fully opaque
    /// regardless of the theme background alpha.
    pub async fn new<W>(
        window: W,
        size: (u32, u32),
        vsync: bool,
        transparent: bool,
    ) -> Result<Self, RenderError>
    where
        W: HasDisplayHandle + HasWindowHandle + Send + Sync + 'static,
    {
        let flags = if cfg!(test) {
            instance_flags_for_tests()
        } else {
            instance_flags_for_release()
        };

        let instance = Instance::new(instance_descriptor(flags));

        // create_surface takes `impl Into<SurfaceTarget>`. A `&'static W`
        // that impls `HasDisplayHandle + HasWindowHandle + Send + Sync` is
        // accepted via the blanket `From<T> for SurfaceTarget` impl.
        let surface = instance.create_surface(window)?;

        let adapter = instance
            .request_adapter(&RequestAdapterOptions {
                power_preference: PowerPreference::LowPower, // power-friendly
                force_fallback_adapter: false,
                compatible_surface: Some(&surface),
            })
            .await
            .map_err(|_| RenderError::NoAdapter)?;
        let info = adapter.get_info();
        tracing::info!(
            target: "render_trace",
            "wgpu adapter: name={:?} backend={:?} device_type={:?} vendor=0x{:04x} device=0x{:04x}",
            info.name, info.backend, info.device_type, info.vendor, info.device,
        );

        let (device, queue) = adapter
            .request_device(&DeviceDescriptor {
                label: Some("toastty-render device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_defaults()
                    .using_resolution(adapter.limits()),
                memory_hints: wgpu::MemoryHints::Performance,
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
                trace: wgpu::Trace::Off,
            })
            .await?;

        let caps = surface.get_capabilities(&adapter);
        let format = surface_format::pick(&caps.formats).ok_or(RenderError::NoSurfaceFormat)?;

        let config = SurfaceConfiguration {
            // COPY_DST: per-frame we copy from the renderer-owned scratch
            // texture into the swapchain back-buffer. Damage tracking
            // needs a stable previous-frame target, but the swapchain
            // rotates buffers (Fifo with 2-frame latency) so any given
            // back-buffer is 1–2 frames stale and `LoadOp::Load` reads
            // garbage. The scratch texture is stable; the copy bridges
            // it to whichever back-buffer the swapchain hands us.
            usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_DST,
            format,
            width: size.0.max(1),
            height: size.1.max(1),
            // vsync=true → AutoVsync (Fifo on every platform — strict
            // vsync, power-friendly). vsync=false → AutoNoVsync, which
            // wgpu maps to Immediate where supported, falling back to
            // Mailbox / Fifo. Tearing is possible under AutoNoVsync but
            // GPU latency is minimized.
            present_mode: if vsync {
                PresentMode::AutoVsync
            } else {
                PresentMode::AutoNoVsync
            },
            desired_maximum_frame_latency: 2,
            alpha_mode: pick_alpha_mode(&caps.alpha_modes, transparent),
            view_formats: vec![],
        };

        surface.configure(&device, &config);

        let (scratch_texture, scratch_view) =
            create_scratch(&device, config.width, config.height, format);
        let (scratch_depth_texture, scratch_depth_view) =
            create_scratch_depth(&device, config.width, config.height);

        Ok(Self {
            device,
            queue,
            surface,
            config,
            clear_color: [0.07, 0.07, 0.09, 1.0],
            text: None,
            theme: Theme::default_dark(),
            base_theme: Theme::default_dark(),
            reverse_video: false,
            // First frame after construction always clears: the scratch
            // texture's initial contents are undefined.
            needs_full_clear: true,
            blink: CursorBlink::new(Instant::now()),
            text_blink: TextBlink::new(Instant::now()),
            ext_palette: Box::new([[0.0, 0.0, 0.0, 1.0]; 256]),
            palette_revision_seen: u32::MAX,
            scratch_texture,
            scratch_view,
            image_pipeline: None,
            image_tex_cache: ImageTextureCache::default(),
            image_revision_seen: u32::MAX,
            image_instances_below_bg: Vec::new(),
            image_instances_below: Vec::new(),
            image_instances_above: Vec::new(),
            rich_tex_cache: crate::image::rich::RichTextureCache::default(),
            rich_instances: Vec::new(),
            rich_drawn_last: false,
            below_bg_drawn_last: false,
            mip_work_pending: false,
            last_view_offset: (0, 0.0),
            scratch_depth_texture,
            scratch_depth_view,
            rgp_pipeline: None,
            rgp_cache: GpuAssetCache::new(),
            rgp_revision_seen: u32::MAX,
            rgp_asset_revision_seen: u32::MAX,
            debug_overlay: None,
            error_banner: None,
            close_prompt: None,
            close_dialog_hover: None,
            ui_pipeline: None,
            preedit: None,
            trace_render: std::env::var_os("TOASTTY_TRACE_RENDER").is_some(),
            // The very first frame is always a full clear → goes
            // through the direct-to-swapchain path → leaves scratch
            // untouched. So scratch is "stale" from the start. Subsequent
            // frames pick up the cascade rule.
            scratch_stale: true,
            scroll_button: None,
            chip_pipeline: None,
            ui_scale: 1.0,
            chip_hover: false,
            chip_fade: ChipFade::default(),
            chip_drawn_last: false,
            rich_chip: None,
            rich_chip_hover: false,
            rich_chip_pipeline: None,
            pad_top: 0,
            pad_right: 0,
            pad_bottom: 0,
            pad_left: 0,
            extend_background_when: ExtendBackgroundWhen::Never,
            extend_background: ExtendBackground::default(),
            solid_rows_prev: Vec::new(),
            solid_cols_prev: Vec::new(),
            grid_align: GridAlign::TopLeft,
        })
    }

    /// Read-only view of the cached linear-light extended palette
    /// (256 entries, RGBA). The renderer rebuilds this on demand from
    /// the term's OSC 4 overrides plus the built-in xterm table.
    /// Public for diagnostics / tests.
    #[must_use]
    pub fn extended_palette(&self) -> &[[f32; 4]; 256] {
        &self.ext_palette
    }

    /// Rebuild `ext_palette` from the term's OSC 4 overrides. Called by
    /// `render_term` when the term's `palette_revision()` has changed
    /// since the last rebuild.
    fn rebuild_ext_palette(&mut self, term: &Term) {
        for idx in 0u16..=255 {
            let idx_u8 = idx as u8;
            let rgb = term
                .palette_override(idx_u8)
                .unwrap_or_else(|| toastty_protocols::palette::default_xterm_256(idx_u8));
            self.ext_palette[idx as usize] =
                crate::text::instance::srgb_to_linear_rgba(rgb[0], rgb[1], rgb[2]);
        }
        self.palette_revision_seen = term.palette_revision();
    }

    /// Initialize the text rendering pipeline at the default line-height
    /// ratio.
    ///
    /// `font_name` is forwarded to cosmic-text's `Attrs::family(...)`.
    /// If `None`, falls back to the bundled `FiraMono`. `font_size_px` is
    /// the pixel size of the glyph cell.
    ///
    /// Idempotent: calling twice rebuilds with new params. Must be
    /// called before [`Renderer::render_term`].
    ///
    /// Equivalent to
    /// `with_font_ex(font_name, font_size_px, DEFAULT_LINE_HEIGHT)`.
    pub fn with_font(&mut self, font_name: Option<&str>, font_size_px: f32) {
        self.with_font_ex(font_name, font_size_px, DEFAULT_LINE_HEIGHT_RATIO);
    }

    /// Initialize the text rendering pipeline with an explicit
    /// line-height multiplier.
    ///
    /// `line_height` is `× font_size_px`. The default is
    /// [`DEFAULT_LINE_HEIGHT`]. Callers loading a `toastty_config::FontConfig`
    /// should pass `font.line_height` here.
    pub fn with_font_ex(&mut self, font_name: Option<&str>, font_size_px: f32, line_height: f32) {
        // Resolve the font family. We always bundle FiraMono so the
        // caller can pass `None` and still get text.
        let family = font_name.unwrap_or("Fira Mono");
        let rasterizer = GlyphRasterizer::new(
            &self.device,
            font_size_px,
            line_height,
            Some(family),
            Some(BUNDLED_FONT),
        );
        let pipeline = TextPipeline::new(&self.device, self.config.format);

        let mask_view = text::pipeline::default_view(rasterizer.mask_texture());
        let color_view = text::pipeline::default_view(rasterizer.color_texture());
        let bind_group = pipeline.make_bind_group(&self.device, &mask_view, &color_view);

        self.text = Some(TextState {
            rasterizer,
            pipeline,
            bind_group,
            rows: RowShaper::new(),
            instances_scratch: Vec::new(),
        });
        // Lazy-init the image pipeline alongside the text pipeline so
        // both are ready before the first `render_term`. The image
        // pipeline shares the swapchain format.
        if self.image_pipeline.is_none() {
            self.image_pipeline = Some(ImagePipeline::new(&self.device, self.config.format));
        }
        if self.rich_chip_pipeline.is_none() {
            self.rich_chip_pipeline = Some(ChipPipeline::new(&self.device, self.config.format));
        }
        if self.chip_pipeline.is_none() {
            self.chip_pipeline = Some(ChipPipeline::new(&self.device, self.config.format));
        }
        if self.ui_pipeline.is_none() {
            self.ui_pipeline = Some(UiPipeline::new(&self.device, self.config.format));
        }
        // Same for the RGP pipeline.
        if self.rgp_pipeline.is_none() {
            self.rgp_pipeline = Some(Rgp3dPipeline::new(&self.device, self.config.format));
        }
        // Font swap invalidates the cell grid — force the next frame
        // to clear.
        self.needs_full_clear = true;
    }

    /// Change only the font size, keeping the family, font database and
    /// GPU pipelines (runtime font zoom). Re-rasterises lazily: the glyph
    /// atlas, per-char cache and every row's shaped glyphs are dropped,
    /// and [`Self::cell_size`] reports the new metrics immediately so the
    /// caller can regrid before the next frame. See
    /// `GlyphRasterizer::set_font_size` for what is kept and why this is
    /// much cheaper than [`Self::with_font_ex`].
    ///
    /// Before any font is loaded there is nothing to resize, so this falls
    /// back to [`Self::with_font_ex`] with the default family.
    pub fn set_font_size(&mut self, font_size_px: f32, line_height: f32) {
        let Some(text) = self.text.as_mut() else {
            self.with_font_ex(None, font_size_px, line_height);
            return;
        };
        text.rasterizer.set_font_size(font_size_px, line_height);
        // Cached rows hold atlas slots from the old packer: re-shape all.
        text.rows.invalidate();
        self.needs_full_clear = true;
    }

    /// True iff the family name passed to the most recent
    /// [`Self::with_font_ex`] call resolved to a real face in the
    /// loaded font database. `false` means cosmic-text fell back to
    /// the host default; the binary should log a warning so the user
    /// notices their `font.family` didn't take effect.
    #[must_use]
    pub fn font_family_available(&self) -> bool {
        self.text
            .as_ref()
            .is_none_or(|t| t.rasterizer.requested_family_available())
    }

    /// Family name as requested by the most recent
    /// [`Self::with_font_ex`] call (or `"monospace"` if `None` was
    /// passed). Useful for log messages.
    #[must_use]
    pub fn font_family_name(&self) -> Option<&str> {
        self.text.as_ref().map(|t| t.rasterizer.family_name())
    }

    /// Current cell size in pixels (width, height). Returns `(0, 0)` if
    /// `with_font` hasn't been called.
    #[must_use]
    pub fn cell_size(&self) -> (f32, f32) {
        self.text
            .as_ref()
            .map_or((0.0, 0.0), |t| t.rasterizer.cell_size())
    }

    /// Set the theme used by [`Renderer::render_term`]. Also syncs the
    /// clear color to the theme background so cells with the default bg
    /// (which emit no instance) show through with the right color.
    ///
    /// Theme changes invalidate the framebuffer (background color, all
    /// foreground colors); the next frame uses `LoadOp::Clear`.
    pub fn set_theme(&mut self, theme: Theme) {
        self.base_theme = theme;
        self.apply_effective_theme();
    }

    /// Derive `theme` from `base_theme` + DECSCNM and resync the clear
    /// color.
    fn apply_effective_theme(&mut self) {
        self.theme = if self.reverse_video {
            reverse_video_theme(self.base_theme)
        } else {
            self.base_theme
        };
        self.clear_color = self.theme.bg;
        self.needs_full_clear = true;
    }

    /// Force the next frame to use `LoadOp::Clear` instead of
    /// `LoadOp::Load`. Called by the binary on resize / theme / font
    /// swap — any event that invalidates the GPU framebuffer's prior
    /// contents.
    pub fn invalidate_framebuffer(&mut self) {
        self.needs_full_clear = true;
    }

    /// Override the cursor blink half-cycle. Used by the config layer
    /// to thread the `[cursor]` rate through to the runtime.
    pub fn set_cursor_blink_interval(&mut self, d: Duration) {
        self.blink.interval = d;
    }

    /// Enable or disable SGR 5 / 6 blinking (`[text] blink`). When
    /// disabled, blinking text is drawn steadily. Forces a full repaint
    /// so text caught in the off-phase reappears.
    pub fn set_text_blink(&mut self, enabled: bool) {
        if self.text_blink.set_enabled(enabled) {
            self.needs_full_clear = true;
        }
    }

    /// True iff blinking text is currently in its visible phase (always
    /// true when blink is disabled or nothing on screen blinks).
    #[must_use]
    pub fn text_blink_visible(&self) -> bool {
        self.text_blink.blink_on()
    }

    /// Current cursor blink half-cycle.
    #[must_use]
    pub fn cursor_blink_interval(&self) -> Duration {
        self.blink.interval
    }

    /// True iff the cursor is currently in the "on" phase of the blink
    /// cycle (or the term has blink disabled, in which case the cursor
    /// is always visible).
    #[must_use]
    pub fn cursor_visible(&self) -> bool {
        self.blink.visible
    }

    /// Time until the cursor's next blink toggle, given `term`'s blink
    /// flag. Returns `None` when the term has blink disabled (DECSCUSR
    /// Ps=2/4/6 → steady cursor) so the binary doesn't pointlessly
    /// schedule a wake-up.
    #[must_use]
    pub fn next_redraw_deadline(&self, term: &Term) -> Option<Duration> {
        let blink = self
            .blink
            .next_deadline(term.cursor_blink(), Instant::now());
        let rgp = term.rgp_scene().animation_deadline();
        // Scroll-chip fade: tick at ~60 Hz while it is in progress.
        let chip = self
            .chip_fade
            .animating(Instant::now())
            .then_some(Duration::from_millis(16));
        // SGR blink: only while blinking cells were on screen last frame.
        let text_blink = self.text_blink.next_deadline(Instant::now());
        // Deferred image mip work: finish it over the next frames.
        let mips = self.mip_work_pending.then_some(Duration::from_millis(16));
        // Whichever fires first wins. All are `Option<Duration>`.
        [blink, rgp, chip, text_blink, mips]
            .into_iter()
            .flatten()
            .min()
    }

    /// Set (or clear) a debug overlay string painted at the top-right
    /// corner each frame. Used by the binary's `TOASTTY_DEBUG` FPS
    /// counter. While `Some`, the renderer skips its damage-only
    /// short-circuit so the overlay text refreshes even on an idle grid.
    ///
    /// Takes `Option<&str>` and copies into a renderer-owned `String`,
    /// reusing the existing allocation across frames — callers no
    /// longer need to allocate a new `String` per FPS-counter update.
    pub fn set_debug_overlay(&mut self, text: Option<&str>) {
        match text {
            Some(s) => {
                if let Some(buf) = self.debug_overlay.as_mut() {
                    buf.clear();
                    buf.push_str(s);
                } else {
                    self.debug_overlay = Some(s.to_owned());
                }
            }
            None => self.debug_overlay = None,
        }
    }

    /// True when a debug overlay is currently set.
    #[must_use]
    pub fn has_debug_overlay(&self) -> bool {
        self.debug_overlay.is_some()
    }

    /// Set (or clear) a multi-line full-width error banner painted at
    /// the top of the surface. Lines are split on `\n`. While `Some`,
    /// the renderer skips its damage-only short-circuit so the banner
    /// remains visible on an idle grid.
    pub fn set_error_banner(&mut self, text: Option<&str>) {
        match text {
            Some(s) => {
                if let Some(buf) = self.error_banner.as_mut() {
                    buf.clear();
                    buf.push_str(s);
                } else {
                    self.error_banner = Some(s.to_owned());
                }
                self.needs_full_clear = true;
            }
            None => {
                if self.error_banner.is_some() {
                    self.error_banner = None;
                    self.needs_full_clear = true;
                }
            }
        }
    }

    /// True when an error banner is currently set.
    #[must_use]
    pub fn has_error_banner(&self) -> bool {
        self.error_banner.is_some()
    }

    /// Show (`Some`) or dismiss (`None`) the close-confirmation dialog.
    /// The copy is derived from `prompt` ([`dialog::prompt_copy`]). While
    /// shown, the renderer skips its damage-only short-circuit so the
    /// dialog stays up on an idle grid.
    pub fn set_close_prompt(&mut self, prompt: Option<ClosePrompt>) {
        if prompt != self.close_prompt {
            self.needs_full_clear = true;
        }
        if prompt.is_none() {
            self.close_dialog_hover = None;
        }
        self.close_prompt = prompt;
    }

    /// True when a close-confirmation prompt is currently set.
    #[must_use]
    pub fn has_close_prompt(&self) -> bool {
        self.close_prompt.is_some()
    }

    /// Where the close dialog goes this frame, if shown.
    fn close_dialog_layout(&self) -> Option<dialog::DialogLayout> {
        let prompt = self.close_prompt.as_ref()?;
        let cell = self.cell_size();
        #[allow(clippy::cast_precision_loss)]
        let metrics = dialog::DialogMetrics {
            surface: (self.config.width as f32, self.config.height as f32),
            content_origin: [self.pad_left as f32, self.pad_top as f32],
            content: self.content_dims(cell),
            cell,
            scale: self.ui_scale,
        };
        dialog::dialog_layout(&dialog::prompt_copy(prompt), &metrics)
    }

    /// The close-dialog button under `(x, y)` (physical px, the same
    /// space as `scroll_button_rect`), if the dialog is shown. The
    /// renderer owns the layout, so hit-tests always match what's drawn.
    #[must_use]
    pub fn close_dialog_button_at(&self, x: f32, y: f32) -> Option<DialogButton> {
        self.close_dialog_layout()?.button_at(x, y)
    }

    /// Update which close-dialog button the pointer hovers. Returns
    /// `true` when that changed (so the caller should redraw).
    pub fn set_close_dialog_hover(&mut self, hovered: Option<DialogButton>) -> bool {
        let hovered = hovered.filter(|_| self.close_prompt.is_some());
        if hovered == self.close_dialog_hover {
            return false;
        }
        self.close_dialog_hover = hovered;
        self.needs_full_clear = true;
        true
    }

    /// Enable (`Some(corner)`) or disable (`None`) the scroll-to-bottom
    /// button. When enabled, the button is painted in `corner` on any
    /// frame where the view is scrolled back into the scrollback. Cheap to
    /// call every config reload; it just stores the corner.
    pub fn set_scroll_button(&mut self, corner: Option<ScrollButtonCorner>) {
        if corner != self.scroll_button {
            self.needs_full_clear = true;
        }
        self.scroll_button = corner;
        if corner.is_none() {
            self.chip_fade.snap(false);
            self.chip_hover = false;
        }
    }

    /// Set the window scale factor (physical px per logical px) used to
    /// size pixel-based UI such as the scroll-to-bottom chip.
    pub fn set_ui_scale(&mut self, scale: f64) {
        #[allow(clippy::cast_possible_truncation)]
        let scale = scale as f32;
        if scale.is_finite() && scale > 0.0 && (scale - self.ui_scale).abs() > f32::EPSILON {
            self.ui_scale = scale;
            self.needs_full_clear = true;
        }
    }

    /// Update whether the pointer hovers the scroll-to-bottom chip.
    /// Returns `true` when that changed (so the caller should redraw).
    pub fn set_scroll_button_hover(&mut self, hovered: bool) -> bool {
        if hovered == self.chip_hover {
            return false;
        }
        self.chip_hover = hovered;
        self.needs_full_clear = true;
        true
    }

    /// Floor-divide leftover and per-axis leading-edge offset for the
    /// current content area, cell size, and [`GridAlign`].
    ///
    /// Returns `(rem, lead)` where `rem = [rem_w, rem_h]` is the sub-cell
    /// sliver the floor-divided grid leaves uncovered, and `lead =
    /// [lead_x, lead_y]` is how far the grid's leading (left/top) edge is
    /// pushed in from the content origin: `0` for `TopLeft`, `rem/2` for
    /// `Centered`. The trailing edge gets `rem - lead`.
    ///
    /// Cell-size-derived (no `Term` needed) so the mouse hit-test path can
    /// call it; `cols/rows = floor(content / cell)` matches the grid the
    /// binary sizes via `grid_dims_from_pixels`.
    #[must_use]
    fn grid_overflow(&self, cell: (f32, f32)) -> ([f32; 2], [f32; 2]) {
        grid_overflow_px(
            self.content_dims(cell),
            cell,
            self.grid_align.leading_fraction(),
        )
    }

    /// Content origin in physical px fed into every pipeline's
    /// `content_origin` uniform. `(pad_left, pad_top)` plus the
    /// [`GridAlign`] leading offset (zero unless the grid is centered).
    /// Internal `[f32; 2]` form.
    #[must_use]
    fn content_origin(&self) -> [f32; 2] {
        let (_, lead) = self.grid_overflow(self.cell_size());
        #[allow(clippy::cast_precision_loss)]
        {
            [
                self.pad_left as f32 + lead[0],
                self.pad_top as f32 + lead[1],
            ]
        }
    }

    /// Public content-origin getter (physical px) for the mouse hit-test
    /// path. Single source of truth shared by rendering and hit-testing —
    /// the binary reads it back rather than re-deriving the origin (so a
    /// centered grid's offset is honored by hit-testing too).
    #[must_use]
    pub fn content_origin_px(&self) -> (f32, f32) {
        let o = self.content_origin();
        (o[0], o[1])
    }

    /// Content (grid) pixel dims = the full surface (`config.width`/
    /// `config.height`, which always stay full-surface — see the
    /// surface-size contract) minus the stored physical pads. Clamped so a
    /// huge padding still leaves at least one cell. `cell` is the cell
    /// pixel size `(w, h)`. Derived on demand; never cached.
    #[must_use]
    fn content_dims(&self, cell: (f32, f32)) -> (f32, f32) {
        #[allow(clippy::cast_precision_loss)]
        {
            let cw = (self.config.width as f32 - (self.pad_left + self.pad_right) as f32)
                .max(cell.0.max(1.0));
            let ch = (self.config.height as f32 - (self.pad_top + self.pad_bottom) as f32)
                .max(cell.1.max(1.0));
            (cw, ch)
        }
    }

    /// Set the window-padding insets in **physical px** (`top, right,
    /// bottom, left` order — matches `PaddingConfig` / `EdgeBleed::pad`).
    /// Forces a full clear only on an actual change (so a config reload
    /// re-push of unchanged padding does not spuriously repaint).
    pub fn set_padding(&mut self, top: u32, right: u32, bottom: u32, left: u32) {
        if (self.pad_top, self.pad_right, self.pad_bottom, self.pad_left)
            != (top, right, bottom, left)
        {
            self.pad_top = top;
            self.pad_right = right;
            self.pad_bottom = bottom;
            self.pad_left = left;
            self.needs_full_clear = true;
        }
    }

    /// Set the global edge-cell background-extension gate
    /// (`extend_background_when`). Forces a full clear only on an actual
    /// change.
    pub fn set_extend_background_when(&mut self, when: ExtendBackgroundWhen) {
        if self.extend_background_when != when {
            self.extend_background_when = when;
            self.needs_full_clear = true;
        }
    }

    /// Set the per-axis edge-cell background-extension rule. Forces a full
    /// clear only on an actual change.
    pub fn set_extend_background(&mut self, mode: ExtendBackground) {
        if self.extend_background != mode {
            self.extend_background = mode;
            self.needs_full_clear = true;
        }
    }

    /// Set how the cell grid is aligned within the content area. Forces a
    /// full clear only on an actual change (the origin shift moves every
    /// cell, so a partial redraw can't overpaint the old positions).
    pub fn set_grid_align(&mut self, align: GridAlign) {
        if self.grid_align != align {
            self.grid_align = align;
            self.needs_full_clear = true;
        }
    }

    /// Append the instances for a full-width, multi-line overlay banner
    /// to `instances`, bottom-anchored to the viewport. Used by the config
    /// error banner. `width`/`height` are
    /// the viewport's physical pixel dimensions (`self.config.{width,height}`).
    ///
    /// We emit two cover quads per banner cell because the cell pipeline
    /// draws in two passes (bg + glyph) and the term's own glyphs land in
    /// the glyph pass too — without an opaque glyph-pass cover, the cells
    /// underneath bleed through and mix into the banner text. So per row
    /// we push:
    ///   1. `FLAG_NO_GLYPH` bg quad — bg pass: paints the banner color
    ///      where the term bg used to be.
    ///   2. `FLAG_UNDERLINE` cover quad — glyph pass: re-paints the banner
    ///      color over any term glyphs that would otherwise render on top.
    ///   3. The banner's own glyph quads — glyph pass: paint after the
    ///      cover, so they land cleanly.
    ///
    /// Taken as an associated function (not `&mut self`) so it can borrow
    /// `text` independently of `self`'s other fields at the call site.
    #[allow(clippy::too_many_arguments)]
    fn draw_banner(
        queue: &Queue,
        width: u32,
        height: u32,
        instances: &mut Vec<CellInstance>,
        text: &mut TextState,
        banner: &str,
        bg: [f32; 4],
        fg: [f32; 4],
        cell_size: (f32, f32),
        term: &Term,
    ) {
        #[allow(clippy::cast_precision_loss)]
        let viewport_w = width as f32;
        #[allow(clippy::cast_precision_loss)]
        let viewport_h = height as f32;
        let cell_w = cell_size.0;
        let cell_h = cell_size.1;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let cols = (viewport_w / cell_w).floor().max(1.0) as u32;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let num_lines = banner.lines().count().max(1) as u32;
        // Bottom-anchored: viewport height minus the banner's total line
        // height (clamped non-negative).
        #[allow(clippy::cast_precision_loss)]
        let banner_top_y = (viewport_h - (num_lines as f32) * cell_h).max(0.0);
        for (row_idx, line) in banner.lines().enumerate() {
            #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
            let y0 = banner_top_y + (row_idx as f32) * cell_h;
            // bg-pass + glyph-pass cover, one of each per column.
            for col in 0..cols {
                #[allow(clippy::cast_precision_loss)]
                let x0 = (col as f32) * cell_w;
                // bg pass cover.
                instances.push(CellInstance {
                    pos: [x0, y0],
                    size: [cell_w, cell_h],
                    uv_min: [0.0, 0.0],
                    uv_max: [0.0, 0.0],
                    fg,
                    bg,
                    flags: crate::text::instance::FLAG_NO_GLYPH,
                    pad: [0; 3],
                });
                // glyph pass cover. FLAG_UNDERLINE in fs_glyph
                // emits a solid premultiplied `in.bg`, so we
                // overpaint any term glyphs the dirty builder
                // emitted for these rows.
                instances.push(CellInstance {
                    pos: [x0, y0],
                    size: [cell_w, cell_h],
                    uv_min: [0.0, 0.0],
                    uv_max: [0.0, 0.0],
                    fg,
                    bg,
                    flags: crate::text::instance::FLAG_UNDERLINE
                        | crate::text::instance::FLAG_NO_GLYPH,
                    pad: [0; 3],
                });
            }
            if line.is_empty() {
                continue;
            }
            let lg = text
                .rasterizer
                .shape_line(queue, line, term.grapheme_cluster_mode());
            for (i, ch) in line.chars().enumerate() {
                #[allow(clippy::cast_possible_truncation)]
                let col = i as u32;
                if col >= cols {
                    break;
                }
                if ch.is_whitespace() {
                    continue;
                }
                #[allow(clippy::cast_precision_loss)]
                let pos = [(col as f32) * cell_w, y0];
                #[allow(clippy::cast_possible_truncation)]
                let col_u16 = col as u16;
                if let Some(slot) = lg.get(col_u16, ch) {
                    let flags = if slot.is_color {
                        crate::text::instance::FLAG_COLOR_GLYPH
                    } else {
                        0
                    };
                    instances.push(CellInstance {
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
    }

    /// Rich blocks: sync GPU textures for the blocks on screen and
    /// return `(render_row, draw)` for every rendered row that will show
    /// an image slice (block image resident and the row's slice present).
    fn sync_rich_blocks(&mut self, term: &Term) -> Vec<(u16, crate::image::rich::RichRowDraw)> {
        if term.rich_blocks().is_empty() && self.rich_tex_cache.is_empty() {
            return Vec::new();
        }
        let cell = self.cell_size();
        if cell.1 < 1.0 {
            return Vec::new();
        }
        let (rows, _) = term.size();
        let content_h = self.content_dims(cell).1;
        let view_pixel = term.view_offset_pixel();
        let rows_rendered =
            crate::text::instance::rows_to_render(rows, view_pixel, cell.1, content_h);
        let y_translate = if view_pixel > 0.0 {
            view_pixel - cell.1
        } else {
            0.0
        };
        let mut tagged: Vec<(u16, toastty_term::RichRow)> = Vec::new();
        for r in 0..rows_rendered {
            if let Some(tag) = term.view_row(r).rich {
                tagged.push((r, tag));
            }
        }
        let visible: Vec<toastty_term::RichRow> = tagged.iter().map(|(_, t)| *t).collect();
        let Some(pipe) = self.image_pipeline.as_mut() else {
            return Vec::new();
        };
        let max_dim = self.device.limits().max_texture_dimension_2d;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let cell_h_px = cell.1.round() as u32;
        let changed = self.rich_tex_cache.sync(
            &self.device,
            &self.queue,
            pipe,
            term,
            &visible,
            cell_h_px,
            max_dim,
        );
        if changed {
            self.needs_full_clear = true;
        }
        tagged
            .into_iter()
            .filter(|(_, tag)| {
                self.rich_tex_cache
                    .get(tag.block)
                    .is_some_and(|e| crate::image::rich::row_slice(e, tag.index).is_some())
            })
            .map(|(r, tag)| {
                (
                    r,
                    crate::image::rich::RichRowDraw {
                        y: f32::from(r) * cell.1 + y_translate,
                        block: tag.block,
                        index: tag.index,
                    },
                )
            })
            .collect()
    }

    /// Selection highlight over rich-block slices: a translucent quad
    /// per selected run of cells, drawn right after the slices so a
    /// selection stays visible over opaque parts of the image.
    fn push_rich_selection_tint(
        &mut self,
        term: &Term,
        draws: &[(u16, crate::image::rich::RichRowDraw)],
        cell: (f32, f32),
    ) {
        if term.selection().is_none() {
            return;
        }
        let Some(pipe) = self.image_pipeline.as_mut() else {
            return;
        };
        let (rows, cols) = term.size();
        let pixel_extra = u16::from(term.view_offset_pixel() > 0.0);
        let sel = self.theme.selection_bg;
        let rgba = [
            crate::color::linear_to_srgb_u8(sel[0]),
            crate::color::linear_to_srgb_u8(sel[1]),
            crate::color::linear_to_srgb_u8(sel[2]),
            110,
        ];
        let mut tint: Option<usize> = None;
        for (r, d) in draws {
            let Some(line_id) = crate::text::instance::line_id_for_render_row(
                term.bottom_id(),
                rows,
                term.view_offset_lines(),
                pixel_extra,
                *r,
            ) else {
                continue;
            };
            let mut c = 0u16;
            while c < cols {
                if !term.is_cell_selected(line_id, c) {
                    c += 1;
                    continue;
                }
                let start = c;
                while c < cols && term.is_cell_selected(line_id, c) {
                    c += 1;
                }
                let idx = *tint.get_or_insert_with(|| {
                    self.rich_tex_cache
                        .tint_texture(&self.device, &self.queue, pipe, rgba)
                });
                #[allow(clippy::cast_possible_truncation)]
                self.rich_instances.push(ImageInstance {
                    pos: [f32::from(start) * cell.0, d.y],
                    size: [f32::from(c - start) * cell.0, cell.1],
                    uv_min: [0.0, 0.0],
                    uv_max: [1.0, 1.0],
                    texture_index: idx as u32,
                    z: -1,
                    image_id: 0,
                    key: 0,
                });
            }
        }
    }

    /// Where the scroll-to-bottom chip sits this frame, if enabled (the
    /// caller decides visibility). Physical px, content origin applied.
    fn scroll_chip_geometry(&self) -> Option<chip::ChipGeometry> {
        let corner = self.scroll_button?;
        let dims = self.content_dims(self.cell_size());
        chip::chip_geometry(self.content_origin(), dims, self.ui_scale, corner)
    }

    /// Show the rich block "Copy Markdown" chip for a block whose first
    /// visible row is view row `top_row` (`None` hides it); `hovered`
    /// brightens it. Returns `true` when anything changed (redraw).
    pub fn set_rich_chip(&mut self, top_row: Option<u16>, hovered: bool) -> bool {
        let hovered = hovered && top_row.is_some();
        if top_row == self.rich_chip && hovered == self.rich_chip_hover {
            return false;
        }
        self.rich_chip = top_row;
        self.rich_chip_hover = hovered;
        self.needs_full_clear = true;
        true
    }

    fn rich_chip_geometry(&self, term: &Term) -> Option<chip::ChipGeometry> {
        let row = self.rich_chip?;
        let cell = self.cell_size();
        let view_pixel = term.view_offset_pixel();
        let y_translate = if view_pixel > 0.0 {
            view_pixel - cell.1
        } else {
            0.0
        };
        let top = f32::from(row) * cell.1 + y_translate;
        let dims = self.content_dims(cell);
        chip::copy_chip_geometry(self.content_origin(), dims, self.ui_scale, top)
    }

    /// Hit rect `[x0, y0, x1, y1]` (physical px) of the "Copy Markdown"
    /// chip, when shown. See [`Self::set_rich_chip`].
    #[must_use]
    pub fn rich_chip_rect(&self, term: &Term) -> Option<[f32; 4]> {
        self.rich_chip_geometry(term).map(|g| g.hit_rect)
    }

    /// Pixel-space hit rect `[x0, y0, x1, y1]` of the scroll-to-bottom
    /// chip when it is currently visible — i.e. enabled via
    /// [`Self::set_scroll_button`] AND `term` is scrolled back into the
    /// scrollback. `None` otherwise. Physical px (content origin baked
    /// in), slightly larger than the drawn disc. The binary hit-tests
    /// clicks and hover against it.
    #[must_use]
    pub fn scroll_button_rect(&self, term: &Term) -> Option<[f32; 4]> {
        if !term.is_view_scrolled_back() {
            return None;
        }
        self.scroll_chip_geometry().map(|g| g.hit_rect)
    }

    /// Set (or clear) the IME preedit string drawn inline at the cursor.
    /// `None` or an empty string clears it. The string is the in-progress
    /// composition text from the platform IME; it is NOT part of the terminal
    /// grid and is drawn as an overlay each frame until cleared/committed.
    ///
    /// Copies into a renderer-owned `String`. An empty string normalizes
    /// to `None` (via [`normalize_preedit`]) so `has_preedit` and the
    /// skip-submit bypass both agree there's nothing to draw. Unlike the
    /// FPS-counter overlay this isn't a hot path — preedit changes at
    /// keystroke rate — so we don't bother reusing the allocation.
    pub fn set_preedit(&mut self, text: Option<&str>) {
        self.preedit = normalize_preedit(text);
    }

    /// True when a non-empty preedit overlay is currently set.
    #[must_use]
    pub fn has_preedit(&self) -> bool {
        self.preedit.is_some()
    }

    /// Current theme.
    pub fn theme(&self) -> Theme {
        self.base_theme
    }

    /// Resize the surface. Width or height of 0 is clamped to 1 (configuring
    /// a zero-sized surface panics in wgpu).
    ///
    /// The new surface back-buffer has undefined contents — the next
    /// frame must use `LoadOp::Clear`.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        self.surface.configure(&self.device, &self.config);
        // Scratch must match the swapchain dims; recreate. Its contents
        // become undefined → force a full clear on the next frame.
        let (tex, view) = create_scratch(
            &self.device,
            self.config.width,
            self.config.height,
            self.config.format,
        );
        self.scratch_texture = tex;
        self.scratch_view = view;
        // Depth attachment shares dims with the color scratch; recreate
        // it for the same reason.
        let (dtex, dview) =
            create_scratch_depth(&self.device, self.config.width, self.config.height);
        self.scratch_depth_texture = dtex;
        self.scratch_depth_view = dview;
        self.needs_full_clear = true;
    }

    /// Set the clear color (linear RGBA, `[0, 1]` range).
    pub fn set_clear_color(&mut self, rgba: [f32; 4]) {
        self.clear_color = rgba;
    }

    /// Current clear color (linear RGBA).
    #[must_use]
    pub fn clear_color(&self) -> [f32; 4] {
        self.clear_color
    }

    /// Current surface format. Mostly here for the demo / tests.
    #[must_use]
    pub fn format(&self) -> TextureFormat {
        self.config.format
    }

    /// Current configured pixel size.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    /// Render `term` to the surface: clear → text/cell pass.
    ///
    /// [`Renderer::with_font`] must be called first; returns
    /// [`RenderError::FontNotConfigured`] otherwise.
    ///
    /// Honors `term.damage()` for the row-shape cache (decision §7,
    /// minimum-viable subset for M5; full skip-submit / cell-level
    /// damage lands in M9).
    ///
    /// Setting `TOASTTY_TRACE_RENDER=1` in the environment makes this
    /// function emit a per-phase timing line via `tracing::info!`. Use it
    /// to attribute per-frame cost in real workloads (the criterion
    /// bench under `benches/render_term.rs` runs in release mode; this
    /// env var works in the debug build too).
    #[allow(clippy::too_many_lines)] // optional tracing branches add ~30 LoC
    pub fn render_term(&mut self, term: &mut Term) -> Result<RenderOutcome, RenderError> {
        if self.text.is_none() {
            return Err(RenderError::FontNotConfigured);
        }
        // DECSCNM (`CSI ? 5 h`): swap the default fg/bg. The term marks
        // every row dirty when the mode flips.
        if term.reverse_video() != self.reverse_video {
            self.reverse_video = term.reverse_video();
            self.apply_effective_theme();
        }

        // DECSET 2026 (synchronized output) gate. The dispatcher flips
        // `pause_rendering` on BSU; until ESU (or the watchdog
        // timeout), we skip the frame entirely — no shaping, no submit,
        // no surface acquire. The watchdog lives in the binary
        // (`Toastty::handle_pty_bytes` and `Event::Redraw`) and calls
        // `Term::force_flush_sync_output` after 1 s, which both clears
        // the pause and marks every row dirty so the next frame is a
        // corrective full redraw. Returning `Skipped` here is how the
        // binary knows NOT to clear the dirty bitset or the BSU
        // force-flushed flag — both must survive across the skip
        // (followup C2).
        if term.pause_rendering() {
            return Ok(RenderOutcome::Skipped);
        }

        // Rebuild the cached extended palette if the term's revision
        // has advanced since the last rebuild. `Term::set_palette_override`
        // also `mark_all_dirty`s, so the cached palette is consumed by
        // every cell on the very next frame.
        if term.palette_revision() != self.palette_revision_seen {
            self.rebuild_ext_palette(term);
        }

        // M11a: image-content sync. If the term's image revision has
        // advanced, drop textures of deleted / replaced images and force
        // a full clear for this frame (the pragmatic shortcut from the
        // milestone plan — partial redraw with images is not supported
        // in M11a). Uploads happen on demand when instances are built.
        if term.image_revision() != self.image_revision_seen
            && let Some(image_pipeline) = self.image_pipeline.as_mut()
        {
            image_pipeline.sync_registry(term.image_registry(), &mut self.image_tex_cache);
            self.image_revision_seen = term.image_revision();
            self.needs_full_clear = true;
            // Mark every row dirty so the partial-redraw path falls back
            // to a full re-emission. The full-clear flag alone is enough
            // for the bg pass, but the M9 dirty-instance builder gates
            // on per-row damage too.
            term.mark_all_dirty();
        }

        // Detect a viewport-offset change since the last render. The
        // renderer's partial-redraw path can't reliably overpaint the
        // previous frame at a new y-translation (blank cells skip
        // emission), so we force `LoadOp::Clear` whenever the offset
        // moves. Term::advance_viewport already calls `mark_all_dirty`
        // so the full-build path runs; this just gets the LoadOp right.
        let cur_view = (term.view_offset_lines(), term.view_offset_pixel());
        if cur_view != self.last_view_offset {
            self.needs_full_clear = true;
            self.last_view_offset = cur_view;
        }

        // M12d: RGP scene sync. Same pattern as the image registry, but
        // split across two revision counters so a transform-only `u`
        // (rotation/scale/color — all per-draw uniforms recomputed in
        // pipeline.render()) repaints WITHOUT re-uploading geometry.
        //
        // Asset table changed (register / delete) → re-upload GPU meshes.
        if term.rgp_asset_revision() != self.rgp_asset_revision_seen {
            self.rgp_cache.sync(&self.device, term.rgp_scene());
            self.rgp_asset_revision_seen = term.rgp_asset_revision();
        }
        // Any RGP scene change (placement / style / asset) → repaint the
        // 3D layer and re-emit cells so the new pose is composited.
        if term.rgp_revision() != self.rgp_revision_seen {
            self.rgp_revision_seen = term.rgp_revision();
            self.needs_full_clear = true;
            term.mark_all_dirty();
        }

        // Rich blocks: upload / refresh the textures of the blocks on
        // screen and collect the rows that draw an image slice.
        let rich_draws = self.sync_rich_blocks(term);
        let rich_active = !rich_draws.is_empty();
        if rich_active != self.rich_drawn_last {
            self.needs_full_clear = true;
        }
        // Rows whose text is suppressed for a rich slice. Left empty
        // (no allocation) on the common frame without rich blocks.
        let mut rich_suppress = Vec::new();
        if rich_active {
            rich_suppress.resize(usize::from(term.size().0) + 2, false);
        }
        for d in &rich_draws {
            if let Some(slot) = rich_suppress.get_mut(usize::from(d.0)) {
                *slot = true;
            }
        }

        // M9 skip-submit: if no cells changed AND no animation tick is
        // due, there's nothing to draw. Skip the surface acquire +
        // encode + submit entirely. The binary preserves the damage
        // signal across skipped frames (followup C2).
        //
        // Cursor blink animation: when a tick is due AND the term has
        // blink enabled, force the frame through so the renderer can
        // toggle `cursor_visible` and emit the updated cursor instance.
        let now = Instant::now();
        // Followup I1: detect the steady→blinking edge BEFORE computing
        // animation_due so the freshly-stamped `last_at` doesn't
        // immediately fire a tick (which would flash the cursor).
        self.blink.sync_enabled(term.cursor_blink(), now);
        let cursor_animation_due = self.blink.animation_due(term.cursor_blink(), now);
        // SGR 5 / 6 text blink: same gate, but only armed while blinking
        // cells were visible on the previous frame (`TextBlink::observe`).
        let text_blink_due = self.text_blink.due(now);
        // M12c: RGP animations force frames through the skip-submit
        // gate the same way cursor blink does. Tick BEFORE the
        // skip check so animation_phase_rad is current for this
        // frame; the tick itself doesn't bump revision.
        let rgp_animation_active = term.rgp_scene().has_active_animations();
        // SIZE-12: a frame that finishes deferred mip levels redraws the
        // images, so repaint everything under them (like RGP animation).
        if self.mip_work_pending {
            self.needs_full_clear = true;
            term.mark_all_dirty();
        }
        if rgp_animation_active {
            term.tick_rgp_animations(now);
            // The previous frame's 3D pose still sits in the scratch
            // colour + depth attachments (both LoadOp::Load by default
            // for partial-redraw). Animating placements can sweep
            // through pixels outside any cell's damage set, so marking
            // dirty cells doesn't overpaint the trail — force a full
            // clear, same as when the RGP revision bumps.
            self.needs_full_clear = true;
            term.mark_all_dirty();
        }
        // Scroll-to-bottom chip: fade toward visible while scrolled back
        // (hidden behind the close dialog). A fade step or a visibility
        // edge forces the frame through the skip gate.
        let chip_target = self.scroll_button.is_some()
            && self.close_prompt.is_none()
            && term.is_view_scrolled_back();
        self.chip_fade.set_visible(chip_target, now);
        let chip_alpha = self.chip_fade.value(now);
        let chip_changed =
            self.chip_fade.animating(now) || (chip_alpha > 0.0) != self.chip_drawn_last;
        if term.damage().is_empty()
            && !cursor_animation_due
            && !text_blink_due
            && !rgp_animation_active
            && !self.mip_work_pending
            && !chip_changed
            && !self.needs_full_clear
            && self.debug_overlay.is_none()
            && self.error_banner.is_none()
            && self.close_prompt.is_none()
            && self.preedit.is_none()
        {
            return Ok(RenderOutcome::Skipped);
        }

        // Rich slices are alpha-blended over the cell backgrounds, so a
        // partial (LoadOp::Load) frame would stack them on last frame's
        // pixels. Repaint everything while blocks are (or just were) shown.
        if rich_active || self.rich_drawn_last {
            self.needs_full_clear = true;
            term.mark_all_dirty();
        }
        if self.rich_chip.is_some() {
            self.needs_full_clear = true;
            term.mark_all_dirty();
        }
        self.rich_drawn_last = rich_active;

        // SIZE-9: `z < INT32_MIN/2` images draw before the bg pass, and
        // that frame skips the default-bg quads so they show through
        // default-bg cells. A partial (LoadOp::Load) frame would leave
        // stale pixels in the skipped cells, so repaint everything while
        // such a placement is (or just was) on screen.
        let below_bg_active =
            has_below_cell_bg_placements(term.image_grid(), term.image_registry());
        if below_bg_forces_full_redraw(below_bg_active, &mut self.below_bg_drawn_last) {
            self.needs_full_clear = true;
            term.mark_all_dirty();
        }

        // The chip is translucent: compositing it over a LoadOp::Load
        // partial frame would stack on last frame's chip. While it is
        // (or just was) on screen, repaint everything underneath.
        if chip_alpha > 0.0 || self.chip_drawn_last {
            self.needs_full_clear = true;
            term.mark_all_dirty();
        }
        self.chip_drawn_last = chip_alpha > 0.0;

        // The close dialog's scrim is translucent and covers the whole
        // window: repaint everything under it each frame it's up (its
        // removal already forces a full clear in `set_close_prompt`).
        if self.close_prompt.is_some() {
            self.needs_full_clear = true;
            term.mark_all_dirty();
        }

        // If the blink tick fired, flip the visibility flag now —
        // before instance building reads `cursor_visible`.
        if cursor_animation_due {
            self.blink.tick(now);
            // Followup C1: under partial-redraw (LoadOp::Load) the
            // dirty-instance builder only emits bg quads for cells in
            // the damage set. When the blink toggles
            // visible→invisible, no other code path marks the cursor's
            // cell dirty, so the previous frame's cursor block ghosts.
            // Mark the cursor's current cell here so the builder emits
            // a fresh bg quad (which overpaints the ghost) and, on the
            // visible→visible→... cycle, re-emits the cursor on top.
            //
            // `mark_cell_dirty` handles the width-2 continuation case
            // (marks col - 1 too so the multi-cell glyph is re-emitted).
            let cur = term.cursor();
            term.mark_cell_dirty(cur.row, cur.col);
        }
        // Text blink tick: flip the phase and mark every blinking cell
        // dirty so the partial-redraw builder re-emits (on-phase) or
        // drops (off-phase) its glyph and decorations over a fresh bg
        // quad. `row_blink` is from the previous frame's shaping, which
        // is exactly what's on screen.
        if text_blink_due {
            self.text_blink.tick(now);
            if let Some(text) = self.text.as_ref() {
                mark_blinking_cells_dirty(term, text.rows.row_blink());
            }
        }
        // If the term has blink disabled, the cursor must always be
        // visible. We don't update `last_at` because the blink state
        // is unobservable while disabled, and any future re-enable
        // should compute the next tick from the moment of re-enable,
        // not from a stale baseline. Steady cursor:
        if !term.cursor_blink() && !self.blink.visible {
            self.blink.force_visible();
        }

        let trace = self.trace_render;
        let t_total = if trace {
            Some(std::time::Instant::now())
        } else {
            None
        };

        let (rows, _) = term.size();
        // Content (grid) pixel dims drive both how many rows we shape +
        // render (the trailing partial bottom row) and the edge-bleed
        // extent. Computed before the `&mut self.text` borrow below so the
        // `&self` method calls don't conflict.
        let content_dims = self.content_dims(self.cell_size());
        let cell_size;
        let atlas_dims;
        let any_blink;
        {
            let text = self.text.as_mut().expect("text initialised above");
            cell_size = text.rasterizer.cell_size();
            atlas_dims = text.rasterizer.atlas_dims();

            // Number of rows to render this frame: content rows, plus the
            // partial top row (sub-row pixel offset) and the trailing
            // partial bottom row (floor-divided grid leftover). See
            // [`crate::text::instance::rows_to_render`].
            let rows_rendered = crate::text::instance::rows_to_render(
                rows,
                term.view_offset_pixel(),
                cell_size.1,
                content_dims.1,
            );

            let t_shape = if trace {
                Some(std::time::Instant::now())
            } else {
                None
            };
            let shaped = text.rows.shape_rows(
                &mut text.rasterizer,
                &self.queue,
                term,
                rows_rendered,
                &rich_suppress,
            );
            if let Some(t) = t_shape {
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                tracing::info!(target: "render_trace", "shape rows={shaped} took={ms:.3}ms");
            }
            any_blink = text.rows.row_blink()[..rows_rendered as usize]
                .iter()
                .any(|b| *b);
            // make_bind_group used to be rebuilt every frame "in case
            // the atlas textures grew". They don't grow inside the
            // render path — `queue.write_texture` makes uploads visible
            // through the existing view. So we keep the bind group from
            // `with_font_ex` until either resize or font swap rebuilds
            // it for real.
        }
        // Arm / disarm the text-blink timer from what's on screen now,
        // so idle frames with no blinking text schedule no wake-ups.
        self.text_blink.observe(any_blink, now);

        // Cascade rule for the direct-to-swapchain optimization
        // (must run BEFORE the build-path decision below, otherwise
        // we'd build *partial* instances and then render with
        // `LoadOp::Clear`, painting only the dirty cells over a fresh
        // bg — the rest of the screen would be bg color):
        //
        // A partial-redraw frame needs `LoadOp::Load` on a stable
        // previous-frame target, which is what the scratch texture
        // provides. If the previous frame went direct-to-swapchain,
        // scratch holds stale pixels — force a full clear so we
        // re-emit everything instead of leaking stale content
        // through `LoadOp::Load`.
        if !self.needs_full_clear && !term.damage().all && self.scratch_stale {
            self.needs_full_clear = true;
        }

        let theme = self.theme;
        // Floor-divided grid leftover: the content area can be wider/taller
        // than a whole number of cells, leaving an uncovered sliver. When
        // bleed is on, the edge cells must reach the *window* edge, not
        // just the next cell boundary — so each edge's bleed distance is
        // `padding + its share of the leftover`. The leading (left/top)
        // share is the `GridAlign` offset already baked into the content
        // origin; the trailing (right/bottom) share is the rest. For
        // `TopLeft` the whole leftover is trailing; for `Centered` it's
        // split evenly, matching the centered origin. Same `grid_overflow`
        // the origin uses, so the two stay consistent.
        let ([rem_w, rem_h], [lead_x, lead_y]) = self.grid_overflow(cell_size);
        // Resolve the global edge-bleed gate once here, where `Term` and
        // config meet.
        let bleed_active = self.extend_background_when.active(term.is_alt_active());
        let ext = self.extend_background;
        // The `SolidLine` rule needs to know which edge rows/columns are an
        // all-non-default-bg "solid band". Scan the visible grid once, but
        // only for the axes that actually use `SolidLine`. A row's/column's
        // solid status flipping changes whether its edge cells bleed — and
        // the dirty path can't repaint non-dirty edge cells — so when these
        // flags differ from last frame we force a full clear. `solid_cols`
        // doubles as the `col_fills` slice handed to the builders.
        let need_rows = bleed_active && ext.horizontal == ExtendCondition::SolidLine;
        let need_cols = bleed_active && ext.vertical == ExtendCondition::SolidLine;
        let (solid_rows, solid_cols) = if need_rows || need_cols {
            let (rows, cols) = term.size();
            let mut solid_rows = vec![false; if need_rows { rows as usize } else { 0 }];
            let mut solid_cols = vec![true; if need_cols { cols as usize } else { 0 }];
            for r in 0..rows {
                let row = term.view_row(r);
                let mut row_all = true;
                // Index loop: one pass updates both the per-row reduce
                // (`row_all`) and the per-column reduce (`solid_cols[c]`).
                #[allow(clippy::needless_range_loop)]
                for c in 0..cols as usize {
                    let filled = row
                        .cells()
                        .get(c)
                        .is_some_and(crate::text::instance::cell_fills_bg);
                    if !filled {
                        row_all = false;
                        if need_cols {
                            solid_cols[c] = false;
                        }
                    }
                }
                if need_rows {
                    solid_rows[r as usize] = row_all;
                }
            }
            (solid_rows, solid_cols)
        } else {
            (Vec::new(), Vec::new())
        };
        if (need_rows && solid_rows != self.solid_rows_prev)
            || (need_cols && solid_cols != self.solid_cols_prev)
        {
            self.needs_full_clear = true;
        }
        self.solid_rows_prev = solid_rows;
        self.solid_cols_prev = solid_cols;
        // `pad` ordering is [top, right, bottom, left] (matching
        // PAD_TOP/RIGHT/BOTTOM/LEFT / set_padding) — distinct from
        // content_origin's (x=left, y=top). Each edge's bleed distance is
        // `padding + its share of the floor-divide leftover`. Computed by
        // value (all Copy except the `col_fills` borrow of a disjoint
        // field) before the `text` borrow so it doesn't conflict.
        #[allow(clippy::cast_precision_loss)]
        let bleed = EdgeBleed {
            pad: [
                self.pad_top as f32 + lead_y,
                self.pad_right as f32 + (rem_w - lead_x),
                self.pad_bottom as f32 + (rem_h - lead_y),
                self.pad_left as f32 + lead_x,
            ],
            active: bleed_active,
            horizontal: ext.horizontal,
            vertical: ext.vertical,
            col_fills: &self.solid_cols_prev,
        };
        // Precompute the content origin + full surface here, before the
        // `&mut self.text` borrow below — the overlay/scroll-button call
        // sites can't call `self.content_origin()` (which borrows `&self`)
        // while `text` aliases `self.text`. `content_dims` is computed
        // above the shape loop. All by value (Copy).
        let content_origin = self.content_origin();
        let close_dialog = self.close_dialog_layout().map(|layout| {
            let colors = dialog::dialog_colors(theme.bg, theme.fg, self.close_dialog_hover);
            (layout, colors)
        });
        // Build instances using the cached row glyphs. Reuse the
        // scratch vec across frames. We have to temporarily extract
        // the scratch out of TextState because the builders need to
        // read `text.line_cache` immutably while writing to the scratch;
        // can't hold two borrows of TextState at once.
        let damage_all = term.damage().all;
        // Combine blink state with the app-side hide flag (DECSET 25).
        // Apps like yazi / helix / neovim toggle 25 to hide the cursor
        // during their alt-screen UI; if we ignored it the cursor block
        // sat over their layout. Also suppress the cursor while the
        // user is scrolled back into history — convention matches Kitty
        // and Alacritty.
        let cursor_visible =
            self.blink.visible && term.cursor_visible() && !term.is_view_scrolled_back();
        // Split-borrow: take a shared borrow of `ext_palette` and a
        // mutable borrow of `text` from disjoint fields of `self` so
        // the builders can read the cached OSC 4 palette while we hold
        // an exclusive borrow of TextState. Coercing via `&*` then
        // `&[[f32;4];256]` avoids the implicit `self` reborrow that
        // would otherwise alias `self.text`.
        let ext_palette: &[[f32; 4]; 256] = &self.ext_palette;
        let text = self.text.as_mut().expect("text init");
        let mut instances = std::mem::take(&mut text.instances_scratch);
        let cell_opts = crate::text::instance::CellRenderOpts {
            blink_on: self.text_blink.blink_on(),
            decor: Some(text.rasterizer.decoration_metrics()),
            // Only consulted by the full builder, which only runs on
            // `LoadOp::Clear(theme.bg)` frames.
            skip_default_bg: crate::text::instance::may_skip_default_bg(&bleed, below_bg_active),
        };
        let t_bi = if trace {
            Some(std::time::Instant::now())
        } else {
            None
        };
        // Pick the builder: full build when the framebuffer is being
        // cleared (LoadOp::Clear); partial build under LoadOp::Load
        // so we only emit instances for cells that actually changed.
        if self.needs_full_clear || damage_all {
            build_term_instances_into(
                &mut instances,
                term,
                cell_size,
                &theme,
                ext_palette,
                &text.rows,
                bleed,
                content_dims.1,
                &cell_opts,
            );
            finish_full_build(&mut instances, cursor_visible);
        } else {
            build_term_dirty_instances_into(
                &mut instances,
                term,
                cell_size,
                &theme,
                ext_palette,
                cursor_visible,
                &text.rows,
                bleed,
                &cell_opts,
            );
        }
        if let Some(t) = t_bi {
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            tracing::info!(target: "render_trace", "build_instances n={} took={ms:.3}ms", instances.len());
        }

        // Debug overlay (TOASTTY_DEBUG FPS counter, etc). Shape the
        // string fresh each frame and append cell instances aligned to
        // the top-right corner of the viewport. The per-char glyph cache
        // makes the shape call cheap for short strings.
        //
        // Each frame we re-emit a bg quad over every overlay cell so the
        // previous frame's text is overpainted under LoadOp::Load — this
        // is why the caller pads to a fixed width.
        //
        // Split-borrow `&self.debug_overlay` (read) from `&mut self.text`
        // (we already hold `text`) to avoid the per-frame `String` clone.
        if let Some(overlay) = self.debug_overlay.as_deref() {
            #[allow(clippy::cast_possible_truncation)]
            let n = overlay.chars().count() as u16;
            let lg = text
                .rasterizer
                .shape_line(&self.queue, overlay, term.grapheme_cluster_mode());
            // Anchor to the content top-right (flows through the
            // content_origin uniform), so use content width here.
            let viewport_w = content_dims.0;
            let cell_w = cell_size.0;
            let cell_h = cell_size.1;
            let overlay_bg = [0.0, 0.0, 0.0, 0.85];
            let overlay_fg = [0.95, 0.85, 0.30, 1.0];
            let x0 = viewport_w - f32::from(n) * cell_w;
            for (i, ch) in overlay.chars().enumerate() {
                #[allow(clippy::cast_possible_truncation)]
                let col = i as u16;
                let pos = [x0 + f32::from(col) * cell_w, 0.0];
                instances.push(CellInstance {
                    pos,
                    size: [cell_w, cell_h],
                    uv_min: [0.0, 0.0],
                    uv_max: [0.0, 0.0],
                    fg: overlay_fg,
                    bg: overlay_bg,
                    flags: crate::text::instance::FLAG_NO_GLYPH,
                    pad: [0; 3],
                });
                if ch.is_whitespace() {
                    continue;
                }
                if let Some(slot) = lg.get(col, ch) {
                    let flags = if slot.is_color {
                        crate::text::instance::FLAG_COLOR_GLYPH
                    } else {
                        0
                    };
                    instances.push(CellInstance {
                        pos: [pos[0] + slot.glyph_offset[0], pos[1] + slot.glyph_offset[1]],
                        size: slot.glyph_size,
                        uv_min: slot.uv_min,
                        uv_max: slot.uv_max,
                        fg: overlay_fg,
                        bg: overlay_bg,
                        flags,
                        pad: [0; 3],
                    });
                }
            }
        }

        // Config error banner: full-width, multi-line, dark-red bg /
        // white fg, anchored to the BOTTOM of the viewport. The
        // two-cover-quad-per-cell drawing (and why) lives in
        // [`Self::draw_banner`]. alpha ≈ 0.95 gives a hint of
        // translucency so the panel doesn't feel hard-edged without
        // letting the underlying grid be legible through it.
        if let Some(banner) = self.error_banner.as_deref() {
            let banner_bg = [0.42, 0.03, 0.03, 0.95];
            let banner_fg = [1.0, 1.0, 1.0, 1.0];
            // Bottom-anchored to the content area: top-y is the content
            // height minus the banner's total line height (clamped
            // non-negative). Positions flow through the content_origin
            // uniform, so pass content dims.
            let (cdw, cdh) = content_dims;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            Self::draw_banner(
                &self.queue,
                cdw as u32,
                cdh as u32,
                &mut instances,
                text,
                banner,
                banner_bg,
                banner_fg,
                cell_size,
                term,
            );
        }

        // IME preedit overlay. The in-progress composition string is
        // drawn inline starting at the terminal cursor cell, advancing
        // cell-by-cell to the right and overlaying whatever grid content
        // sits underneath. Rendered underlined (and with the theme's
        // cursor color as fg) so it reads as distinct, half-formed text
        // until the IME commits it into the grid (or clears it).
        //
        // We reuse the same cursor-cell anchor the cursor block uses:
        // `cursor_pixel_rect` clamps `term.cursor()` into the grid and
        // applies the sub-pixel scroll y-translate. We take its left edge
        // (x0) for the start column; the cell-top y is recomputed from the
        // same clamped row so the underline shapes/underlines align with
        // the grid regardless of cursor shape (Underline shifts the rect's
        // own y down).
        if let Some(preedit) = self.preedit.as_deref() {
            let (rows, cols) = term.size();
            let cell_w = cell_size.0;
            let cell_h = cell_size.1;
            let cursor_rect = crate::text::instance::cursor_pixel_rect(term, cell_size);
            // Cell-top y for the cursor row, mirroring cursor_pixel_rect's
            // cell_pos: clamp the cursor row into the grid and apply the
            // same sub-pixel scroll y-translate.
            let view_pixel = term.view_offset_pixel();
            let y_translate = if view_pixel > 0.0 {
                view_pixel - cell_h
            } else {
                0.0
            };
            let cur_row = u16::min(term.cursor().row, rows.saturating_sub(1));
            let y0 = f32::from(cur_row) * cell_h + y_translate;
            // Start column from the cursor rect's left edge (already
            // clamped + cell-aligned by cursor_pixel_rect).
            let start_x = cursor_rect[0];
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let start_col = (start_x / cell_w).round() as u16;

            let lg = text
                .rasterizer
                .shape_line(&self.queue, preedit, term.grapheme_cluster_mode());
            let mode_2027 = term.grapheme_cluster_mode();
            let preedit_fg = theme.cursor;
            // Walk the composition string char-by-char, advancing the
            // overlay column by each char's display width so wide CJK
            // glyphs occupy two cells (matching the shaper's column
            // assignment, which leaves the continuation column empty).
            let mut col: u16 = start_col;
            let mut buf = [0u8; 4];
            for ch in preedit.chars() {
                let ch_str = ch.encode_utf8(&mut buf);
                let width = u16::from(cluster_cell_width(ch_str, mode_2027));
                // Clip at the right edge: stop emitting once the glyph
                // (or its trailing continuation cell) would run past the
                // last column. Don't wrap.
                if col >= cols || col + width > cols {
                    break;
                }
                let pos = [f32::from(col) * cell_w, y0];
                let span = [f32::from(width) * cell_w, cell_h];
                // Cover the grid cell underneath so the half-formed glyph
                // reads cleanly, not mixed with whatever was already
                // there. Two covers, mirroring the error banner: a bg-pass
                // quad (FLAG_NO_GLYPH) repaints the cell bg, and a
                // glyph-pass cover (FLAG_UNDERLINE | FLAG_NO_GLYPH emits a
                // solid bg) overpaints the term's own glyph for this cell
                // — which was already emitted earlier under LoadOp::Load.
                instances.push(CellInstance {
                    pos,
                    size: span,
                    uv_min: [0.0, 0.0],
                    uv_max: [0.0, 0.0],
                    fg: preedit_fg,
                    bg: theme.bg,
                    flags: crate::text::instance::FLAG_NO_GLYPH,
                    pad: [0; 3],
                });
                instances.push(CellInstance {
                    pos,
                    size: span,
                    uv_min: [0.0, 0.0],
                    uv_max: [0.0, 0.0],
                    fg: preedit_fg,
                    bg: theme.bg,
                    flags: crate::text::instance::FLAG_UNDERLINE
                        | crate::text::instance::FLAG_NO_GLYPH,
                    pad: [0; 3],
                });
                // Underline strip spanning the char's full cell width,
                // reusing the same machinery a normal underlined cell
                // emits (FLAG_UNDERLINE, fg-as-bg). This is the preedit's
                // visual distinction.
                instances.push(crate::text::instance::underline_instance(
                    pos, span, preedit_fg,
                ));
                // The glyph, looked up at this column. `lg` was shaped from
                // the preedit string alone, so its columns are 0-based
                // relative to the composition start — subtract `start_col`
                // to map this absolute grid column back into that space.
                // (Continuation cells for wide chars are left empty by the
                // shaper, same as the grid.)
                if !ch.is_whitespace()
                    && let Some(slot) = lg.get(col - start_col, ch)
                {
                    let flags = if slot.is_color {
                        crate::text::instance::FLAG_COLOR_GLYPH
                    } else {
                        0
                    };
                    instances.push(CellInstance {
                        pos: [pos[0] + slot.glyph_offset[0], pos[1] + slot.glyph_offset[1]],
                        size: slot.glyph_size,
                        uv_min: slot.uv_min,
                        uv_max: slot.uv_max,
                        fg: preedit_fg,
                        bg: theme.bg,
                        flags,
                        pad: [0; 3],
                    });
                }
                col += width;
            }
        }

        // Close dialog text, last so it can be drawn on its own after the
        // dialog panel (see the render pass). Glyph-only instances, so the
        // bg pass skips them.
        let dialog_glyphs_start = instances.len();
        if let Some((layout, colors)) = close_dialog.as_ref() {
            for run in &layout.lines {
                push_dialog_text(
                    &self.queue,
                    text,
                    &mut instances,
                    run,
                    |tone| colors.text(tone),
                    cell_size,
                    content_origin,
                );
            }
            for button in layout.buttons() {
                let bc = colors.button(button.kind);
                let key_text = button.keycap.as_ref().and_then(|kc| match &kc.glyph {
                    dialog::KeyGlyph::Text(run) => Some(run),
                    dialog::KeyGlyph::Return(_) => None,
                });
                for run in std::iter::once(&button.label).chain(key_text) {
                    push_dialog_text(
                        &self.queue,
                        text,
                        &mut instances,
                        run,
                        |tone| bc.text(tone),
                        cell_size,
                        content_origin,
                    );
                }
            }
        }

        // M11a: build image instances split by z sign. We clear the
        // existing vecs and rebuild every frame (cheap: one instance per
        // placement). Resolving binds each instance to its texture,
        // uploading images on their first draw (SIZE-8); it runs even
        // with no placements so undrawn textures can be evicted.
        self.image_instances_below_bg.clear();
        self.image_instances_below.clear();
        self.image_instances_above.clear();
        // A3: placements are positioned against the content origin
        // (pixel sizing snaps to physical pixels); take it before the
        // pipeline borrow below.
        let image_origin = self.content_origin();
        if let Some(image_pipeline) = self.image_pipeline.as_mut() {
            let t_img = trace.then(std::time::Instant::now);
            let mut tmp: Vec<ImageInstance> = Vec::new();
            if !term.image_grid().is_empty() {
                // Placements are anchored to line ids: place them against
                // the live screen's top line and shift them by the same
                // scrollback offset text gets (`(L + view_lines) * cell_h +
                // view_pixel` for logical row L), then crop to the content
                // area.
                #[allow(clippy::cast_precision_loss)]
                let scroll_px =
                    term.view_offset_lines() as f32 * cell_size.1 + term.view_offset_pixel();
                let view = ImageView {
                    top_line: term.image_top_line(),
                    scroll_px,
                    clip: image_clip(term, cell_size, content_dims.1),
                    origin: image_origin,
                };
                build_image_instances(
                    &mut tmp,
                    term.image_grid(),
                    term.image_registry(),
                    cell_size,
                    &view,
                );
            }
            if term.has_virtual_placements() {
                // Unicode-placeholder images, resolved from the rows drawn
                // this frame, before the textures are resolved. Re-sorts
                // `tmp` by (z, id) for the split.
                let view_pixel = term.view_offset_pixel();
                crate::image::placeholder::build_placeholder_instances(
                    &mut tmp,
                    term,
                    cell_size,
                    crate::image::placeholder::RowLayout {
                        rows_rendered: crate::text::instance::rows_to_render(
                            rows,
                            view_pixel,
                            cell_size.1,
                            content_dims.1,
                        ),
                        y_translate: if view_pixel > 0.0 {
                            view_pixel - cell_size.1
                        } else {
                            0.0
                        },
                    },
                    (!(self.needs_full_clear || damage_all)).then(|| term.damage()),
                );
            }
            let stats = image_pipeline.resolve_kitty_textures(
                &self.device,
                &self.queue,
                term.image_registry(),
                &mut self.image_tex_cache,
                &mut tmp,
            );
            self.mip_work_pending = stats.deferred > 0;
            if let Some(t) = t_img
                && (!tmp.is_empty() || !self.image_tex_cache.is_empty())
            {
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                tracing::info!(
                    target: "render_trace",
                    "images n={} uploaded={} mipped={} deferred={} evicted={} resident={} ({} KiB) took={ms:.3}ms",
                    tmp.len(),
                    stats.uploaded,
                    stats.mipped,
                    stats.deferred,
                    stats.evicted,
                    self.image_tex_cache.len(),
                    self.image_tex_cache.total_bytes() / 1024,
                );
            }
            let (below_bg, below, above) = split_layers(&tmp);
            self.image_instances_below_bg.extend_from_slice(below_bg);
            self.image_instances_below.extend_from_slice(below);
            self.image_instances_above.extend_from_slice(above);
        }

        // Rich-block slices, then the selection tint over them.
        self.rich_instances.clear();
        if rich_active {
            let rows_draw: Vec<crate::image::rich::RichRowDraw> =
                rich_draws.iter().map(|d| d.1).collect();
            crate::image::rich::build_rich_instances(
                &mut self.rich_instances,
                &self.rich_tex_cache,
                &rows_draw,
                cell_size.1,
            );
            self.push_rich_selection_tint(term, &rich_draws, cell_size);
        }

        // Acquire surface frame. This is where `Fifo` present mode
        // blocks waiting for vsync; if any prior frame is still queued,
        // we sit here for ~16.7ms.
        let t_acq = if trace {
            Some(std::time::Instant::now())
        } else {
            None
        };
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                // Followup C2: the reconfigured back-buffer has
                // undefined contents (same invariant as `resize`).
                // Without flagging a full clear here, the next frame
                // may pick `LoadOp::Load` and read garbage. The cost
                // is one extra clear on recovery — cheap.
                self.needs_full_clear = true;
                // No frame went out — caller must not clear damage / BSU
                // force-flushed flag, so report Skipped.
                return Ok(RenderOutcome::Skipped);
            }
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Validation => {
                return Err(RenderError::SurfaceLost);
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                // Followup C2: defensively force the next frame to
                // clear. Timeout means the driver missed its
                // deadline; Occluded means the window isn't visible.
                // In both cases the back-buffer's prior contents may
                // be stale — flagging a full clear avoids any chance
                // of a LoadOp::Load reading garbage on recovery.
                self.needs_full_clear = true;
                return Ok(RenderOutcome::Skipped);
            }
        };
        if let Some(t) = t_acq {
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            tracing::info!(target: "render_trace", "surface_acquire took={ms:.3}ms (blocks on vsync under Fifo)");
        }

        let t_enc = if trace {
            Some(std::time::Instant::now())
        } else {
            None
        };
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("toastty-render encoder (term)"),
            });
        // SIZE-12: mip chains for images first drawn downscaled this
        // frame, ahead of the pass that samples them.
        if let Some(img_pipe) = self.image_pipeline.as_mut() {
            img_pipe.encode_pending_mips(&self.device, &mut encoder);
        }

        // Content origin (physical px) added to every quad in the vertex
        // shader before the px->NDC map. `viewport_and_atlas.xy` MUST stay
        // full-surface (the px->NDC divisor) so grown edge quads map past
        // the content area into the gutter rather than being rescaled.
        // Reuse the value precomputed before the `text` borrow.
        let origin = content_origin; // [pad_left, pad_top]

        // Cursor rect + color travel via the Globals UBO so the glyph
        // fragment shader can recolor any glyph pixel that overlaps the
        // cursor block. When the cursor is hidden we pass an all-zero
        // rect (degenerate) so the shader's strict-inside test never
        // matches — the glyph keeps its normal fg.
        //
        // `fs_glyph` compares raw `in.clip.xy` (post-origin framebuffer
        // px) against `cursor_rect`, so the rect is pre-offset by `origin`
        // here on the CPU (NOT in the shader). A hidden cursor stays
        // `[0; 4]` (degenerate; the origin shift is irrelevant on a
        // zero-area rect).
        let cursor_rect = if cursor_visible {
            let r = crate::text::instance::cursor_pixel_rect(term, cell_size);
            [
                r[0] + origin[0],
                r[1] + origin[1],
                r[2] + origin[0],
                r[3] + origin[1],
            ]
        } else {
            [0.0; 4]
        };
        #[allow(clippy::cast_precision_loss)] // viewport/atlas sizes fit comfortably in 24 bits.
        let globals = GlobalsUbo {
            viewport_and_atlas: [
                self.config.width as f32,
                self.config.height as f32,
                atlas_dims.0 as f32,
                atlas_dims.1 as f32,
            ],
            cursor_rect,
            cursor_color: self.theme.cursor,
            content_origin: [origin[0], origin[1], 0.0, 0.0],
        };

        // damage.all is the M8 corrective-flush path: cascade it into
        // a full clear for this frame.
        if term.damage().all {
            self.needs_full_clear = true;
        }

        let load_op = if self.needs_full_clear {
            // Premultiply the theme bg before clearing: no-op when alpha==1
            // (rgb*1==rgb); enables premultiplied transparency when alpha<1
            // so the desktop shows through on a premultiplied-alpha surface.
            wgpu::LoadOp::Clear(premultiplied_color(self.theme.bg))
        } else {
            wgpu::LoadOp::Load
        };

        // Depth load op tracks the color load op: on a full clear we
        // clear depth to 1.0 (NDC far); on a partial-redraw frame
        // we Load so the previous frame's depth (cell layer at 0.5,
        // any 3D objects at their NDC z) is preserved.
        let depth_load_op = if self.needs_full_clear {
            wgpu::LoadOp::Clear(1.0)
        } else {
            wgpu::LoadOp::Load
        };

        // Full-clear frames render directly into the acquired swapchain
        // back-buffer, skipping the scratch texture and the per-frame
        // `copy_texture_to_texture` blit (12–36 MB of GPU bandwidth per
        // frame at typical sizes). LoadOp::Clear means we don't need
        // the previous-frame contents on the back-buffer, so swapchain
        // rotation isn't a problem. Partial-redraw frames still target
        // scratch + blit because LoadOp::Load needs the stable
        // previous-frame pixels scratch provides.
        let render_direct = self.needs_full_clear;
        let frame_view = if render_direct {
            Some(
                frame
                    .texture
                    .create_view(&wgpu::TextureViewDescriptor::default()),
            )
        } else {
            None
        };
        let color_view: &wgpu::TextureView = if let Some(v) = frame_view.as_ref() {
            v
        } else {
            &self.scratch_view
        };

        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("toastty-render term pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    // Either the scratch texture (partial-redraw path)
                    // or the acquired swapchain back-buffer (full-clear
                    // direct path). See `render_direct` selection above.
                    view: color_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: load_op,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.scratch_depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: depth_load_op,
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            // Draw order:
            //   0. below-cell-bg images (z < INT32_MIN/2) — drawn first
            //      so the text bg pass paints over them.
            //   1. text bg pass — no depth test/write, so 3D overpaints
            //      cell backgrounds (3D shows through).
            //   2. RGP 3D pass — writes color + depth.
            //   3. below-text images — depth-tested at z=0.5.
            //   4. text glyph pass — depth-tested at z=0.5; cursor,
            //      underline, and glyphs live here.
            //   5. above-text images.
            //   6./7. scroll-to-bottom and "Copy Markdown" chips.
            //   8. close dialog (shapes, then its glyphs), topmost.
            // Z-ordering between 3D and {glyphs, images} is handled by
            // the per-placement `depth` field: objects with
            // `depth < 0` win the depth test against the z=0.5 cell
            // layer, `depth > 0` lose to it. Cell bg is always behind.
            #[allow(clippy::cast_precision_loss)] // viewport in 16k-px range.
            let viewport = (self.config.width as f32, self.config.height as f32);

            if let Some(img_pipe) = self.image_pipeline.as_mut() {
                img_pipe.begin_frame();
            }
            // m2: below-cell-bg images draw before the text bg pass so
            // the cell background colors paint over them.
            if let Some(img_pipe) = self.image_pipeline.as_mut()
                && !self.image_instances_below_bg.is_empty()
            {
                img_pipe.render(
                    &self.device,
                    &self.queue,
                    &mut rp,
                    &self.image_instances_below_bg,
                    viewport,
                    origin,
                );
            }

            // Upload text instances + globals once; both bg and glyph
            // passes share the buffer. Drop the &mut borrow before
            // taking the shared refs the render pass holds.
            {
                let text_mut = self.text.as_mut().expect("text init checked above");
                text_mut
                    .pipeline
                    .upload(&self.device, &self.queue, globals, &instances);
            }
            let text_ref = self.text.as_ref().expect("text init checked above");
            // The close dialog's glyphs (if any) sit at the end and are
            // drawn after its panel, below.
            let inst_count = dialog_glyphs_start;
            text_ref
                .pipeline
                .render_bg(&mut rp, &text_ref.bind_group, inst_count);

            // Rich-block slices (+ selection tint) over the cell
            // backgrounds, under everything else.
            if let Some(img_pipe) = self.image_pipeline.as_mut()
                && !self.rich_instances.is_empty()
            {
                img_pipe.render(
                    &self.device,
                    &self.queue,
                    &mut rp,
                    &self.rich_instances,
                    viewport,
                    origin,
                );
            }

            if let Some(rgp_pipe) = self.rgp_pipeline.as_mut() {
                rgp_pipe.render(
                    &self.device,
                    &self.queue,
                    &mut rp,
                    term.rgp_scene(),
                    &self.rgp_cache,
                    viewport,
                    cell_size,
                    (origin[0], origin[1]),
                );
            }

            if let Some(img_pipe) = self.image_pipeline.as_mut()
                && !self.image_instances_below.is_empty()
            {
                img_pipe.render(
                    &self.device,
                    &self.queue,
                    &mut rp,
                    &self.image_instances_below,
                    viewport,
                    origin,
                );
            }

            text_ref
                .pipeline
                .render_glyph(&mut rp, &text_ref.bind_group, inst_count);

            if let Some(img_pipe) = self.image_pipeline.as_mut()
                && !self.image_instances_above.is_empty()
            {
                img_pipe.render(
                    &self.device,
                    &self.queue,
                    &mut rp,
                    &self.image_instances_above,
                    viewport,
                    origin,
                );
            }

            // 6. scroll-to-bottom chip, on top of everything.
            if chip_alpha > 0.0
                && let Some(chip_pipe) = self.chip_pipeline.as_ref()
                && let Some(geom) = self.scroll_chip_geometry()
            {
                let colors = chip::chip_colors(self.theme.bg, self.theme.fg, self.chip_hover);
                let uniform = ChipUniform::new(viewport, &geom, self.ui_scale, chip_alpha, &colors);
                chip_pipe.render(&self.queue, &mut rp, &uniform);
            }

            // 7. rich block "Copy Markdown" chip.
            if self.close_prompt.is_none()
                && let Some(chip_pipe) = self.rich_chip_pipeline.as_ref()
                && let Some(geom) = self.rich_chip_geometry(term)
            {
                let colors = chip::chip_colors(self.theme.bg, self.theme.fg, self.rich_chip_hover);
                let uniform = ChipUniform::new(viewport, &geom, self.ui_scale, 1.0, &colors)
                    .with_icon(chip::pipeline::ICON_COPY);
                chip_pipe.render(&self.queue, &mut rp, &uniform);
            }

            // 8. close dialog, topmost: scrim, shadow, panel, buttons and
            //    icon, then its text on the panel.
            if let Some((layout, colors)) = close_dialog.as_ref()
                && let Some(ui_pipe) = self.ui_pipeline.as_mut()
            {
                let rects = dialog::dialog_rects(layout, colors);
                ui_pipe.render(&self.device, &self.queue, &mut rp, &rects, viewport);
                text_ref.pipeline.render_glyph_range(
                    &mut rp,
                    &text_ref.bind_group,
                    dialog_glyphs_start..instances.len(),
                );
            }
        }

        // Blit scratch → swapchain only on partial-redraw frames; the
        // direct path rendered straight into the back-buffer and
        // doesn't need (and would clobber) the copy.
        if !render_direct {
            encoder.copy_texture_to_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.scratch_texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyTextureInfo {
                    texture: &frame.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::Extent3d {
                    width: self.config.width,
                    height: self.config.height,
                    depth_or_array_layers: 1,
                },
            );
        }

        if let Some(t) = t_enc {
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            tracing::info!(target: "render_trace", "encode_pass took={ms:.3}ms");
        }
        let t_sub = if trace {
            Some(std::time::Instant::now())
        } else {
            None
        };
        self.queue.submit(std::iter::once(encoder.finish()));
        frame.present();
        if let Some(t) = t_sub {
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            tracing::info!(target: "render_trace", "submit+present took={ms:.3}ms");
        }
        if let Some(t) = t_total {
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            tracing::info!(target: "render_trace", "render_term TOTAL={ms:.3}ms ----");
        }

        // Return the scratch vec to TextState so the next frame reuses
        // the allocation.
        if let Some(text) = self.text.as_mut() {
            text.instances_scratch = instances;
        }
        // Successful submit — the framebuffer now holds known contents,
        // so the next frame can use LoadOp::Load unless someone
        // explicitly invalidates again.
        self.needs_full_clear = false;
        // Track which target actually got written this frame:
        // - direct path wrote the swapchain back-buffer; scratch is
        //   now stale relative to "what's on screen"
        // - scratch path wrote scratch (+ blitted); scratch is current
        self.scratch_stale = render_direct;
        Ok(RenderOutcome::Rendered)
    }

    /// Render one frame. M4a: just a clear-color pass.
    pub fn render(&mut self) -> Result<(), RenderError> {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated => {
                // Surface size mismatch — reconfigure and try next frame.
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Validation => {
                return Err(RenderError::SurfaceLost);
            }
            // Driver missed the deadline (Timeout) or window not visible
            // (Occluded) — silently skip this frame.
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(());
            }
        };

        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("toastty-render encoder"),
            });

        {
            let _rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // Premultiply the clear color: no-op when alpha==1
                        // (rgb*1==rgb); enables premultiplied transparency
                        // when alpha<1 on a premultiplied-alpha surface.
                        load: wgpu::LoadOp::Clear(premultiplied_color(self.clear_color)),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                // wgpu 29 papercut: required field, even when not using multiview.
                multiview_mask: None,
            });
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        frame.present();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `set_preedit`/`has_preedit` contract, exercised through the pure
    /// normalization helper they delegate to (a full `Renderer` needs a
    /// real window surface, so it can't be built in a unit test). A
    /// non-empty string is stored (`has_preedit` → true); `None` and the
    /// empty string both clear it (`has_preedit` → false).
    #[test]
    fn preedit_normalization_mirrors_has_preedit() {
        // set_preedit(Some("한")) → stored → has_preedit() == true.
        let some = normalize_preedit(Some("한"));
        assert_eq!(some.as_deref(), Some("한"));
        assert!(some.is_some());
        // set_preedit(None) → cleared → has_preedit() == false.
        assert!(normalize_preedit(None).is_none());
        // set_preedit(Some("")) → empty normalizes to None → false.
        assert!(normalize_preedit(Some("")).is_none());
    }

    #[test]
    fn instance_flags_test_includes_validation() {
        assert!(instance_flags_for_tests().contains(InstanceFlags::VALIDATION));
        assert!(instance_flags_for_tests().contains(InstanceFlags::DEBUG));
    }

    #[test]
    fn instance_flags_release_is_empty() {
        assert!(instance_flags_for_release().is_empty());
    }

    #[test]
    fn instance_descriptor_uses_primary_backends() {
        let d = instance_descriptor(InstanceFlags::empty());
        assert_eq!(d.backends, Backends::PRIMARY);
        assert!(d.display.is_none());
    }

    /// Followup C2: `RenderOutcome` is the contract that lets the binary
    /// gate the "clear dirty + clear BSU force-flushed" cleanup so it
    /// only runs after a real render. Skipped and Rendered must be
    /// distinguishable values so the match arms in main.rs can branch.
    #[test]
    fn render_outcome_variants_are_distinct() {
        assert_ne!(RenderOutcome::Rendered, RenderOutcome::Skipped);
        // Default-derived Eq + Copy keep the binary's match arms cheap.
        let o = RenderOutcome::Skipped;
        let p = o;
        assert_eq!(o, p);
    }

    // ----- M9 cursor blink ------------------------------------------------

    #[test]
    fn cursor_blink_animation_due_after_interval() {
        let now = Instant::now();
        let blink = CursorBlink {
            last_at: now
                .checked_sub(Duration::from_millis(600))
                .expect("now is past UNIX epoch"),
            visible: true,
            interval: Duration::from_millis(530),
            prev_enabled: true,
        };
        assert!(blink.animation_due(true, now));
    }

    #[test]
    fn cursor_blink_animation_not_due_before_interval() {
        let now = Instant::now();
        let blink = CursorBlink {
            last_at: now,
            visible: true,
            interval: Duration::from_millis(530),
            prev_enabled: true,
        };
        assert!(!blink.animation_due(true, now));
    }

    #[test]
    fn text_blink_is_disarmed_until_blinking_cells_appear() {
        let t0 = Instant::now();
        let mut b = TextBlink::new(t0);
        let later = t0 + Duration::from_secs(5);
        assert!(!b.due(later));
        assert!(b.next_deadline(later).is_none());
        assert!(b.blink_on());
        // Blinking text appears: armed, a full interval from now.
        b.observe(true, later);
        assert!(b.blink_on());
        assert!(!b.due(later));
        assert_eq!(b.next_deadline(later), Some(DEFAULT_CURSOR_BLINK_INTERVAL));
        let flip = later + DEFAULT_CURSOR_BLINK_INTERVAL;
        assert!(b.due(flip));
        b.tick(flip);
        assert!(!b.blink_on());
        // Still on screen: stays armed and off.
        b.observe(true, flip);
        assert!(!b.blink_on());
        assert!(b.next_deadline(flip).is_some());
        // Blinking text gone: disarmed, phase forced visible, no wake-ups.
        b.observe(false, flip);
        assert!(b.blink_on());
        assert!(!b.due(flip + Duration::from_secs(9)));
        assert!(b.next_deadline(flip).is_none());
    }

    #[test]
    fn text_blink_config_switch() {
        let t0 = Instant::now();
        let mut b = TextBlink::new(t0);
        assert!(!b.set_enabled(true), "already enabled");
        b.observe(true, t0);
        b.tick(t0 + DEFAULT_CURSOR_BLINK_INTERVAL);
        assert!(!b.blink_on());
        assert!(b.set_enabled(false));
        assert!(b.blink_on(), "disabling shows the text");
        b.observe(true, t0);
        assert!(b.next_deadline(t0).is_none(), "disabled never arms");
        assert!(!b.due(t0 + Duration::from_secs(9)));
        assert!(b.set_enabled(true));
        b.observe(true, t0);
        assert!(b.next_deadline(t0).is_some());
    }

    #[test]
    fn mark_blinking_cells_dirty_marks_only_blink_cells() {
        let mut t = Term::new(3, 6, 0);
        let mut p = toastty_parser::Parser::new();
        p.advance(
            &mut t,
            b"ab\x1b[5mcd\x1b[25mef\r\n\x1b[5m\xe4\xb8\xad\x1b[0m",
        );
        t.clear_damage();
        mark_blinking_cells_dirty(&mut t, &[true, true, false, true, true]);
        let d = t.damage();
        let row0: Vec<u16> = d.rows[0].cols.iter().copied().collect();
        assert_eq!(row0, vec![2, 3]);
        let row1: Vec<u16> = d.rows[1].cols.iter().copied().collect();
        // Wide blinking cluster: primary + continuation.
        assert_eq!(row1, vec![0, 1]);
        assert!(d.rows[2].is_empty());
    }

    #[test]
    fn cursor_blink_toggles_after_interval() {
        let mut blink = CursorBlink::new(Instant::now());
        let was_visible = blink.visible;
        blink.tick(Instant::now());
        assert_ne!(blink.visible, was_visible);
    }

    #[test]
    fn cursor_blink_disabled_when_term_blink_off() {
        let now = Instant::now();
        let blink = CursorBlink {
            last_at: now
                .checked_sub(Duration::from_secs(10))
                .expect("now is past UNIX epoch"),
            visible: true,
            interval: Duration::from_millis(530),
            prev_enabled: true,
        };
        // Term has blink disabled → animation NEVER due.
        assert!(!blink.animation_due(false, now));
        // And no deadline scheduled.
        assert!(blink.next_deadline(false, now).is_none());
    }

    #[test]
    fn next_redraw_deadline_is_some_when_blink_on() {
        let now = Instant::now();
        let blink = CursorBlink::new(now);
        let d = blink.next_deadline(true, now);
        assert!(d.is_some(), "deadline must be Some when blink is on");
        let d = d.unwrap();
        // Just after construction, the next toggle is ~`interval` away.
        assert!(d <= blink.interval);
    }

    #[test]
    fn next_redraw_deadline_saturates_at_zero_when_overdue() {
        let now = Instant::now();
        let blink = CursorBlink {
            last_at: now
                .checked_sub(Duration::from_secs(5))
                .expect("now is past UNIX epoch"),
            visible: true,
            interval: Duration::from_millis(530),
            prev_enabled: true,
        };
        let d = blink.next_deadline(true, now).unwrap();
        assert_eq!(d, Duration::ZERO, "overdue deadline must saturate to 0");
    }

    #[test]
    fn cursor_blink_force_visible_resets_phase() {
        let mut blink = CursorBlink::new(Instant::now());
        blink.visible = false;
        blink.force_visible();
        assert!(blink.visible);
    }

    /// Followup I1: on the steady → blinking edge, the previous
    /// `last_at` is stale (the term was disabled), so `animation_due`
    /// would return true immediately and the cursor would flicker on
    /// the very first frame after re-enable. `sync_enabled` must reset
    /// `last_at` and `visible` on the edge so the next tick is a full
    /// `interval` away.
    #[test]
    fn cursor_blink_re_enable_resets_phase() {
        // Set up a blink with a tiny interval (1 ms) and an ancient
        // `last_at`, then mark it as "previously disabled". The
        // simulated "now" is well past `last_at + interval`, which
        // without the fix would trigger an instant flash.
        let then = Instant::now();
        let mut blink = CursorBlink {
            last_at: then
                .checked_sub(Duration::from_secs(5))
                .expect("now is past UNIX epoch"),
            visible: false,
            interval: Duration::from_millis(1),
            prev_enabled: false,
        };
        // Simulate "later" (no real sleep needed — the edge check
        // operates on the `now` parameter).
        let later = then;
        blink.sync_enabled(true, later);

        // After the edge: visible is back ON, and the next deadline
        // is close to `interval` (not zero / overdue).
        assert!(blink.visible, "re-enable must force visible ON");
        let d = blink
            .next_deadline(true, later)
            .expect("blink is enabled now");
        assert_eq!(d, blink.interval, "fresh cycle: deadline == interval");
        // And no tick should be due right now.
        assert!(
            !blink.animation_due(true, later),
            "no instant flicker — first tick should be a full interval away"
        );
    }

    /// Followup I1: `sync_enabled` must be a no-op on the true → true
    /// edge so it doesn't reset the phase mid-cycle.
    #[test]
    fn cursor_blink_sync_enabled_noop_when_already_enabled() {
        let now = Instant::now();
        let original_last_at = now
            .checked_sub(Duration::from_millis(100))
            .expect("now is past UNIX epoch");
        let mut blink = CursorBlink {
            last_at: original_last_at,
            visible: false,
            interval: Duration::from_millis(530),
            prev_enabled: true,
        };
        blink.sync_enabled(true, now);
        // true → true: nothing changed.
        assert_eq!(blink.last_at, original_last_at);
        assert!(!blink.visible);
        assert!(blink.prev_enabled);
    }

    /// Followup I1: on the true → false edge `sync_enabled` should
    /// only record `prev_enabled = false`; it must not touch
    /// `last_at` or `visible` (the steady-cursor force-visible path
    /// lives elsewhere — `force_visible` — and a stale `last_at`
    /// while disabled is harmless since `animation_due` short-circuits).
    #[test]
    fn cursor_blink_sync_enabled_disable_only_records_edge() {
        let now = Instant::now();
        let original_last_at = now
            .checked_sub(Duration::from_millis(100))
            .expect("now is past UNIX epoch");
        let mut blink = CursorBlink {
            last_at: original_last_at,
            visible: false,
            interval: Duration::from_millis(530),
            prev_enabled: true,
        };
        blink.sync_enabled(false, now);
        assert_eq!(blink.last_at, original_last_at);
        assert!(!blink.visible);
        assert!(!blink.prev_enabled);
    }

    /// OSC 4 override-vs-default check: the renderer's `rebuild_ext_palette`
    /// helper must produce a different sRGB-linearised value when the
    /// term has an override at a given index versus when it doesn't.
    /// This exercises the cache-rebuild path without a GPU.
    #[test]
    fn ext_palette_rebuild_picks_up_override() {
        // We can't construct a `Renderer` without a GPU, but
        // `rebuild_ext_palette` only reads `Term::palette_override` +
        // `palette_revision`. Drive the helper directly on a stub by
        // mirroring its body: assert that the linearised value for a
        // freshly-set override differs from the default for the same
        // index.
        use toastty_parser::Parser;
        use toastty_term::Term;

        let mut term = Term::new(2, 4, 0);
        let default = toastty_protocols::palette::default_xterm_256(1);
        let mut p = Parser::new();
        // Set index 1 to white — clearly different from the xterm
        // default of (0x80, 0, 0).
        p.advance(&mut term, b"\x1b]4;1;rgb:ff/ff/ff\x1b\\");
        let override_rgb = term.palette_override(1).expect("override set");
        assert_ne!(override_rgb, default);

        // Linearisation must differ too (i.e. the rebuild path doesn't
        // collapse them into the same float vec). Comparison done
        // channel-by-channel with an epsilon to satisfy clippy's
        // `float_cmp` lint.
        let default_lin =
            crate::text::instance::srgb_to_linear_rgba(default[0], default[1], default[2]);
        let override_lin = crate::text::instance::srgb_to_linear_rgba(
            override_rgb[0],
            override_rgb[1],
            override_rgb[2],
        );
        let any_differs = (0..4).any(|i| (default_lin[i] - override_lin[i]).abs() > 1e-6);
        assert!(any_differs, "override should yield a different linear rgba");
    }

    /// Followup C1: when the blink toggles visible→invisible, the
    /// renderer must mark the cursor's cell dirty so the dirty-instance
    /// builder emits a fresh bg quad over the previous cursor block.
    /// Otherwise, under LoadOp::Load, the old cursor ghosts.
    ///
    /// This is the integration-level invariant: we simulate the
    /// `render_term` path's blink tick + `Term::mark_cell_dirty` +
    /// `build_dirty_instances_into` sequence, then assert the OFF
    /// frame's instance list contains a bg quad at the cursor cell
    /// without the cursor flag set.
    #[test]
    fn blink_off_emits_bg_quad_at_cursor_cell() {
        use crate::text::instance::{
            FLAG_CURSOR, FLAG_NO_GLYPH, Theme, build_dirty_instances_into,
        };
        use toastty_term::Term;

        let mut term = Term::new(3, 8, 0);
        // Cursor at (0, 0) by default. Position it to a distinctive
        // cell so we can assert position-precisely.
        let mut parser = toastty_parser::Parser::new();
        parser.advance(&mut term, b"\x1b[2;4H"); // row 2, col 4 (1-based)
        let cur = term.cursor();
        assert_eq!((cur.row, cur.col), (1, 3));
        term.clear_damage();

        // Simulate first blink tick: ON → OFF. The renderer's path is:
        //  1) detect animation_due
        //  2) tick (visible flips false)
        //  3) mark_cell_dirty at the cursor's current row/col
        //  4) build_dirty_instances_into with cursor_visible=false
        let cell_size = (8.0_f32, 16.0_f32);
        let theme = Theme::default_dark();
        term.mark_cell_dirty(cur.row, cur.col);
        let mut instances = Vec::new();
        build_dirty_instances_into(
            &mut instances,
            &term,
            term.damage(),
            cell_size,
            &theme,
            None,
            false, // cursor_visible == OFF frame
            |_, _, _, _| None,
            |_, _| false,
            crate::text::instance::EdgeBleed::default(),
        );

        // No cursor instance must be present (visible=false).
        assert!(
            instances.iter().all(|i| i.flags & FLAG_CURSOR == 0),
            "OFF frame must not emit any cursor instance"
        );
        // A background-only quad must be present at the cursor's cell.
        let expected_pos = [
            f32::from(cur.col) * cell_size.0,
            f32::from(cur.row) * cell_size.1,
        ];
        let bg_at_cursor = instances.iter().find(|i| {
            i.flags & FLAG_NO_GLYPH != 0
                && (i.pos[0] - expected_pos[0]).abs() < 1e-3
                && (i.pos[1] - expected_pos[1]).abs() < 1e-3
        });
        assert!(
            bg_at_cursor.is_some(),
            "OFF frame must emit a bg quad at the cursor's cell to overpaint the old cursor block"
        );
    }

    // ----- Grid alignment geometry -----------------------------------------

    #[test]
    fn grid_align_leading_fraction() {
        assert_eq!(GridAlign::default(), GridAlign::TopLeft);
        assert_eq!(GridAlign::TopLeft.leading_fraction(), 0.0);
        assert_eq!(GridAlign::Centered.leading_fraction(), 0.5);
    }

    #[test]
    fn grid_overflow_top_left_puts_all_leftover_trailing() {
        // 805×605 content, 10×20 cells → 80×30 grid, leftover (5, 5).
        let (rem, lead) = grid_overflow_px((805.0, 605.0), (10.0, 20.0), 0.0);
        assert!((rem[0] - 5.0).abs() < 1e-3, "rem_w");
        assert!((rem[1] - 5.0).abs() < 1e-3, "rem_h");
        // TopLeft: nothing on the leading edge.
        assert_eq!(lead, [0.0, 0.0]);
    }

    #[test]
    fn grid_overflow_centered_splits_leftover() {
        let (rem, lead) = grid_overflow_px((805.0, 605.0), (10.0, 20.0), 0.5);
        assert!((rem[0] - 5.0).abs() < 1e-3);
        assert!((rem[1] - 5.0).abs() < 1e-3);
        // Centered: half the leftover leads, floored to a whole pixel
        // (2.5 -> 2) so glyphs land on the pixel grid; the trailing
        // edge gets the remaining 3 px.
        assert_eq!(lead, [2.0, 2.0]);
    }

    #[test]
    fn grid_overflow_centered_even_leftover_splits_exactly() {
        let (rem, lead) = grid_overflow_px((806.0, 608.0), (10.0, 20.0), 0.5);
        assert_eq!(rem, [6.0, 8.0]);
        assert_eq!(lead, [3.0, 4.0]);
    }

    #[test]
    fn grid_overflow_lead_is_always_whole_pixels() {
        // Fractional cell sizes (DPI scaling) still give integer leads,
        // never more than the leftover.
        for w in [801.0_f32, 803.5, 809.9] {
            let (rem, lead) = grid_overflow_px((w, 600.0), (9.6, 20.0), 0.5);
            assert_eq!(lead[0], lead[0].floor(), "w={w}");
            assert!(lead[0] <= rem[0] * 0.5 + 1e-4, "w={w}");
        }
        // TopLeft (fraction 0) never leads.
        let (_, lead) = grid_overflow_px((805.0, 605.0), (10.0, 20.0), 0.0);
        assert_eq!(lead, [0.0, 0.0]);
    }

    #[test]
    fn grid_overflow_exact_multiple_is_zero() {
        let (rem, lead) = grid_overflow_px((800.0, 600.0), (10.0, 20.0), 0.5);
        assert_eq!(rem, [0.0, 0.0]);
        assert_eq!(lead, [0.0, 0.0]);
    }

    #[test]
    fn grid_overflow_degenerate_cell_is_zero() {
        // Zero cell size must not divide-by-zero / produce NaN.
        let (rem, lead) = grid_overflow_px((805.0, 605.0), (0.0, 0.0), 0.5);
        assert_eq!(rem, [0.0, 0.0]);
        assert_eq!(lead, [0.0, 0.0]);
    }

    #[test]
    fn grid_overflow_edges_reach_window_on_both_sides() {
        // The invariant the bleed relies on: leading bleed + grid span +
        // trailing bleed == content span + both pads == surface span, for
        // ANY alignment. Verify for TopLeft and Centered.
        const PAD_L: f32 = 4.0;
        const PAD_R: f32 = 7.0;
        let content_w = 805.0_f32;
        let cell_w = 10.0_f32;
        let cols = (content_w / cell_w).floor(); // 80
        let surface_w = content_w + PAD_L + PAD_R;
        for frac in [0.0_f32, 0.5] {
            let ([rem_w, _], [lead_x, _]) =
                grid_overflow_px((content_w, 600.0), (cell_w, 20.0), frac);
            // Origin offset from window left = pad_left + leading share.
            let left_bleed = PAD_L + lead_x;
            // Trailing share + right pad.
            let right_bleed = PAD_R + (rem_w - lead_x);
            let spanned = left_bleed + cols * cell_w + right_bleed;
            assert!(
                (spanned - surface_w).abs() < 1e-3,
                "frac={frac}: spanned {spanned} != surface {surface_w}"
            );
        }
    }
}

/// DECSCNM: swap the theme's default foreground and background. The
/// background keeps its alpha (window opacity) and the selection tint is
/// re-derived for the swapped pair.
#[must_use]
pub fn reverse_video_theme(theme: Theme) -> Theme {
    let [fr, fg_, fb, _] = theme.fg;
    let [br, bg_, bb, ba] = theme.bg;
    Theme {
        fg: [br, bg_, bb, 1.0],
        bg: [fr, fg_, fb, ba],
        ..theme
    }
    .with_default_selection_bg()
}

#[cfg(test)]
mod reverse_video_tests {
    use super::*;

    #[test]
    fn reverse_video_swaps_fg_bg_and_keeps_bg_alpha() {
        let base = Theme {
            fg: [0.9, 0.8, 0.7, 1.0],
            bg: [0.1, 0.2, 0.3, 0.5],
            ..Theme::default_dark()
        };
        let rev = reverse_video_theme(base);
        assert_eq!(rev.fg, [0.1, 0.2, 0.3, 1.0]);
        assert_eq!(rev.bg, [0.9, 0.8, 0.7, 0.5]);
        assert_eq!(rev.palette, base.palette);
        assert_eq!(rev.cursor, base.cursor);
    }
}

#[cfg(test)]
mod below_cell_bg_tests {
    use super::*;

    /// SIZE-9: only placements with `z < INT32_MIN/2` switch the frame
    /// into the "images under non-default backgrounds" mode.
    #[test]
    fn below_cell_bg_detection_uses_the_kitty_band() {
        use toastty_term::{ImageGrid, Placement, SrcRect};
        let place = |z: i32| {
            let mut p = Placement::new(1, 1, 1);
            p.key = toastty_term::ImageKey(1);
            p.src_rect = SrcRect::FULL;
            p.z = z;
            p
        };
        let mut reg = toastty_term::ImageRegistry::<toastty_term::ImageKey>::new(1024);
        reg.insert(
            toastty_term::ImageKey(1),
            toastty_term::ImageData {
                width: 1,
                height: 1,
                pixels: vec![0; 4],
            },
        )
        .unwrap();
        let mut grid = ImageGrid::new();
        assert!(!has_below_cell_bg_placements(&grid, &reg));
        grid.add(place(0));
        grid.add(place(-1));
        grid.add(place(i32::MIN / 2));
        assert!(!has_below_cell_bg_placements(&grid, &reg));
        grid.add(place(i32::MIN / 2 - 1));
        assert!(has_below_cell_bg_placements(&grid, &reg));
    }

    /// Placements that never draw (image gone, empty span) don't force
    /// the below-bg full-redraw mode.
    #[test]
    fn below_cell_bg_detection_ignores_placements_that_never_draw() {
        use toastty_term::{ImageData, ImageGrid, ImageRegistry, Placement, SrcRect};
        let place = |id: u32, rows: std::ops::Range<u16>, cols: std::ops::Range<u16>| {
            let mut p = Placement::new(id, rows.end - rows.start, cols.end - cols.start);
            p.line = i64::from(rows.start);
            p.col = i32::from(cols.start);
            p.key = toastty_term::ImageKey(id);
            p.src_rect = SrcRect::FULL;
            p.z = i32::MIN;
            p
        };
        let mut reg = ImageRegistry::<toastty_term::ImageKey>::new(1024);
        reg.insert(
            toastty_term::ImageKey(1),
            ImageData {
                width: 1,
                height: 1,
                pixels: vec![0; 4],
            },
        )
        .unwrap();
        let mut grid = ImageGrid::new();
        grid.add(place(2, 0..1, 0..1)); // image 2 not in the registry
        grid.add(place(1, 0..0, 0..1)); // no rows
        grid.add(place(1, 0..1, 3..3)); // no cols
        assert!(!has_below_cell_bg_placements(&grid, &reg));
        grid.add(place(1, 0..1, 0..1));
        assert!(has_below_cell_bg_placements(&grid, &reg));
    }

    /// The forced full redraw holds while the band is active and for one
    /// frame after, then stops.
    #[test]
    fn below_bg_full_redraw_lasts_one_frame_past_the_image() {
        let mut drawn_last = false;
        assert!(!below_bg_forces_full_redraw(false, &mut drawn_last));
        assert!(below_bg_forces_full_redraw(true, &mut drawn_last));
        assert!(below_bg_forces_full_redraw(true, &mut drawn_last));
        assert!(
            below_bg_forces_full_redraw(false, &mut drawn_last),
            "cleanup frame"
        );
        assert!(!below_bg_forces_full_redraw(false, &mut drawn_last));
    }

    /// The hidden cursor (the full build's last instance) is popped.
    #[test]
    fn finish_full_build_pops_hidden_cursor() {
        use crate::text::instance::{FLAG_CURSOR, Theme, build_instances};
        let theme = Theme::default_dark();
        let mut term = Term::new(1, 6, 0);
        let mut parser = toastty_parser::Parser::new();
        parser.advance(&mut term, b"ab\x1b[41mcd\x1b[0m");
        let built = build_instances(&term, (10.0, 20.0), &theme, None, |_, _, _, _| None);
        let has_cursor = |v: &[CellInstance]| v.iter().any(|i| i.flags & FLAG_CURSOR != 0);

        let mut v = built.clone();
        finish_full_build(&mut v, true);
        assert_eq!(v, built);
        assert!(has_cursor(&v));

        let mut v = built.clone();
        finish_full_build(&mut v, false);
        assert_eq!(v.len(), built.len() - 1);
        assert!(!has_cursor(&v));
    }
}
