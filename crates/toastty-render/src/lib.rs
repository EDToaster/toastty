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

use crate::text::glyph_rasterizer::{GlyphRasterizer, LineGlyphs, DEFAULT_LINE_HEIGHT_RATIO};
use crate::text::instance::{build_instances, CellInstance, Theme};
use crate::text::pipeline::{GlobalsUbo, TextPipeline};

fn build_term_instances(
    term: &Term,
    cell_size: (f32, f32),
    theme: &Theme,
    row_glyphs: &[LineGlyphs],
) -> Vec<CellInstance> {
    build_instances(term, cell_size, theme, |row, col, ch, _style| {
        let lg = row_glyphs.get(row as usize)?;
        lg.by_column.get(&(col, ch)).copied()
    })
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
    pub fn with_font_ex(
        &mut self,
        font_name: Option<&str>,
        font_size_px: f32,
        line_height: f32,
    ) {
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
    /// TODO(damage): full-frame redraw for now; the dirty-set integration
    /// lands in M5 (decision §7).
    pub fn render_term(&mut self, term: &Term) -> Result<(), RenderError> {
        if self.text.is_none() {
            return Err(RenderError::FontNotConfigured);
        }

        // Shape every row up front so atlas slots are populated before
        // we touch the borrow on `self.text` for the render pass.
        let (rows, _) = term.size();
        let cell_size;
        let atlas_dims;
        let mut row_glyphs: Vec<text::glyph_rasterizer::LineGlyphs> =
            Vec::with_capacity(rows as usize);
        {
            let text = self.text.as_mut().expect("text initialised above");
            cell_size = text.rasterizer.cell_size();
            atlas_dims = text.rasterizer.atlas_dims();
            for r in 0..rows {
                let row = term.row(r);
                let line_text: String = row
                    .cells
                    .iter()
                    .map(|c| if c.ch == '\0' { ' ' } else { c.ch })
                    .collect();
                let lg = text.rasterizer.shape_line(&self.queue, &line_text);
                row_glyphs.push(lg);
            }
            // Rebuild bind group in case the atlas textures grew (M4b
            // they don't, but writes through `queue.write_texture` are
            // visible without rebinding so this is precautionary).
            let mask_view = text::pipeline::default_view(text.rasterizer.mask_texture());
            let color_view = text::pipeline::default_view(text.rasterizer.color_texture());
            text.bind_group =
                text.pipeline
                    .make_bind_group(&self.device, &mask_view, &color_view);
        }

        let theme = self.theme;
        let instances = build_term_instances(term, cell_size, &theme, &row_glyphs);

        // Acquire surface frame.
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Validation => {
                return Err(RenderError::SurfaceLost);
            }
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

        self.queue.submit(std::iter::once(encoder.finish()));
        frame.present();
        Ok(())
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
}
