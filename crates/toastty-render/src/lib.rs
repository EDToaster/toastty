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
pub mod surface_format;
pub mod text;

use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use thiserror::Error;
use toastty_term::Term;
use wgpu::{
    BackendOptions, Backends, CompositeAlphaMode, Device, DeviceDescriptor, Instance,
    InstanceDescriptor, InstanceFlags, MemoryBudgetThresholds, PowerPreference, PresentMode, Queue,
    RequestAdapterOptions, Surface, SurfaceConfiguration, TextureFormat, TextureUsages,
};

use crate::text::glyph_rasterizer::{DEFAULT_LINE_HEIGHT_RATIO, GlyphRasterizer, LineGlyphs};
use crate::text::instance::{CellInstance, Theme};
use crate::text::pipeline::{GlobalsUbo, TextPipeline};

/// Append instances for `term` into `out`. The closure pulls glyph
/// slots from the line cache; missing entries fall through to a
/// background-only instance (the next frame, after re-shape, will fill
/// in the glyph).
fn build_term_instances_into(
    out: &mut Vec<CellInstance>,
    term: &Term,
    cell_size: (f32, f32),
    theme: &Theme,
    row_glyphs: &[Option<LineGlyphs>],
) {
    crate::text::instance::build_instances_into(out, term, cell_size, theme, |row, col, ch, _style| {
        let lg = row_glyphs.get(row as usize)?.as_ref()?;
        lg.by_column.get(&(col, ch)).copied()
    });
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
}

impl std::fmt::Debug for TextState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextState")
            .field("rasterizer", &self.rasterizer)
            .field("pipeline", &self.pipeline)
            .finish_non_exhaustive()
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
    pub async fn new<W>(window: W, size: (u32, u32)) -> Result<Self, RenderError>
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
            usage: TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.0.max(1),
            height: size.1.max(1),
            // Fifo for power friendliness per the spec. Mailbox is a
            // future config flag; see TODO below.
            // TODO(mailbox): expose a config knob for Mailbox vs Fifo.
            present_mode: PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps
                .alpha_modes
                .iter()
                .copied()
                .find(|m| *m == CompositeAlphaMode::Opaque)
                .unwrap_or(caps.alpha_modes[0]),
            view_formats: vec![],
        };

        surface.configure(&device, &config);

        Ok(Self {
            device,
            queue,
            surface,
            config,
            clear_color: [0.07, 0.07, 0.09, 1.0],
            text: None,
            theme: Theme::default_dark(),
        })
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
    /// `line_height` is `× font_size_px`. The renderer's snapshots were
    /// captured at [`DEFAULT_LINE_HEIGHT`] (`1.4`). Callers loading a
    /// `toastty_config::FontConfig` should pass `font.line_height` here.
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
        });
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
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
        self.clear_color = theme.bg;
    }

    /// Current theme.
    pub fn theme(&self) -> Theme {
        self.theme
    }

    /// Resize the surface. Width or height of 0 is clamped to 1 (configuring
    /// a zero-sized surface panics in wgpu).
    pub fn resize(&mut self, width: u32, height: u32) {
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        self.surface.configure(&self.device, &self.config);
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
    /// Honors `term.dirty_rows()` for the row-shape cache (decision §7,
    /// minimum-viable subset for M5; full skip-submit / cell-level
    /// damage lands in M9).
    ///
    /// Setting `TOASTTY_TRACE_RENDER=1` in the environment makes this
    /// function emit a per-phase timing line via `tracing::info!`. Use it
    /// to attribute per-frame cost in real workloads (the criterion
    /// bench under `benches/render_term.rs` runs in release mode; this
    /// env var works in the debug build too).
    #[allow(clippy::too_many_lines)] // optional tracing branches add ~30 LoC
    pub fn render_term(&mut self, term: &Term) -> Result<RenderOutcome, RenderError> {
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

        let trace = std::env::var_os("TOASTTY_TRACE_RENDER").is_some();
        let t_total = if trace { Some(std::time::Instant::now()) } else { None };

        let (rows, _) = term.size();
        let cell_size;
        let atlas_dims;
        {
            let text = self.text.as_mut().expect("text initialised above");
            cell_size = text.rasterizer.cell_size();
            atlas_dims = text.rasterizer.atlas_dims();

            // Resize the row-shape cache to match the visible row count.
            // Growth is dirty (new entries are `None`); shrinking just
            // drops old slots.
            if text.line_cache.len() != rows as usize {
                text.line_cache.resize(rows as usize, None);
            }

            // Re-shape only dirty rows; reuse cached `LineGlyphs` for
            // the rest. The atlas itself never shrinks, so a clean row's
            // glyph slots stay valid across frames.
            let dirty = term.dirty_rows();
            let mut shaped = 0u32;
            let t_shape = if trace { Some(std::time::Instant::now()) } else { None };
            for r in 0..rows {
                let is_dirty = dirty.get(r as usize).copied().unwrap_or(true)
                    || text.line_cache[r as usize].is_none();
                if !is_dirty {
                    continue;
                }
                let row = term.row(r);
                // Reuse a per-call String allocation; under release LLVM
                // hoists the small allocation per row, but a future
                // optimization could move the buffer into TextState.
                //
                // Continuation cells are excluded: they're the second
                // half of a width-2 cluster whose primary cell already
                // contributes its full multi-cell glyph to the shaper.
                // Feeding the continuation in as a space would insert
                // an extra glyph cosmic-text would shape, shifting every
                // downstream cluster's snapped column by one.
                let line_text: String = row
                    .cells
                    .iter()
                    .filter(|c| !c.is_continuation)
                    .map(|c| if c.ch == '\0' { ' ' } else { c.ch })
                    .collect();
                let lg = text.rasterizer.shape_line(
                    &self.queue,
                    &line_text,
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

        let theme = self.theme;
        // Build instances using the cached row glyphs. Reuse the
        // scratch vec across frames. We have to temporarily extract
        // the scratch out of TextState because `build_term_instances_into`
        // needs to read `text.line_cache` immutably while writing to the
        // scratch; can't hold two borrows of TextState at once.
        let text = self.text.as_mut().expect("text init");
        let mut instances = std::mem::take(&mut text.instances_scratch);
        let t_bi = if trace { Some(std::time::Instant::now()) } else { None };
        build_term_instances_into(&mut instances, term, cell_size, &theme, &text.line_cache);
        if let Some(t) = t_bi {
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            tracing::info!(target: "render_trace", "build_instances n={} took={ms:.3}ms", instances.len());
        }

        // Acquire surface frame. This is where `Fifo` present mode
        // blocks waiting for vsync; if any prior frame is still queued,
        // we sit here for ~16.7ms.
        let t_acq = if trace { Some(std::time::Instant::now()) } else { None };
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                // No frame went out — caller must not clear damage / BSU
                // force-flushed flag, so report Skipped.
                return Ok(RenderOutcome::Skipped);
            }
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Validation => {
                return Err(RenderError::SurfaceLost);
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(RenderOutcome::Skipped);
            }
        };
        if let Some(t) = t_acq {
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            tracing::info!(target: "render_trace", "surface_acquire took={ms:.3}ms (blocks on vsync under Fifo)");
        }

        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let t_enc = if trace { Some(std::time::Instant::now()) } else { None };
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("toastty-render encoder (term)"),
            });

        #[allow(clippy::cast_precision_loss)] // viewport/atlas sizes fit comfortably in 24 bits.
        let globals = GlobalsUbo {
            viewport_and_atlas: [
                self.config.width as f32,
                self.config.height as f32,
                atlas_dims.0 as f32,
                atlas_dims.1 as f32,
            ],
        };

        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("toastty-render term pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: f64::from(self.theme.bg[0]),
                            g: f64::from(self.theme.bg[1]),
                            b: f64::from(self.theme.bg[2]),
                            a: f64::from(self.theme.bg[3]),
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            let text = self.text.as_mut().expect("text init checked above");
            text.pipeline.render(
                &self.device,
                &self.queue,
                &mut rp,
                &text.bind_group,
                globals,
                &instances,
            );
        }

        if let Some(t) = t_enc {
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            tracing::info!(target: "render_trace", "encode_pass took={ms:.3}ms");
        }
        let t_sub = if trace { Some(std::time::Instant::now()) } else { None };
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
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: f64::from(self.clear_color[0]),
                            g: f64::from(self.clear_color[1]),
                            b: f64::from(self.clear_color[2]),
                            a: f64::from(self.clear_color[3]),
                        }),
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
}
