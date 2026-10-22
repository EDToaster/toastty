//! Shared test harness for headless text-rendering snapshots.
//!
//! Builds a wgpu device, renders a `Term` into an offscreen RGBA
//! texture, reads pixels back, and produces an `image::RgbaImage`.
//! Also handles golden-PNG comparison via SSIM.

#![allow(dead_code)] // each integration test file uses a subset

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use image::RgbaImage;
use image_compare::Algorithm;
use pollster::block_on;
use toastty_render::text::glyph_rasterizer::GlyphRasterizer;
use toastty_render::text::instance::{EdgeBleed, ExtendCondition, Theme};
use toastty_render::text::pipeline::{self, GlobalsUbo, TextPipeline};
use toastty_render::text::row_shaper::RowShaper;
use toastty_render::{instance_descriptor, instance_flags_for_tests};
use toastty_term::Term;
use wgpu::{
    Color, CommandEncoderDescriptor, DeviceDescriptor, Extent3d, LoadOp, Operations,
    PowerPreference, RenderPassColorAttachment, RenderPassDescriptor, RequestAdapterOptions,
    StoreOp, TexelCopyBufferInfo, TexelCopyBufferLayout, TextureDescriptor, TextureDimension,
    TextureFormat, TextureUsages, TextureViewDescriptor,
};

pub(crate) const SNAPSHOT_DIR: &str = "tests/snapshots";

/// Embedded `FiraMono` — same blob the renderer bundles, so the harness
/// can stay self-contained without going through `Renderer::with_font`
/// (which requires a real window surface).
const TEST_FONT: &[u8] = include_bytes!("../../fonts/FiraMono-Medium.ttf");

/// One device per test binary. Tests in a file run on parallel threads,
/// and creating several wgpu instances/devices at once intermittently
/// segfaults Mesa's software Vulkan driver (lavapipe) on headless CI.
fn shared_device() -> (wgpu::Device, wgpu::Queue) {
    static GPU: OnceLock<(wgpu::Device, wgpu::Queue)> = OnceLock::new();
    GPU.get_or_init(|| {
        let instance = wgpu::Instance::new(instance_descriptor(instance_flags_for_tests()));
        let adapter = block_on(instance.request_adapter(&RequestAdapterOptions {
            power_preference: PowerPreference::LowPower,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .expect("no GPU adapter");
        block_on(adapter.request_device(&DeviceDescriptor {
            label: Some("text-snapshot device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults(),
            memory_hints: wgpu::MemoryHints::Performance,
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            trace: wgpu::Trace::Off,
        }))
        .expect("device request")
    })
    .clone()
}

/// Render `term` into a `width × height` RGBA8 image, headless.
pub(crate) fn render_term_offscreen(term: &Term, width: u32, height: u32) -> RgbaImage {
    render_term_offscreen_with(term, width, height, &Offscreen::default())
}

/// Knobs for [`render_term_offscreen_with`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct Offscreen {
    /// Render target format. RGBA8 (not sRGB) by default so saved PNG
    /// bytes are directly comparable across runs without sRGB-decode
    /// drift; the app's swapchain is usually `Bgra8UnormSrgb`.
    pub format: TextureFormat,
    /// `CellRenderOpts::skip_default_bg`.
    pub skip_default_bg: bool,
    /// Window padding `[top, right, bottom, left]` in px; the grid is
    /// inset by it, as in the renderer.
    pub pad: [u32; 4],
    /// Edge bleed (`extend_background = always` on both axes).
    pub bleed: bool,
}

impl Default for Offscreen {
    fn default() -> Self {
        Self {
            format: TextureFormat::Rgba8Unorm,
            skip_default_bg: true,
            pad: [0; 4],
            bleed: false,
        }
    }
}

/// [`render_term_offscreen`] with explicit [`Offscreen`] options. Rows
/// are shaped by the renderer's own `RowShaper` (viewport rows, so a
/// scrolled-back term draws its glyphs).
#[allow(clippy::too_many_lines, clippy::cast_precision_loss)] // test harness — clarity > brevity
pub(crate) fn render_term_offscreen_with(
    term: &Term,
    width: u32,
    height: u32,
    o: &Offscreen,
) -> RgbaImage {
    let (device, queue) = shared_device();
    let format = o.format;
    let target = device.create_texture(&TextureDescriptor {
        label: Some("snapshot target"),
        size: Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format,
        usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&TextureViewDescriptor::default());

    // M12d: text pipeline now requires a depth attachment. The
    // snapshot harness creates its own render pass (bypassing
    // `Renderer::render_term`), so we allocate a matching
    // `Depth32Float` texture and attach it below.
    let depth = device.create_texture(&TextureDescriptor {
        label: Some("snapshot depth"),
        size: Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format: TextureFormat::Depth32Float,
        usage: TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let depth_view = depth.create_view(&TextureViewDescriptor::default());

    // Construct the text path manually (no Renderer; that needs a window).
    // Use the same default line-height ratio the renderer applies so
    // snapshots stay byte-comparable to the M4b goldens.
    let mut rasterizer = GlyphRasterizer::new(
        &device,
        16.0,
        toastty_render::DEFAULT_LINE_HEIGHT,
        Some("Fira Mono"),
        Some(TEST_FONT),
    );
    let mut text_pipeline = TextPipeline::new(&device, format);
    let mask_view = pipeline::default_view(rasterizer.mask_texture());
    let color_view = pipeline::default_view(rasterizer.color_texture());
    let bind_group = text_pipeline.make_bind_group(&device, &mask_view, &color_view);

    // Content area inside the padding, and the rows drawn into it (the
    // renderer's own math), shaped like the renderer does.
    let cell_size = rasterizer.cell_size();
    let atlas_dims = rasterizer.atlas_dims();
    let (rows, cols) = term.size();
    let [pad_t, pad_r, pad_b, pad_l] = o.pad.map(|p| p as f32);
    let content_w = width as f32 - pad_l - pad_r;
    let content_h = height as f32 - pad_t - pad_b;
    let rows_rendered = toastty_render::text::instance::rows_to_render(
        rows,
        term.view_offset_pixel(),
        cell_size.1,
        content_h,
    );
    let mut shaper = RowShaper::new();
    shaper.shape_rows(&mut rasterizer, &queue, term, rows_rendered, &[]);
    // Bleed distance per edge: padding plus the floor-divide leftover on
    // the trailing edges (top-left grid alignment).
    let bleed = EdgeBleed {
        pad: [
            pad_t,
            pad_r + (content_w - f32::from(cols) * cell_size.0),
            pad_b + (content_h - f32::from(rows) * cell_size.1),
            pad_l,
        ],
        active: o.bleed,
        horizontal: ExtendCondition::Always,
        vertical: ExtendCondition::Always,
        col_fills: &[],
    };

    // Build instances.
    let theme = Theme::default_dark();
    let skip_default_bg = o.skip_default_bg;
    // Font-derived decoration geometry, as the live renderer uses.
    let opts = toastty_render::text::instance::CellRenderOpts {
        blink_on: true,
        decor: Some(rasterizer.decoration_metrics()),
        // The pass below clears to `theme.bg`, as the renderer's
        // full-redraw frames do.
        skip_default_bg,
    };
    let mut instances = Vec::new();
    toastty_render::text::instance::build_instances_into_opts(
        &mut instances,
        term,
        cell_size,
        &theme,
        None,
        |row, col, ch, _style| shaper.glyph(row, col, ch),
        |_, _| false,
        bleed,
        content_h,
        &opts,
    );

    // Readback buffer.
    let bytes_per_pixel: u32 = 4;
    let row_bytes = width * bytes_per_pixel;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded_row_bytes = row_bytes.div_ceil(align) * align;
    let buffer_size = u64::from(padded_row_bytes) * u64::from(height);
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("snapshot readback"),
        size: buffer_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor {
        label: Some("snapshot encoder"),
    });

    {
        let mut rp = encoder.begin_render_pass(&RenderPassDescriptor {
            label: Some("snapshot text pass"),
            color_attachments: &[Some(RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: Operations {
                    load: LoadOp::Clear(Color {
                        r: f64::from(theme.bg[0]),
                        g: f64::from(theme.bg[1]),
                        b: f64::from(theme.bg[2]),
                        a: f64::from(theme.bg[3]),
                    }),
                    store: StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &depth_view,
                depth_ops: Some(Operations {
                    load: LoadOp::Clear(1.0),
                    store: StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });

        #[allow(clippy::cast_precision_loss)]
        let globals = GlobalsUbo {
            viewport_and_atlas: [
                width as f32,
                height as f32,
                atlas_dims.0 as f32,
                atlas_dims.1 as f32,
            ],
            cursor_rect: toastty_render::text::instance::cursor_pixel_rect(term, cell_size),
            cursor_color: theme.cursor,
            content_origin: [pad_l, pad_t, 0.0, 0.0],
        };

        text_pipeline.upload(&device, &queue, globals, &instances);
        text_pipeline.render_bg(&mut rp, &bind_group, instances.len());
        text_pipeline.render_glyph(&mut rp, &bind_group, instances.len());
    }

    encoder.copy_texture_to_buffer(
        target.as_image_copy(),
        TexelCopyBufferInfo {
            buffer: &readback,
            layout: TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_row_bytes),
                rows_per_image: Some(height),
            },
        },
        Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );

    queue.submit(std::iter::once(encoder.finish()));

    let slice = readback.slice(..);
    let mapped = Arc::new(AtomicBool::new(false));
    let mapped_clone = mapped.clone();
    slice.map_async(wgpu::MapMode::Read, move |res| {
        res.expect("map_async");
        mapped_clone.store(true, Ordering::SeqCst);
    });
    while !mapped.load(Ordering::SeqCst) {
        let _ = device.poll(wgpu::PollType::wait_indefinitely());
    }
    let data = slice.get_mapped_range();

    // Repack into a tight RGBA8 buffer (skip row padding; swizzle BGRA).
    let bgra = matches!(
        format,
        TextureFormat::Bgra8Unorm | TextureFormat::Bgra8UnormSrgb
    );
    let mut out = Vec::with_capacity((row_bytes * height) as usize);
    for y in 0..height {
        let row_start = (y * padded_row_bytes) as usize;
        let row_end = row_start + row_bytes as usize;
        out.extend_from_slice(&data[row_start..row_end]);
    }
    if bgra {
        for px in out.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
    }
    drop(data);
    readback.unmap();

    RgbaImage::from_raw(width, height, out).expect("RgbaImage from raw bytes")
}

/// Compare `img` to the golden at `tests/snapshots/<name>.png`.
///
/// First-run / regeneration: set `TOASTTY_UPDATE_SNAPSHOTS=1` and the
/// captured image is written to disk instead of compared.
///
/// Otherwise, asserts SSIM ≥ 0.99 against the committed PNG.
pub(crate) fn assert_matches_golden(name: &str, img: &RgbaImage) {
    let path = snapshot_path(name);

    if std::env::var_os("TOASTTY_UPDATE_SNAPSHOTS").is_some() {
        let dir = path.parent().expect("snapshot path has parent");
        std::fs::create_dir_all(dir).expect("create snapshot dir");
        img.save(&path).expect("write golden");
        eprintln!("wrote golden {}", path.display());
        return;
    }

    if !path.exists() {
        // First run, no golden yet: write it and don't fail. CI / next
        // run will pick it up as the comparison baseline.
        let dir = path.parent().expect("snapshot path has parent");
        std::fs::create_dir_all(dir).expect("create snapshot dir");
        img.save(&path).expect("write initial golden");
        eprintln!(
            "no golden at {} — wrote captured output as new baseline; \
             commit it. Re-run to assert.",
            path.display()
        );
        return;
    }

    let golden = image::open(&path)
        .unwrap_or_else(|e| panic!("open {}: {e}", path.display()))
        .to_rgb8();
    let candidate = image::DynamicImage::ImageRgba8(img.clone()).to_rgb8();

    assert_eq!(
        golden.dimensions(),
        candidate.dimensions(),
        "golden dimensions {:?} don't match candidate {:?}",
        golden.dimensions(),
        candidate.dimensions()
    );

    let similarity =
        image_compare::rgb_similarity_structure(&Algorithm::MSSIMSimple, &golden, &candidate)
            .expect("rgb similarity");

    let score = similarity.score;
    eprintln!("snapshot {name}: SSIM = {score:.6}");
    if score < 0.99 {
        // Keep the mismatching render for inspection (CI uploads this
        // directory as an artifact on failure).
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target-snapshot-failures");
        if std::fs::create_dir_all(&dir).is_ok() {
            let _ = img.save(dir.join(format!("{name}.actual.png")));
            eprintln!("wrote {}", dir.join(format!("{name}.actual.png")).display());
        }
    }
    assert!(
        score >= 0.99,
        "SSIM {score:.6} < 0.99 for snapshot `{name}` (golden at {})",
        path.display()
    );
}

fn snapshot_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(SNAPSHOT_DIR)
        .join(format!("{name}.png"))
}
