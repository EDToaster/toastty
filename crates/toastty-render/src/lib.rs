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

pub mod color;
pub mod image;
pub mod rgp;
pub mod surface_format;
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

use crate::image::atlas::ImageTextureCache;
use crate::image::instance::{ImageInstance, build_image_instances, split_layers};
use crate::image::pipeline::ImagePipeline;
use crate::rgp::{GpuAssetCache, Rgp3dPipeline};
use crate::text::glyph_rasterizer::{DEFAULT_LINE_HEIGHT_RATIO, GlyphRasterizer, LineGlyphs};
use crate::text::instance::{CellInstance, Theme};
use crate::text::pipeline::{GlobalsUbo, TextPipeline};

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
    row_glyphs: &[Option<LineGlyphs>],
    bleed: EdgeBleed<'_>,
    content_h: f32,
) {
    crate::text::instance::build_instances_into(
        out,
        term,
        cell_size,
        theme,
        Some(ext_palette),
        |row, col, ch, _style| {
            let lg = row_glyphs.get(row as usize)?.as_ref()?;
            lg.get(col, ch)
        },
        |line_id, col| term.is_cell_selected(line_id, col),
        bleed,
        content_h,
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
    row_glyphs: &[Option<LineGlyphs>],
    bleed: EdgeBleed<'_>,
) {
    crate::text::instance::build_dirty_instances_into(
        out,
        term,
        term.damage(),
        cell_size,
        theme,
        Some(ext_palette),
        cursor_visible,
        |row, col, ch, _style| {
            let lg = row_glyphs.get(row as usize)?.as_ref()?;
            lg.get(col, ch)
        },
        |line_id, col| term.is_cell_selected(line_id, col),
        bleed,
    );
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
    /// Theme used by `render_term` when emitting instances.
    theme: Theme,
    /// True when the next frame must use `LoadOp::Clear` instead of
    /// `LoadOp::Load`. Set on construction (so the very first frame
    /// fully paints), on resize / theme / font swap, and whenever the
    /// term reports `damage.all` (M8 corrective-flush path).
    needs_full_clear: bool,
    /// Cursor blink state machine.
    blink: CursorBlink,
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
    /// CPU-side mirror of the texture cache.
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
    /// Optional close-confirmation dialog body painted every frame as a
    /// centered, bordered, padded "window" box over a dimmed full-viewport
    /// backdrop (see [`Self::draw_close_dialog`]). Embedded `\n`s split the
    /// body into rows. Used by the binary when the user tries to close the
    /// window while a program is still running, to surface a confirm prompt
    /// without tearing down the session. When `Some`, bypasses the
    /// skip-submit gate so the dialog refreshes even on an idle grid (same
    /// as `error_banner`).
    close_prompt: Option<String>,
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
    /// enabled, the button is painted in `corner` each frame the view is
    /// scrolled back (see [`Self::draw_scroll_button`]).
    scroll_button: Option<ScrollButtonCorner>,
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
/// axis, and `lead = rem * leading_fraction` is the share assigned to the
/// leading (left/top) edge. A non-positive cell dimension yields zero
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

struct TextState {
    rasterizer: GlyphRasterizer,
    pipeline: TextPipeline,
    bind_group: wgpu::BindGroup,
    /// Row-shape cache. `line_cache[r]` holds the shaped glyphs for row
    /// `r` of the active grid; `None` slots are re-shaped on the next
    /// frame. Sized to match the term's current visible-row count;
    /// resized in `render_term` on geometry change.
    ///
    /// This is the minimum-viable subset of decision #7 / M9 needed to
    /// kill the per-keystroke render cost — shape only dirty rows, reuse
    /// cached glyphs for the rest.
    line_cache: Vec<Option<LineGlyphs>>,
    /// Reusable instance buffer. Cleared (not freed) at the start of
    /// every `render_term` so the allocation survives across frames.
    instances_scratch: Vec<CellInstance>,
    /// Reusable per-row text buffer. `render_term` clears + repopulates
    /// this for each dirty row instead of allocating a fresh `String`,
    /// which previously showed up in profile traces as per-row alloc
    /// churn (one allocation per dirty row, every frame).
    line_text_scratch: String,
}

impl std::fmt::Debug for TextState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextState")
            .field("rasterizer", &self.rasterizer)
            .field("pipeline", &self.pipeline)
            .finish_non_exhaustive()
    }
}

/// Compose the text rows of the close-confirmation dialog box: a rounded
/// border around the `message` lines, with `pad_x` cells of horizontal
/// padding and `pad_y` blank rows of vertical padding inside the border.
/// Box width is sized from the widest message line. One `char` == one
/// column (the dialog text is ASCII + box-drawing glyphs, all width 1).
fn compose_dialog_rows(message: &str, pad_x: usize, pad_y: usize) -> Vec<String> {
    let content: Vec<&str> = message.lines().collect();
    let content_cols = content.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let interior_cols = content_cols + 2 * pad_x;
    let bar: String = "─".repeat(interior_cols);
    let blank_interior = format!("│{}│", " ".repeat(interior_cols));

    let mut rows: Vec<String> = Vec::with_capacity(content.len() + 2 * pad_y + 2);
    rows.push(format!("╭{bar}╮"));
    for _ in 0..pad_y {
        rows.push(blank_interior.clone());
    }
    for line in &content {
        let right = interior_cols - pad_x - line.chars().count();
        rows.push(format!(
            "│{}{line}{}│",
            " ".repeat(pad_x),
            " ".repeat(right)
        ));
    }
    for _ in 0..pad_y {
        rows.push(blank_interior.clone());
    }
    rows.push(format!("╰{bar}╯"));
    rows
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
            // First frame after construction always clears: the scratch
            // texture's initial contents are undefined.
            needs_full_clear: true,
            blink: CursorBlink::new(Instant::now()),
            ext_palette: Box::new([[0.0, 0.0, 0.0, 1.0]; 256]),
            palette_revision_seen: u32::MAX,
            scratch_texture,
            scratch_view,
            image_pipeline: None,
            image_tex_cache: ImageTextureCache::new(ImageTextureCache::DEFAULT_MAX_ACTIVE),
            image_revision_seen: u32::MAX,
            image_instances_below_bg: Vec::new(),
            image_instances_below: Vec::new(),
            image_instances_above: Vec::new(),
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
            preedit: None,
            trace_render: std::env::var_os("TOASTTY_TRACE_RENDER").is_some(),
            // The very first frame is always a full clear → goes
            // through the direct-to-swapchain path → leaves scratch
            // untouched. So scratch is "stale" from the start. Subsequent
            // frames pick up the cascade rule.
            scratch_stale: true,
            scroll_button: None,
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
            line_cache: Vec::new(),
            instances_scratch: Vec::new(),
            line_text_scratch: String::new(),
        });
        // Lazy-init the image pipeline alongside the text pipeline so
        // both are ready before the first `render_term`. The image
        // pipeline shares the swapchain format.
        if self.image_pipeline.is_none() {
            self.image_pipeline = Some(ImagePipeline::new(&self.device, self.config.format));
        }
        // Same for the RGP pipeline.
        if self.rgp_pipeline.is_none() {
            self.rgp_pipeline = Some(Rgp3dPipeline::new(&self.device, self.config.format));
        }
        // Font swap invalidates the cell grid — force the next frame
        // to clear.
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
        self.theme = theme;
        self.clear_color = theme.bg;
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
        // Whichever fires first wins. Both are `Option<Duration>`.
        match (blink, rgp) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
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

    /// Set (or clear) a multi-line full-width close-confirmation banner
    /// painted vertically centered on the surface. Lines are split on
    /// `\n`. While `Some`, the renderer skips its damage-only
    /// short-circuit so the prompt remains visible on an idle grid.
    pub fn set_close_prompt(&mut self, text: Option<&str>) {
        match text {
            Some(s) => {
                if let Some(buf) = self.close_prompt.as_mut() {
                    buf.clear();
                    buf.push_str(s);
                } else {
                    self.close_prompt = Some(s.to_owned());
                }
                self.needs_full_clear = true;
            }
            None => {
                if self.close_prompt.is_some() {
                    self.close_prompt = None;
                    self.needs_full_clear = true;
                }
            }
        }
    }

    /// True when a close-confirmation prompt is currently set.
    #[must_use]
    pub fn has_close_prompt(&self) -> bool {
        self.close_prompt.is_some()
    }

    /// Enable (`Some(corner)`) or disable (`None`) the scroll-to-bottom
    /// button. When enabled, the button is painted in `corner` on any
    /// frame where the view is scrolled back into the scrollback. Cheap to
    /// call every config reload; it just stores the corner.
    pub fn set_scroll_button(&mut self, corner: Option<ScrollButtonCorner>) {
        self.scroll_button = corner;
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
    /// error banner. (The close-confirmation dialog uses the centered,
    /// bordered [`Self::draw_close_dialog`] instead.) `width`/`height` are
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

    /// Append instances for the centered close-confirmation dialog: a
    /// dimmed full-viewport backdrop with a rounded, padded "window" box
    /// floating in the middle. Distinct from [`Self::draw_banner`] (the
    /// full-width error banner) — this is a fixed-width box centered on
    /// both axes, with a border and interior padding, so the "program
    /// still running" prompt reads as a modal dialog.
    ///
    /// `message` is the dialog body; embedded `\n`s split it into rows (an
    /// empty line renders as a blank spacer row). The renderer wraps the
    /// rows in a rounded border and adds horizontal/vertical padding sized
    /// from the widest row.
    ///
    /// Layout assumes one column per `char` (no wide-char handling), same
    /// as [`Self::draw_banner`] — fine for the ASCII + box-drawing dialog
    /// text. The per-cell two-cover-quad technique is also shared with
    /// `draw_banner` (see its docs); here it fills the box panel so the
    /// term glyphs the dirty builder emitted underneath are overpainted.
    /// The single translucent quad pushed first is the dim backdrop:
    /// `FLAG_UNDERLINE | FLAG_NO_GLYPH` routes it through the glyph pass
    /// only (discarded in the bg pass) and emits premultiplied `bg`, which
    /// the glyph pass's over-blend composites as a multiply-darken of
    /// whatever the term/RGP passes drew underneath.
    #[allow(
        clippy::too_many_arguments,
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::similar_names
    )]
    fn draw_close_dialog(
        queue: &Queue,
        // Full physical surface size — the scrim dims the WHOLE window.
        surface_w: u32,
        surface_h: u32,
        // Content area size — the box is centered on the content.
        content_w: u32,
        content_h: u32,
        // Content origin (physical px): the scrim is emitted in pre-origin
        // space at `-origin` so that after the content_origin uniform adds
        // `+origin` it spans `[0,0]..[surface_w, surface_h]`.
        origin: [f32; 2],
        instances: &mut Vec<CellInstance>,
        text: &mut TextState,
        message: &str,
        scrim: [f32; 4],
        panel_bg: [f32; 4],
        fg: [f32; 4],
        cell_size: (f32, f32),
        term: &Term,
    ) {
        /// Interior horizontal padding, in cells, on each side of the text.
        const PAD_X: usize = 3;
        /// Interior vertical padding, in blank rows, above and below the text.
        const PAD_Y: usize = 1;

        let surface_wf = surface_w as f32;
        let surface_hf = surface_h as f32;
        let content_wf = content_w as f32;
        let content_hf = content_h as f32;
        let cell_w = cell_size.0;
        let cell_h = cell_size.1;

        // Dim backdrop: one translucent black quad over the WHOLE window
        // (not just the content area) so the gutter is dimmed too — a
        // content-only scrim would leave bled edge colors at full
        // brightness around a dimmed modal. Emit pre-origin at `-origin`
        // so the content_origin uniform's `+origin` lands it at the full
        // surface `[0,0]..[surface_w, surface_h]`.
        instances.push(CellInstance {
            pos: [-origin[0], -origin[1]],
            size: [surface_wf, surface_hf],
            uv_min: [0.0, 0.0],
            uv_max: [0.0, 0.0],
            fg,
            bg: scrim,
            flags: crate::text::instance::FLAG_UNDERLINE | crate::text::instance::FLAG_NO_GLYPH,
            pad: [0; 3],
        });

        // Compose the box rows (border + padding + content), sized from
        // the widest content line. The top border row is exactly the box
        // width, so derive `box_cols` from it. Centered on the CONTENT
        // area (positions flow through the content_origin uniform).
        let rows = compose_dialog_rows(message, PAD_X, PAD_Y);
        let cols = (content_wf / cell_w).floor().max(1.0) as u32;
        let vrows = (content_hf / cell_h).floor().max(1.0) as u32;
        let box_cols = rows.first().map_or(0, |r| r.chars().count()) as u32;
        let box_rows = rows.len() as u32;

        // Center the box; clamp to the viewport's top-left if it's larger.
        let left_col = cols.saturating_sub(box_cols) / 2;
        let top_row = vrows.saturating_sub(box_rows) / 2;
        Self::draw_box_rows(
            queue, instances, text, &rows, left_col, top_row, cols, cell_size, panel_bg, fg, term,
        );
    }

    /// Paint a pre-composed block of box rows (border + content) at cell
    /// position `(left_col, top_row)`. Each cell gets the banner's two
    /// cover quads (bg pass + glyph-pass cover) filled with `panel_bg`, and
    /// each row's glyphs are drawn in `fg` on top. `grid_cols` clips any
    /// cell that would run past the right edge. The box width is taken from
    /// the first row (every row is the same width). Shared by
    /// [`Self::draw_close_dialog`] and [`Self::draw_scroll_button`]; the
    /// per-cell two-cover-quad rationale lives in [`Self::draw_banner`].
    #[allow(clippy::too_many_arguments, clippy::cast_precision_loss)]
    fn draw_box_rows(
        queue: &Queue,
        instances: &mut Vec<CellInstance>,
        text: &mut TextState,
        rows: &[String],
        left_col: u32,
        top_row: u32,
        grid_cols: u32,
        cell_size: (f32, f32),
        panel_bg: [f32; 4],
        fg: [f32; 4],
        term: &Term,
    ) {
        let cell_w = cell_size.0;
        let cell_h = cell_size.1;
        let box_cols = rows.first().map_or(0, |r| r.chars().count()) as u32;
        for (row_idx, line) in rows.iter().enumerate() {
            let y0 = (top_row as f32 + row_idx as f32) * cell_h;
            // Panel fill: bg-pass + glyph-pass cover per box cell so any
            // term glyph underneath is overpainted.
            for c in 0..box_cols {
                if left_col + c >= grid_cols {
                    break;
                }
                let x0 = (left_col + c) as f32 * cell_w;
                instances.push(CellInstance {
                    pos: [x0, y0],
                    size: [cell_w, cell_h],
                    uv_min: [0.0, 0.0],
                    uv_max: [0.0, 0.0],
                    fg,
                    bg: panel_bg,
                    flags: crate::text::instance::FLAG_NO_GLYPH,
                    pad: [0; 3],
                });
                instances.push(CellInstance {
                    pos: [x0, y0],
                    size: [cell_w, cell_h],
                    uv_min: [0.0, 0.0],
                    uv_max: [0.0, 0.0],
                    fg,
                    bg: panel_bg,
                    flags: crate::text::instance::FLAG_UNDERLINE
                        | crate::text::instance::FLAG_NO_GLYPH,
                    pad: [0; 3],
                });
            }
            // Glyphs for this row. The shaper keys slots by the line-local
            // column (0-based), so look up with the local index and place
            // at the offset column.
            let lg = text
                .rasterizer
                .shape_line(queue, line, term.grapheme_cluster_mode());
            for (i, ch) in line.chars().enumerate() {
                let col = i as u32;
                if col >= box_cols || left_col + col >= grid_cols {
                    break;
                }
                if ch.is_whitespace() {
                    continue;
                }
                let x0 = (left_col + col) as f32 * cell_w;
                if let Some(slot) = lg.get(col as u16, ch) {
                    let flags = if slot.is_color {
                        crate::text::instance::FLAG_COLOR_GLYPH
                    } else {
                        0
                    };
                    instances.push(CellInstance {
                        pos: [x0 + slot.glyph_offset[0], y0 + slot.glyph_offset[1]],
                        size: slot.glyph_size,
                        uv_min: slot.uv_min,
                        uv_max: slot.uv_max,
                        fg,
                        bg: panel_bg,
                        flags,
                        pad: [0; 3],
                    });
                }
            }
        }
    }

    /// Scroll-to-bottom button box dimensions, in cells — a 5×3 rounded
    /// box (`╭───╮` / `│ ↓ │` / `╰───╯`) — and its margin from the
    /// viewport edge.
    const SCROLL_BTN_COLS: u32 = 5;
    const SCROLL_BTN_ROWS: u32 = 3;
    const SCROLL_BTN_MARGIN: u32 = 1;

    /// Cell-space top-left `(left_col, top_row)` of the scroll-to-bottom
    /// button for the given viewport pixels + corner. `None` when the
    /// viewport is too small to fit the box plus its margins.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn scroll_button_cell_origin(
        width: u32,
        height: u32,
        cell_size: (f32, f32),
        corner: ScrollButtonCorner,
    ) -> Option<(u32, u32)> {
        let (cell_w, cell_h) = cell_size;
        if cell_w <= 0.0 || cell_h <= 0.0 {
            return None;
        }
        let cols = (width as f32 / cell_w).floor() as u32;
        let rows = (height as f32 / cell_h).floor() as u32;
        if cols < Self::SCROLL_BTN_COLS + 2 * Self::SCROLL_BTN_MARGIN
            || rows < Self::SCROLL_BTN_ROWS + Self::SCROLL_BTN_MARGIN
        {
            return None;
        }
        let top_row = rows - Self::SCROLL_BTN_ROWS - Self::SCROLL_BTN_MARGIN;
        let left_col = match corner {
            ScrollButtonCorner::BottomRight => {
                cols - Self::SCROLL_BTN_COLS - Self::SCROLL_BTN_MARGIN
            }
            ScrollButtonCorner::BottomLeft => Self::SCROLL_BTN_MARGIN,
        };
        Some((left_col, top_row))
    }

    /// Pixel-space rect `[x0, y0, x1, y1]` of the scroll-to-bottom button
    /// when it is currently visible — i.e. enabled via
    /// [`Self::set_scroll_button`] AND `term` is scrolled back into the
    /// scrollback. `None` otherwise. The binary uses this to hit-test
    /// mouse clicks against the button.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn scroll_button_rect(&self, term: &Term) -> Option<[f32; 4]> {
        let corner = self.scroll_button?;
        if !term.is_view_scrolled_back() {
            return None;
        }
        let cell_size = self.cell_size();
        // The button anchors to the content corner (the painted positions
        // flow through the content_origin uniform). This rect is
        // physical-space (hit-tested against raw mouse px), so bake the
        // origin in here on the CPU.
        let (cdw, cdh) = self.content_dims(cell_size);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (left_col, top_row) =
            Self::scroll_button_cell_origin(cdw as u32, cdh as u32, cell_size, corner)?;
        let (cw, ch) = cell_size;
        let origin = self.content_origin();
        let x0 = left_col as f32 * cw + origin[0];
        let y0 = top_row as f32 * ch + origin[1];
        Some([
            x0,
            y0,
            x0 + Self::SCROLL_BTN_COLS as f32 * cw,
            y0 + Self::SCROLL_BTN_ROWS as f32 * ch,
        ])
    }

    /// Append instances for the scroll-to-bottom button anchored at
    /// `corner`. The caller is responsible for deciding visibility (config
    /// enabled + view scrolled back); this just paints the box. Reuses
    /// [`compose_dialog_rows`] to build the `↓`-in-a-rounded-box rows and
    /// [`Self::draw_box_rows`] to paint them.
    #[allow(
        clippy::too_many_arguments,
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn draw_scroll_button(
        queue: &Queue,
        width: u32,
        height: u32,
        instances: &mut Vec<CellInstance>,
        text: &mut TextState,
        corner: ScrollButtonCorner,
        panel_bg: [f32; 4],
        fg: [f32; 4],
        cell_size: (f32, f32),
        term: &Term,
    ) {
        let Some((left_col, top_row)) =
            Self::scroll_button_cell_origin(width, height, cell_size, corner)
        else {
            return;
        };
        let grid_cols = (width as f32 / cell_size.0).floor().max(1.0) as u32;
        // "↓" in a 3-wide rounded box → ["╭───╮", "│ ↓ │", "╰───╯"].
        let rows = compose_dialog_rows("↓", 1, 0);
        Self::draw_box_rows(
            queue, instances, text, &rows, left_col, top_row, grid_cols, cell_size, panel_bg, fg,
            term,
        );
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
        self.theme
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
        // advanced, re-sync GPU textures and force a full clear for
        // this frame (the pragmatic shortcut from the milestone plan —
        // partial redraw with images is not supported in M11a).
        if term.image_revision() != self.image_revision_seen
            && let Some(image_pipeline) = self.image_pipeline.as_mut()
        {
            image_pipeline.sync_registry(
                &self.device,
                &self.queue,
                term.image_registry(),
                &mut self.image_tex_cache,
            );
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
        // M12c: RGP animations force frames through the skip-submit
        // gate the same way cursor blink does. Tick BEFORE the
        // skip check so animation_phase_rad is current for this
        // frame; the tick itself doesn't bump revision.
        let rgp_animation_active = term.rgp_scene().has_active_animations();
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
        if term.damage().is_empty()
            && !cursor_animation_due
            && !rgp_animation_active
            && !self.needs_full_clear
            && self.debug_overlay.is_none()
            && self.error_banner.is_none()
            && self.close_prompt.is_none()
            && self.preedit.is_none()
        {
            return Ok(RenderOutcome::Skipped);
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
        {
            let text = self.text.as_mut().expect("text initialised above");
            cell_size = text.rasterizer.cell_size();
            atlas_dims = text.rasterizer.atlas_dims();

            // Resize the row-shape cache. We allocate two extra slots
            // beyond `rows`: one for the partial *top* row during sub-pixel
            // scrolling, one for the partial *bottom* row when the content
            // area isn't a whole number of cells tall. Sizing for the worst
            // case avoids reallocation churn as the scroll offset crosses
            // cell boundaries. Growth is dirty (new entries are `None`);
            // shrinking just drops old slots.
            let cache_len = rows as usize + 2;
            if text.line_cache.len() != cache_len {
                text.line_cache.resize(cache_len, None);
            }

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

            // Re-shape only dirty rows; reuse cached `LineGlyphs` for
            // the rest. The atlas itself never shrinks, so a clean row's
            // glyph slots stay valid across frames.
            let damage = term.damage();
            let mut shaped = 0u32;
            let t_shape = if trace {
                Some(std::time::Instant::now())
            } else {
                None
            };
            for r in 0..rows_rendered {
                let is_dirty = damage.all
                    || damage.rows.get(r as usize).is_some_and(|rd| !rd.is_empty())
                    || text.line_cache[r as usize].is_none();
                if !is_dirty {
                    continue;
                }
                let row = term.view_row(r);
                // Reuse the cross-row scratch String instead of
                // allocating a fresh one per dirty row.
                //
                // Continuation cells are excluded: they're the second
                // half of a width-2 cluster whose primary cell already
                // contributes its full multi-cell glyph to the shaper.
                // Feeding the continuation in as a space would insert
                // an extra glyph cosmic-text would shape, shifting every
                // downstream cluster's snapped column by one.
                text.line_text_scratch.clear();
                for c in &row.cells {
                    if c.is_continuation {
                        continue;
                    }
                    // Replace Kitty Unicode placeholder cells
                    // (U+10EEEE) with a space before shaping. The
                    // codepoint shapes to `.notdef` and its
                    // cluster width can throw off the snap of
                    // neighboring chars — we don't want it in the
                    // glyph cache and the image pipeline draws
                    // the real pixels anyway.
                    let ch = if c.ch == '\0' || c.ch == toastty_term::PLACEHOLDER {
                        ' '
                    } else {
                        c.ch
                    };
                    text.line_text_scratch.push(ch);
                }
                let lg = text.rasterizer.shape_line(
                    &self.queue,
                    &text.line_text_scratch,
                    term.grapheme_cluster_mode(),
                );
                text.line_cache[r as usize] = Some(lg);
                shaped += 1;
            }
            if let Some(t) = t_shape {
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                tracing::info!(target: "render_trace", "shape rows={shaped} took={ms:.3}ms");
            }
            // make_bind_group used to be rebuilt every frame "in case
            // the atlas textures grew". They don't grow inside the
            // render path — `queue.write_texture` makes uploads visible
            // through the existing view. So we keep the bind group from
            // `with_font_ex` until either resize or font swap rebuilds
            // it for real.
        }

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
                        .cells
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
        let surface_dims = (self.config.width, self.config.height);
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
                &text.line_cache,
                bleed,
                content_dims.1,
            );
            if !cursor_visible {
                instances.pop();
            }
        } else {
            build_term_dirty_instances_into(
                &mut instances,
                term,
                cell_size,
                &theme,
                ext_palette,
                cursor_visible,
                &text.line_cache,
                bleed,
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

        // Scroll-to-bottom button: a small rounded box in a corner, shown
        // only while the view is scrolled up into the scrollback. Drawn
        // before the error banner / close dialog so a modal scrim dims it.
        // The chip background is the theme bg nudged toward fg so it reads
        // as a raised affordance against the grid; the arrow + border use
        // the theme fg.
        if let Some(corner) = self.scroll_button
            && term.is_view_scrolled_back()
        {
            let mix = |a: f32, b: f32, k: f32| a + (b - a) * k;
            let chip = [
                mix(theme.bg[0], theme.fg[0], 0.16),
                mix(theme.bg[1], theme.fg[1], 0.16),
                mix(theme.bg[2], theme.fg[2], 0.16),
                0.95,
            ];
            // Anchor the button to the content corner: pass content dims
            // (the positions flow through the content_origin uniform).
            let (cdw, cdh) = content_dims;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            Self::draw_scroll_button(
                &self.queue,
                cdw as u32,
                cdh as u32,
                &mut instances,
                text,
                corner,
                chip,
                theme.fg,
                cell_size,
                term,
            );
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

        // Close-confirmation dialog: a centered, rounded, padded "window"
        // box floating over a dimmed full-viewport backdrop, painted in a
        // dark-slate panel (distinct from the error banner's red) so the
        // "program still running" prompt reads as a modal dialog rather
        // than an error. Drawn after the error banner — the binary only
        // sets one of the two at a time, but no ordering is assumed.
        if let Some(prompt) = self.close_prompt.as_deref() {
            // 50% multiply-darken behind the dialog (premultiplied black).
            let scrim = [0.0, 0.0, 0.0, 0.5];
            let panel_bg = [0.12, 0.14, 0.20, 0.98];
            let panel_fg = [0.95, 0.96, 0.98, 1.0];
            // Scrim covers the FULL surface; the box centers on content.
            let (cdw, cdh) = content_dims;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            Self::draw_close_dialog(
                &self.queue,
                surface_dims.0,
                surface_dims.1,
                cdw as u32,
                cdh as u32,
                content_origin,
                &mut instances,
                text,
                prompt,
                scrim,
                panel_bg,
                panel_fg,
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

        // M11a: build image instances split by z sign. We clear the
        // existing vecs and rebuild every frame — the count of images
        // is small (<= 14) so this is cheap.
        self.image_instances_below_bg.clear();
        self.image_instances_below.clear();
        self.image_instances_above.clear();
        if !term.image_grid().is_empty() {
            let mut tmp: Vec<ImageInstance> = Vec::new();
            build_image_instances(
                &mut tmp,
                term.image_grid(),
                term.image_registry(),
                &self.image_tex_cache,
                cell_size,
            );
            let (below_bg, below, above) = split_layers(&tmp);
            self.image_instances_below_bg.extend_from_slice(below_bg);
            self.image_instances_below.extend_from_slice(below);
            self.image_instances_above.extend_from_slice(above);
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
            // Z-ordering between 3D and {glyphs, images} is handled by
            // the per-placement `depth` field: objects with
            // `depth < 0` win the depth test against the z=0.5 cell
            // layer, `depth > 0` lose to it. Cell bg is always behind.
            #[allow(clippy::cast_precision_loss)] // viewport in 16k-px range.
            let viewport = (self.config.width as f32, self.config.height as f32);

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
            let inst_count = instances.len();
            text_ref
                .pipeline
                .render_bg(&mut rp, &text_ref.bind_group, inst_count);

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

    /// The close-confirmation dialog box is a well-formed rectangle: every
    /// row is the same column width, the corners are rounded box-drawing
    /// glyphs, and the message is inset by the requested padding.
    #[test]
    fn compose_dialog_rows_builds_padded_rounded_box() {
        let pad_x = 3;
        let pad_y = 1;
        let rows = compose_dialog_rows("hello\n\nworld!", pad_x, pad_y);

        // border + pad_y blanks + 3 content rows + pad_y blanks + border.
        assert_eq!(rows.len(), 2 + 2 * pad_y + 3);

        // Width is the widest line ("world!" = 6) + 2*pad_x interior +
        // 2 border columns. All rows share that width (counted in chars,
        // since each box glyph occupies one column).
        let width = 6 + 2 * pad_x + 2;
        for r in &rows {
            assert_eq!(r.chars().count(), width, "row not full width: {r:?}");
        }

        // Rounded corners on the first/last rows.
        assert!(rows[0].starts_with('╭') && rows[0].ends_with('╮'));
        let last = rows.last().unwrap();
        assert!(last.starts_with('╰') && last.ends_with('╯'));

        // Side borders + horizontal padding on a content row.
        let first_content = &rows[1 + pad_y];
        assert!(first_content.starts_with(&format!("│{}hello", " ".repeat(pad_x))));
        assert!(first_content.ends_with('│'));
    }

    /// The scroll-button corner math anchors the 5×3 box one cell in from
    /// the chosen corner, and bails out when the viewport is too small.
    #[test]
    fn scroll_button_origin_anchors_to_corner() {
        // 800×400 with 10×20 cells → 80 cols, 20 rows.
        let cell = (10.0, 20.0);
        // top_row = rows - BTN_ROWS(3) - MARGIN(1) = 16 for both corners.
        let br =
            Renderer::scroll_button_cell_origin(800, 400, cell, ScrollButtonCorner::BottomRight);
        // left = cols - BTN_COLS(5) - MARGIN(1) = 74.
        assert_eq!(br, Some((74, 16)));
        let bl =
            Renderer::scroll_button_cell_origin(800, 400, cell, ScrollButtonCorner::BottomLeft);
        // left = MARGIN(1).
        assert_eq!(bl, Some((1, 16)));

        // Too few columns to fit box + margins → None.
        assert!(
            Renderer::scroll_button_cell_origin(30, 400, cell, ScrollButtonCorner::BottomRight)
                .is_none()
        );
        // Degenerate cell size → None (no divide-by-zero blowup).
        assert!(
            Renderer::scroll_button_cell_origin(
                800,
                400,
                (0.0, 0.0),
                ScrollButtonCorner::BottomRight
            )
            .is_none()
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
        // Centered: half the leftover leads, half trails.
        assert!((lead[0] - 2.5).abs() < 1e-3, "lead_x");
        assert!((lead[1] - 2.5).abs() < 1e-3, "lead_y");
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
