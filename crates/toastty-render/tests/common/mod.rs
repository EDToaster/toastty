//! Shared test harness for headless text-rendering snapshots.
//!
//! Builds a wgpu device, renders a `Term` into an offscreen RGBA
//! texture, reads pixels back, and produces an `image::RgbaImage`.
//! Also handles golden-PNG comparison via SSIM.

#![allow(dead_code)] // each integration test file uses a subset

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use image::RgbaImage;
use image_compare::Algorithm;
use pollster::block_on;
use toastty_render::text::glyph_rasterizer::GlyphRasterizer;
use toastty_render::text::instance::Theme;
use toastty_render::text::pipeline::{self, GlobalsUbo, TextPipeline};
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

/// Render `term` into a `width × height` RGBA8 image, headless.
#[allow(clippy::too_many_lines)] // test harness — clarity > brevity
pub(crate) fn render_term_offscreen(term: &Term, width: u32, height: u32) -> RgbaImage {
    let instance = wgpu::Instance::new(instance_descriptor(instance_flags_for_tests()));
    let adapter = block_on(instance.request_adapter(&RequestAdapterOptions {
        power_preference: PowerPreference::LowPower,
        force_fallback_adapter: false,
        compatible_surface: None,
    }))
    .expect("no GPU adapter");
    let (device, queue) = block_on(adapter.request_device(&DeviceDescriptor {
        label: Some("text-snapshot device"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::downlevel_defaults(),
        memory_hints: wgpu::MemoryHints::Performance,
        experimental_features: wgpu::ExperimentalFeatures::disabled(),
        trace: wgpu::Trace::Off,
    }))
    .expect("device request");

    // Offscreen render target. RGBA8 (not sRGB) so saved PNG bytes are
    // directly comparable across runs without sRGB-decode drift.
    let format = TextureFormat::Rgba8Unorm;
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

    // Shape every visible row.
    let cell_size = rasterizer.cell_size();
    let atlas_dims = rasterizer.atlas_dims();
    let (rows, _cols) = term.size();
    let mut row_glyphs = Vec::with_capacity(rows as usize);
    for r in 0..rows {
        let row = term.row(r);
        let line_text: String = row
            .cells
            .iter()
            .map(|c| if c.ch == '\0' { ' ' } else { c.ch })
            .collect();
        row_glyphs.push(rasterizer.shape_line(&queue, &line_text, term.grapheme_cluster_mode()));
    }

    // Build instances.
    let theme = Theme::default_dark();
    let row_glyphs_ref = &row_glyphs;
    let instances = toastty_render::text::instance::build_instances(
        term,
        cell_size,
        &theme,
        None,
        |row, col, ch, _style| {
            let lg = row_glyphs_ref.get(row as usize)?;
            lg.by_column.get(&(col, ch)).copied()
        },
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

    // Repack into a tight RGBA8 buffer (skip row padding).
    let mut out = Vec::with_capacity((row_bytes * height) as usize);
    for y in 0..height {
        let row_start = (y * padded_row_bytes) as usize;
        let row_end = row_start + row_bytes as usize;
        out.extend_from_slice(&data[row_start..row_end]);
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
