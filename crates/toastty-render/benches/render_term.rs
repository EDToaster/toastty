//! Benchmark for the `render_term` hot path.
//!
//! Builds a headless wgpu device + the same shape/build/encode chain
//! `Renderer::render_term` uses (no surface — present() is excluded).
//! Two scenarios:
//!
//! - `fullframe_200x60`: render a fully-populated 200×60 grid.
//! - `single_cell_change_200x60`: same grid, mutate one cell between
//!    iterations (the realistic keystroke case).
//!
//! Run with `cargo bench -p toastty-render --bench render_term -- --quick`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use criterion::{Criterion, criterion_group, criterion_main};
use pollster::block_on;
use toastty_parser::Parser;
use toastty_render::DEFAULT_LINE_HEIGHT;
use toastty_render::text::glyph_rasterizer::{GlyphRasterizer, LineGlyphs};
use toastty_render::text::instance::{Theme, build_instances};
use toastty_render::text::pipeline::{self, GlobalsUbo, TextPipeline};
use toastty_render::{instance_descriptor, instance_flags_for_release};
use toastty_term::Term;
use wgpu::{
    Color, CommandEncoderDescriptor, Device, DeviceDescriptor, Extent3d, LoadOp, Operations,
    PowerPreference, Queue, RenderPassColorAttachment, RenderPassDescriptor, RequestAdapterOptions,
    StoreOp, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages,
    TextureViewDescriptor,
};

const FONT_BYTES: &[u8] = include_bytes!("../fonts/FiraMono-Medium.ttf");

const COLS: u16 = 200;
const ROWS: u16 = 60;

/// Headless renderer that mirrors the costs of `Renderer::render_term`
/// minus `present()`. Recreated once per bench setup.
struct Harness {
    device: Device,
    queue: Queue,
    rasterizer: GlyphRasterizer,
    pipeline: TextPipeline,
    bind_group: wgpu::BindGroup,
    target_view: wgpu::TextureView,
    width: u32,
    height: u32,
    theme: Theme,
}

impl Harness {
    fn new(width: u32, height: u32) -> Self {
        // Use release-build instance flags to match the hot path users see.
        let instance = wgpu::Instance::new(instance_descriptor(instance_flags_for_release()));
        let adapter = block_on(instance.request_adapter(&RequestAdapterOptions {
            power_preference: PowerPreference::LowPower,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .expect("no GPU adapter for bench");
        let (device, queue) = block_on(adapter.request_device(&DeviceDescriptor {
            label: Some("bench device"),
            required_features: wgpu::Features::empty(),
            // `defaults()` lets us go past 2048 pixels (fullscreen at
            // 200×60 cells with a 16px font is ~2660×1344).
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::Performance,
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            trace: wgpu::Trace::Off,
        }))
        .expect("device request failed");

        let format = TextureFormat::Rgba8Unorm;
        let target = device.create_texture(&TextureDescriptor {
            label: Some("bench target"),
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
        let target_view = target.create_view(&TextureViewDescriptor::default());

        let rasterizer = GlyphRasterizer::new(
            &device,
            16.0,
            DEFAULT_LINE_HEIGHT,
            Some("Fira Mono"),
            Some(FONT_BYTES),
        );
        let pipeline = TextPipeline::new(&device, format);
        let mask_view = pipeline::default_view(rasterizer.mask_texture());
        let color_view = pipeline::default_view(rasterizer.color_texture());
        let bind_group = pipeline.make_bind_group(&device, &mask_view, &color_view);

        Self {
            device,
            queue,
            rasterizer,
            pipeline,
            bind_group,
            target_view,
            width,
            height,
            theme: Theme::default_dark(),
        }
    }

    /// One full render: shape every row, build instances, encode the
    /// text pass, submit. No `present()`. Mirrors `Renderer::render_term`.
    fn render_term(&mut self, term: &Term) {
        let (rows, _cols) = term.size();
        let cell_size = self.rasterizer.cell_size();
        let atlas_dims = self.rasterizer.atlas_dims();

        let mut row_glyphs: Vec<LineGlyphs> = Vec::with_capacity(rows as usize);
        for r in 0..rows {
            let row = term.row(r);
            let line_text: String = row
                .cells
                .iter()
                .map(|c| if c.ch == '\0' { ' ' } else { c.ch })
                .collect();
            row_glyphs.push(self.rasterizer.shape_line(&self.queue, &line_text));
        }

        // Mirror render_term: rebuild bind group each frame.
        let mask_view = pipeline::default_view(self.rasterizer.mask_texture());
        let color_view = pipeline::default_view(self.rasterizer.color_texture());
        self.bind_group = self
            .pipeline
            .make_bind_group(&self.device, &mask_view, &color_view);

        let theme = self.theme;
        let row_glyphs_ref = &row_glyphs;
        let instances =
            build_instances(term, cell_size, &theme, |row, col, ch, _style| {
                let lg = row_glyphs_ref.get(row as usize)?;
                lg.by_column.get(&(col, ch)).copied()
            });

        let mut encoder = self
            .device
            .create_command_encoder(&CommandEncoderDescriptor {
                label: Some("bench term encoder"),
            });

        #[allow(clippy::cast_precision_loss)]
        let globals = GlobalsUbo {
            viewport_and_atlas: [
                self.width as f32,
                self.height as f32,
                atlas_dims.0 as f32,
                atlas_dims.1 as f32,
            ],
        };

        {
            let mut rp = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("bench term pass"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &self.target_view,
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
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.pipeline.render(
                &self.device,
                &self.queue,
                &mut rp,
                &self.bind_group,
                globals,
                &instances,
            );
        }

        self.queue.submit(std::iter::once(encoder.finish()));

        // Block until the GPU finishes so the bench measures real cost.
        // (Without this, `submit` returns immediately and we'd measure
        // CPU-side queueing only.)
        let done = Arc::new(AtomicBool::new(false));
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        done.store(true, Ordering::SeqCst);
    }

    /// Same as `render_term` but prints per-phase wall time. Used by the
    /// one-off `bench_phase_breakdown` to attribute the cost.
    #[allow(clippy::too_many_lines)]
    fn render_term_instrumented(&mut self, term: &Term) {
        let t_total = Instant::now();
        let (rows, _cols) = term.size();
        let cell_size = self.rasterizer.cell_size();
        let atlas_dims = self.rasterizer.atlas_dims();

        // Phase A: shape lines.
        let t_shape = Instant::now();
        let mut row_glyphs: Vec<LineGlyphs> = Vec::with_capacity(rows as usize);
        for r in 0..rows {
            let row = term.row(r);
            let line_text: String = row
                .cells
                .iter()
                .map(|c| if c.ch == '\0' { ' ' } else { c.ch })
                .collect();
            row_glyphs.push(self.rasterizer.shape_line(&self.queue, &line_text));
        }
        let shape_ms = t_shape.elapsed().as_secs_f64() * 1000.0;

        // Phase B: bind group.
        let t_bg = Instant::now();
        let mask_view = pipeline::default_view(self.rasterizer.mask_texture());
        let color_view = pipeline::default_view(self.rasterizer.color_texture());
        self.bind_group = self
            .pipeline
            .make_bind_group(&self.device, &mask_view, &color_view);
        let bg_ms = t_bg.elapsed().as_secs_f64() * 1000.0;

        // Phase C: build_instances.
        let t_bi = Instant::now();
        let theme = self.theme;
        let row_glyphs_ref = &row_glyphs;
        let instances =
            build_instances(term, cell_size, &theme, |row, col, ch, _style| {
                let lg = row_glyphs_ref.get(row as usize)?;
                lg.by_column.get(&(col, ch)).copied()
            });
        let bi_ms = t_bi.elapsed().as_secs_f64() * 1000.0;
        let n_instances = instances.len();

        // Phase D: encode the render pass.
        let t_enc = Instant::now();
        let mut encoder = self
            .device
            .create_command_encoder(&CommandEncoderDescriptor {
                label: Some("instr term encoder"),
            });

        #[allow(clippy::cast_precision_loss)]
        let globals = GlobalsUbo {
            viewport_and_atlas: [
                self.width as f32,
                self.height as f32,
                atlas_dims.0 as f32,
                atlas_dims.1 as f32,
            ],
        };
        {
            let mut rp = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("instr term pass"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &self.target_view,
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
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.pipeline.render(
                &self.device,
                &self.queue,
                &mut rp,
                &self.bind_group,
                globals,
                &instances,
            );
        }
        let enc_ms = t_enc.elapsed().as_secs_f64() * 1000.0;

        // Phase E: submit + wait.
        let t_sub = Instant::now();
        self.queue.submit(std::iter::once(encoder.finish()));
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        let sub_ms = t_sub.elapsed().as_secs_f64() * 1000.0;

        let total_ms = t_total.elapsed().as_secs_f64() * 1000.0;
        println!("  instances                  = {n_instances}");
        println!("  shape_lines (×{rows})         = {shape_ms:>8.3} ms");
        println!("  make_bind_group            = {bg_ms:>8.3} ms");
        println!("  build_instances            = {bi_ms:>8.3} ms");
        println!("  encode pass                = {enc_ms:>8.3} ms");
        println!("  submit + device.poll(wait) = {sub_ms:>8.3} ms");
        println!("  TOTAL                      = {total_ms:>8.3} ms");
    }
}

/// Lorem-ipsum-ish line generator. Returns deterministic text of the
/// requested width with varied punctuation/colors-of-letters; no SGR
/// escapes (we want raw cell content cost, not parser overhead).
fn make_term_filled(rows: u16, cols: u16) -> Term {
    let mut term = Term::new(rows, cols, 0);
    let mut parser = Parser::new();
    // Build one continuous stream that wraps to fill the grid.
    let lorem = "Lorem ipsum dolor sit amet, consectetur adipiscing elit, \
                 sed do eiusmod tempor incididunt ut labore et dolore magna \
                 aliqua. Ut enim ad minim veniam, quis nostrud exercitation \
                 ullamco laboris nisi ut aliquip ex ea commodo consequat. \
                 Duis aute irure dolor in reprehenderit in voluptate velit \
                 esse cillum dolore eu fugiat nulla pariatur. Excepteur sint \
                 occaecat cupidatat non proident, sunt in culpa qui officia \
                 deserunt mollit anim id est laborum. ";
    let target_chars = usize::from(rows) * usize::from(cols);
    let mut buf = String::with_capacity(target_chars + 64);
    while buf.len() < target_chars {
        buf.push_str(lorem);
    }
    parser.advance(&mut term, buf.as_bytes());
    term
}

fn bench_fullframe(c: &mut Criterion) {
    // Compute the pixel size from the rasterizer's cell metrics so the
    // viewport matches the grid. We need the rasterizer just to read
    // cell size — borrow Harness for the rest.
    let harness = Harness::new(1, 1);
    let (cw, ch) = harness.rasterizer.cell_size();
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let width = (cw * f32::from(COLS)).ceil() as u32;
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let height = (ch * f32::from(ROWS)).ceil() as u32;
    drop(harness);
    let mut harness = Harness::new(width, height);

    let term = make_term_filled(ROWS, COLS);

    // Warm up the atlas so the first measured iteration isn't penalised
    // by glyph uploads.
    harness.render_term(&term);

    c.bench_function("fullframe_200x60", |b| {
        b.iter(|| {
            harness.render_term(&term);
        });
    });
}

fn bench_single_cell_change(c: &mut Criterion) {
    let harness = Harness::new(1, 1);
    let (cw, ch) = harness.rasterizer.cell_size();
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let width = (cw * f32::from(COLS)).ceil() as u32;
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let height = (ch * f32::from(ROWS)).ceil() as u32;
    drop(harness);
    let mut harness = Harness::new(width, height);

    let mut term = make_term_filled(ROWS, COLS);
    let mut parser = Parser::new();
    // Park the cursor at row 30 col 100 so each iteration just toggles
    // one cell.
    parser.advance(&mut term, b"\x1b[31;101H");

    harness.render_term(&term);

    let mut tick: u8 = 0;
    c.bench_function("single_cell_change_200x60", |b| {
        b.iter(|| {
            tick = tick.wrapping_add(1);
            // Move to row 30 col 100, write one printable character,
            // come back. Just two CSI moves + 1 byte ~= what a keystroke
            // echo from the shell looks like.
            let ch = b"abcdefghijklmnopqrstuvwxyz"[(tick as usize) % 26];
            parser.advance(&mut term, &[0x1b, b'[', b'3', b'1', b';', b'1', b'0', b'1', b'H', ch]);
            harness.render_term(&term);
        });
    });
}

/// Phase breakdown — runs once when `TOASTTY_BENCH_BREAKDOWN=1` is set,
/// otherwise registered as a no-op criterion bench. Prints per-section
/// wall time for one warm `render_term` at fullscreen.
fn bench_phase_breakdown(c: &mut Criterion) {
    if std::env::var_os("TOASTTY_BENCH_BREAKDOWN").is_none() {
        return;
    }

    let mut term = make_term_filled(ROWS, COLS);
    let harness = Harness::new(1, 1);
    let (cw, ch) = harness.rasterizer.cell_size();
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let width = (cw * f32::from(COLS)).ceil() as u32;
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let height = (ch * f32::from(ROWS)).ceil() as u32;
    drop(harness);
    let mut h = Harness::new(width, height);

    // Warm
    for _ in 0..3 {
        h.render_term(&term);
    }

    let mut parser = Parser::new();
    parser.advance(&mut term, b"\x1b[31;101Hx");

    println!("\n=== phase breakdown: fullscreen {}x{} ({} px × {} px) ===", COLS, ROWS, width, height);
    h.render_term_instrumented(&term);

    // Register a dummy bench so criterion is happy.
    c.bench_function("phase_breakdown_dummy", |b| {
        b.iter(|| {});
    });
}

criterion_group!(benches, bench_fullframe, bench_single_cell_change, bench_phase_breakdown);
criterion_main!(benches);
