//! Benchmark for the `render_term` hot path.
//!
//! Builds a headless wgpu device + the same shape/build/encode chain
//! `Renderer::render_term` uses (no surface — `present()` is excluded).
//! Scenarios:
//!
//! - `fullframe_200x60`: render a fully-populated 200×60 grid.
//! - `single_cell_change_200x60`: same grid, mutate one cell between
//!   iterations (the realistic keystroke case).
//! - `fullframe_300x120` / `single_cell_change_300x120`: the same two
//!   cases at twice the row count, to show how shaping cost scales.
//! - `fullframe_unicode_200x60`: full repaint of unchanged mixed-script
//!   text (CJK, emoji, box drawing), the rows that need cosmic-text.
//! - `scroll_ascii_200x60` / `scroll_unicode_200x60`: one new line of
//!   output per frame at the live bottom, so every row shifts.
//!
//! Run with `cargo bench -p toastty-render --bench render_term -- --quick`.
//! `TOASTTY_BENCH_CPU_ONLY=1` stops each frame after shaping + instance
//! building (no GPU work), for low-noise CPU numbers.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use criterion::{Criterion, criterion_group, criterion_main};
use pollster::block_on;
use toastty_parser::Parser;
use toastty_render::DEFAULT_LINE_HEIGHT;
use toastty_render::text::glyph_rasterizer::GlyphRasterizer;
use toastty_render::text::instance::{
    CellInstance, CellRenderOpts, EdgeBleed, Theme, build_instances_into_opts,
};
use toastty_render::text::pipeline::{self, GlobalsUbo, TextPipeline};
use toastty_render::text::row_shaper::RowShaper;
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
    depth_view: wgpu::TextureView,
    width: u32,
    height: u32,
    theme: Theme,
    /// The renderer's own row-shape cache + shaping loop.
    rows: RowShaper,
    /// Mirrors `TextState::instances_scratch` in `Renderer`.
    instances: Vec<CellInstance>,
    /// `TOASTTY_BENCH_CPU_ONLY=1`: stop after shaping + instance
    /// building, so the numbers exclude GPU work (and the noise of other
    /// processes sharing the GPU).
    cpu_only: bool,
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

        let depth = device.create_texture(&TextureDescriptor {
            label: Some("bench depth"),
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
            depth_view,
            width,
            height,
            theme: Theme::default_dark(),
            rows: RowShaper::new(),
            instances: Vec::new(),
            cpu_only: std::env::var_os("TOASTTY_BENCH_CPU_ONLY").is_some(),
        }
    }

    /// One full render: shape only dirty rows (re-using `line_cache`
    /// for clean ones), build instances into the reusable scratch vec,
    /// encode the text pass, submit. No `present()`. Mirrors
    /// `Renderer::render_term` post-fix.
    ///
    /// Callers should invoke `term.clear_damage()` after this returns
    /// to consume the damage signal.
    fn render_term(&mut self, term: &Term) {
        let (rows, _cols) = term.size();
        let cell_size = self.rasterizer.cell_size();
        let atlas_dims = self.rasterizer.atlas_dims();

        self.rows
            .shape_rows(&mut self.rasterizer, &self.queue, term, rows, &[]);

        let theme = self.theme;
        let rows_ref = &self.rows;
        // Same options as the renderer's full-redraw frames.
        let opts = CellRenderOpts {
            blink_on: true,
            decor: Some(self.rasterizer.decoration_metrics()),
            skip_default_bg: true,
        };
        build_instances_into_opts(
            &mut self.instances,
            term,
            cell_size,
            &theme,
            None,
            |row, col, ch, _style| rows_ref.glyph(row, col, ch),
            |_, _| false,
            EdgeBleed::default(),
            f32::from(term.size().0) * cell_size.1,
            &opts,
        );
        if self.cpu_only {
            return;
        }
        let instances = &self.instances;

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
            cursor_rect: toastty_render::text::instance::cursor_pixel_rect(term, cell_size),
            cursor_color: theme.cursor,
            content_origin: [0.0; 4],
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
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth_view,
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
            self.pipeline
                .upload(&self.device, &self.queue, globals, instances);
            self.pipeline
                .render_bg(&mut rp, &self.bind_group, instances.len());
            self.pipeline
                .render_glyph(&mut rp, &self.bind_group, instances.len());
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
    ///
    /// `force_full` ignores `term.damage()` and re-shapes every row,
    /// simulating the pre-fix code path so the report can show what we
    /// avoided.
    #[allow(clippy::too_many_lines)]
    fn render_term_instrumented(&mut self, term: &Term, force_full: bool) {
        let t_total = Instant::now();
        let (rows, cols) = term.size();
        let cell_size = self.rasterizer.cell_size();
        let atlas_dims = self.rasterizer.atlas_dims();

        // Phase A: shape (dirty rows only; `force_full` also drops the
        // row-shape cache, though rows still hit the rasterizer's
        // row-text memo).
        let t_shape = Instant::now();
        if force_full {
            self.rows.invalidate();
        }
        let shaped = self
            .rows
            .shape_rows(&mut self.rasterizer, &self.queue, term, rows, &[]);
        let shape_ms = t_shape.elapsed().as_secs_f64() * 1000.0;

        // Phase B: bind group (post-fix this is a no-op; left here for
        // direct comparison with the pre-fix breakdown).
        let t_bg = Instant::now();
        if force_full {
            let mask_view = pipeline::default_view(self.rasterizer.mask_texture());
            let color_view = pipeline::default_view(self.rasterizer.color_texture());
            self.bind_group = self
                .pipeline
                .make_bind_group(&self.device, &mask_view, &color_view);
        }
        let bg_ms = t_bg.elapsed().as_secs_f64() * 1000.0;

        // Phase C: build_instances.
        let t_build = Instant::now();
        let theme = self.theme;
        let rows_ref = &self.rows;
        // Same options as the renderer's full-redraw frames.
        let opts = CellRenderOpts {
            blink_on: true,
            decor: Some(self.rasterizer.decoration_metrics()),
            skip_default_bg: true,
        };
        build_instances_into_opts(
            &mut self.instances,
            term,
            cell_size,
            &theme,
            None,
            |row, col, ch, _style| rows_ref.glyph(row, col, ch),
            |_, _| false,
            EdgeBleed::default(),
            f32::from(term.size().0) * cell_size.1,
            &opts,
        );
        let build_ms = t_build.elapsed().as_secs_f64() * 1000.0;
        let n_instances = self.instances.len();
        let instances = &self.instances;

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
            cursor_rect: toastty_render::text::instance::cursor_pixel_rect(term, cell_size),
            cursor_color: theme.cursor,
            content_origin: [0.0; 4],
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
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth_view,
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
            self.pipeline
                .upload(&self.device, &self.queue, globals, instances);
            self.pipeline
                .render_bg(&mut rp, &self.bind_group, instances.len());
            self.pipeline
                .render_glyph(&mut rp, &self.bind_group, instances.len());
        }
        let enc_ms = t_enc.elapsed().as_secs_f64() * 1000.0;

        // Phase E: submit + wait.
        let t_sub = Instant::now();
        self.queue.submit(std::iter::once(encoder.finish()));
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        let sub_ms = t_sub.elapsed().as_secs_f64() * 1000.0;

        let total_ms = t_total.elapsed().as_secs_f64() * 1000.0;
        println!("  rows={rows} cols={cols} instances={n_instances} shaped={shaped}");
        println!("  shape_lines (×{shaped})         = {shape_ms:>8.3} ms");
        println!("  make_bind_group            = {bg_ms:>8.3} ms");
        println!("  build_instances            = {build_ms:>8.3} ms");
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

/// Compute the pixel viewport for a `rows × cols` grid using the
/// rasterizer's cell metrics. Drops the harness; callers build a fresh
/// one at the right size.
fn viewport_pixels(rows: u16, cols: u16) -> (u32, u32) {
    let h = Harness::new(1, 1);
    let (cw, ch) = h.rasterizer.cell_size();
    drop(h);
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let w = (cw * f32::from(cols)).ceil() as u32;
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let hpx = (ch * f32::from(rows)).ceil() as u32;
    (w, hpx)
}

fn bench_fullframe(c: &mut Criterion) {
    let (width, height) = viewport_pixels(ROWS, COLS);
    let mut harness = Harness::new(width, height);
    let mut term = make_term_filled(ROWS, COLS);

    // Warm up the atlas so the first measured iteration isn't penalised
    // by glyph uploads.
    harness.render_term(&term);
    term.clear_damage();

    c.bench_function("fullframe_200x60", |b| {
        b.iter(|| {
            // Worst case: every row is dirty (full repaint).
            term.mark_all_dirty();
            harness.render_term(&term);
            term.clear_damage();
        });
    });
}

fn bench_single_cell_change(c: &mut Criterion) {
    let (width, height) = viewport_pixels(ROWS, COLS);
    let mut harness = Harness::new(width, height);
    let mut term = make_term_filled(ROWS, COLS);
    let mut parser = Parser::new();
    parser.advance(&mut term, b"\x1b[31;101H");

    harness.render_term(&term);
    term.clear_damage();

    let mut tick: u8 = 0;
    c.bench_function("single_cell_change_200x60", |b| {
        b.iter(|| {
            tick = tick.wrapping_add(1);
            // Move to row 30 col 100, write one printable character —
            // exactly what a keystroke echo from the shell looks like.
            // Only row 30 gets marked dirty.
            let ch = b"abcdefghijklmnopqrstuvwxyz"[(tick as usize) % 26];
            parser.advance(
                &mut term,
                &[0x1b, b'[', b'3', b'1', b';', b'1', b'0', b'1', b'H', ch],
            );
            harness.render_term(&term);
            term.clear_damage();
        });
    });
}

/// Bigger "fullscreen at 5K" scenario — twice the rows of the 200×60
/// case to confirm scaling with row count. The pre-fix code shaped
/// every row every frame, so this is where the lag was the worst.
const BIG_COLS: u16 = 300;
const BIG_ROWS: u16 = 120;

fn bench_fullframe_fullscreen(c: &mut Criterion) {
    let (width, height) = viewport_pixels(BIG_ROWS, BIG_COLS);
    let mut harness = Harness::new(width, height);
    let mut term = make_term_filled(BIG_ROWS, BIG_COLS);
    harness.render_term(&term);
    term.clear_damage();

    c.bench_function("fullframe_300x120", |b| {
        b.iter(|| {
            term.mark_all_dirty();
            harness.render_term(&term);
            term.clear_damage();
        });
    });
}

fn bench_single_cell_change_fullscreen(c: &mut Criterion) {
    let (width, height) = viewport_pixels(BIG_ROWS, BIG_COLS);
    let mut harness = Harness::new(width, height);
    let mut term = make_term_filled(BIG_ROWS, BIG_COLS);
    let mut parser = Parser::new();
    parser.advance(&mut term, b"\x1b[61;101H");

    harness.render_term(&term);
    term.clear_damage();

    let mut tick: u8 = 0;
    c.bench_function("single_cell_change_300x120", |b| {
        b.iter(|| {
            tick = tick.wrapping_add(1);
            let ch = b"abcdefghijklmnopqrstuvwxyz"[(tick as usize) % 26];
            parser.advance(
                &mut term,
                &[0x1b, b'[', b'6', b'1', b';', b'1', b'0', b'1', b'H', ch],
            );
            harness.render_term(&term);
            term.clear_damage();
        });
    });
}

/// Mixed-script lines: CJK, emoji, box drawing, accents. Every line
/// with a wide char goes through cosmic-text (the slow path) whenever
/// it is shaped.
const UNICODE_LINES: [&str; 4] = [
    "│ src/main.rs │ 你好世界 ideographs こんにちは 한글 — CJK wide cells ├──┤ ",
    "Emoji 🚀 rocket ✨ sparkles 👍 thumbs ═══ café naïve résumé Ω λ Ж ░▒▓█ ",
    "日本語のテキスト mixed with ASCII words and → arrows ← and … ellipses ",
    "status: ok ✔ 进度 50% ⣿⣿⣿⣿⡇ elapsed 00:12 — 中文 漢字 ",
];

fn unicode_line(i: usize, cols: u16) -> String {
    let base = UNICODE_LINES[i % UNICODE_LINES.len()];
    let mut line = format!("{i:>6} ");
    // Fill to (roughly) the grid width without wrapping; wide chars
    // count as two columns.
    let mut w = line.len();
    for ch in base.chars().cycle() {
        let cw = if ch.is_ascii() || ('\u{2190}'..='\u{2bff}').contains(&ch) {
            1
        } else {
            2
        };
        if w + cw > usize::from(cols) {
            break;
        }
        line.push(ch);
        w += cw;
    }
    line
}

fn make_term_unicode(rows: u16, cols: u16) -> Term {
    let mut term = Term::new(rows, cols, 0);
    let mut parser = Parser::new();
    for i in 0..usize::from(rows) {
        let mut l = unicode_line(i, cols);
        if i + 1 < usize::from(rows) {
            l.push_str("\r\n");
        }
        parser.advance(&mut term, l.as_bytes());
    }
    term
}

fn bench_fullframe_unicode(c: &mut Criterion) {
    let (width, height) = viewport_pixels(ROWS, COLS);
    let mut harness = Harness::new(width, height);
    let mut term = make_term_unicode(ROWS, COLS);
    harness.render_term(&term);
    term.clear_damage();
    c.bench_function("fullframe_unicode_200x60", |b| {
        b.iter(|| {
            // Full repaint of unchanged text (a TUI repainting in new
            // colours, or a viewport change).
            term.mark_all_dirty();
            harness.render_term(&term);
            term.clear_damage();
        });
    });
}

/// New output at the live bottom: one new line per frame, every other
/// row shifts up one (and is dirty).
fn bench_scroll(c: &mut Criterion, name: &str, unicode: bool) {
    let (width, height) = viewport_pixels(ROWS, COLS);
    let mut harness = Harness::new(width, height);
    let mut term = if unicode {
        make_term_unicode(ROWS, COLS)
    } else {
        make_term_filled(ROWS, COLS)
    };
    let mut parser = Parser::new();
    parser.advance(&mut term, b"\x1b[60;1H");
    harness.render_term(&term);
    term.clear_damage();
    let mut i = 0usize;
    c.bench_function(name, |b| {
        b.iter(|| {
            i += 1;
            let line = if unicode {
                unicode_line(i, COLS)
            } else {
                format!("{i:>6} lorem ipsum dolor sit amet, consectetur adipiscing elit")
            };
            parser.advance(&mut term, b"\r\n");
            parser.advance(&mut term, line.as_bytes());
            harness.render_term(&term);
            term.clear_damage();
        });
    });
}

fn bench_scroll_ascii(c: &mut Criterion) {
    bench_scroll(c, "scroll_ascii_200x60", false);
}

fn bench_scroll_unicode(c: &mut Criterion) {
    bench_scroll(c, "scroll_unicode_200x60", true);
}

/// Low-noise CPU numbers — runs when `TOASTTY_BENCH_MIN=1` is set,
/// otherwise a no-op. Each scenario renders 400 frames CPU-only
/// (shaping + instance building, no GPU) and prints the minimum and
/// median frame time: the minimum is what the code costs, the median
/// what it costs on a busy machine.
fn bench_min_times(c: &mut Criterion) {
    if std::env::var_os("TOASTTY_BENCH_MIN").is_none() {
        return;
    }
    const FRAMES: usize = 400;
    let report = |name: &str, mut t: Vec<f64>| {
        t.sort_by(f64::total_cmp);
        println!(
            "min-time {name:<26} min {:>8.1} us  p50 {:>8.1} us",
            t[0],
            t[t.len() / 2]
        );
    };
    let lorem: Vec<&str> = "lorem ipsum dolor sit amet consectetur adipiscing elit sed do \
                            eiusmod tempor incididunt ut labore et dolore magna aliqua"
        .split(' ')
        .collect();
    for (rows, cols) in [(ROWS, COLS), (BIG_ROWS, BIG_COLS)] {
        let (width, height) = viewport_pixels(rows, cols);
        let mut h = Harness::new(width, height);
        h.cpu_only = true;
        // Full repaint of unchanged text.
        let mut term = make_term_filled(rows, cols);
        h.render_term(&term);
        let mut t = Vec::with_capacity(FRAMES);
        for _ in 0..FRAMES {
            term.mark_all_dirty();
            let t0 = Instant::now();
            h.render_term(&term);
            t.push(t0.elapsed().as_secs_f64() * 1e6);
            term.clear_damage();
        }
        report(&format!("repaint_{cols}x{rows}"), t);
        // Every row new text every frame (no memo hits), coloured words
        // (256-colour, then truecolor).
        for truecolor in [false, true] {
            let mut parser = Parser::new();
            let mut seed = 1u32;
            let mut t = Vec::with_capacity(FRAMES);
            for f in 0..FRAMES {
                let mut screen = String::from("\x1b[H");
                for r in 0..rows {
                    screen.push_str(&format!("\x1b[{};1H", r + 1));
                    let mut w = 0usize;
                    while w + 12 < usize::from(cols) {
                        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                        let word = lorem[(seed >> 16) as usize % lorem.len()];
                        if truecolor {
                            let (r, g, b) = (seed >> 8 & 255, seed >> 12 & 255, seed >> 4 & 255);
                            screen.push_str(&format!("\x1b[38;2;{r};{g};{b}m{word} "));
                        } else {
                            screen.push_str(&format!("\x1b[38;5;{}m{word} ", (seed >> 8) % 256));
                        }
                        w += word.len() + 1;
                    }
                    screen.push_str(&format!("{f}\x1b[K"));
                }
                parser.advance(&mut term, screen.as_bytes());
                let t0 = Instant::now();
                h.render_term(&term);
                t.push(t0.elapsed().as_secs_f64() * 1e6);
                term.clear_damage();
            }
            let kind = if truecolor { "rgb" } else { "256" };
            report(&format!("newtext_{kind}_{cols}x{rows}"), t);
        }
    }
    let (width, height) = viewport_pixels(ROWS, COLS);
    let mut h = Harness::new(width, height);
    h.cpu_only = true;
    let mut term = make_term_unicode(ROWS, COLS);
    h.render_term(&term);
    let mut t = Vec::with_capacity(FRAMES);
    for _ in 0..FRAMES {
        term.mark_all_dirty();
        let t0 = Instant::now();
        h.render_term(&term);
        t.push(t0.elapsed().as_secs_f64() * 1e6);
        term.clear_damage();
    }
    report("repaint_unicode_200x60", t);
    let mut parser = Parser::new();
    parser.advance(&mut term, b"\x1b[60;1H");
    let mut t = Vec::with_capacity(FRAMES);
    for i in 0..FRAMES {
        parser.advance(&mut term, b"\r\n");
        parser.advance(&mut term, unicode_line(i + 1000, COLS).as_bytes());
        let t0 = Instant::now();
        h.render_term(&term);
        t.push(t0.elapsed().as_secs_f64() * 1e6);
        term.clear_damage();
    }
    report("scroll_unicode_200x60", t);
    c.bench_function("min_times_dummy", |b| b.iter(|| {}));
}

/// Phase breakdown — runs once when `TOASTTY_BENCH_BREAKDOWN=1` is set,
/// otherwise registered as a no-op criterion bench. Prints per-section
/// wall time for both the cached single-cell case and the worst-case
/// full repaint, at both window sizes.
fn bench_phase_breakdown(c: &mut Criterion) {
    if std::env::var_os("TOASTTY_BENCH_BREAKDOWN").is_none() {
        return;
    }

    for (label, rows, cols) in [("200x60", ROWS, COLS), ("300x120", BIG_ROWS, BIG_COLS)] {
        let (width, height) = viewport_pixels(rows, cols);
        let mut h = Harness::new(width, height);
        let mut term = make_term_filled(rows, cols);

        // Warm up so atlas uploads aren't in the timing.
        for _ in 0..3 {
            h.render_term(&term);
        }
        term.clear_damage();

        println!("\n=== phase breakdown: {label}  grid={rows}x{cols}  px={width}x{height} ===",);

        // Single-cell change (the realistic keystroke case): mark only
        // one row dirty.
        let mut parser = Parser::new();
        parser.advance(&mut term, b"\x1b[1;1Hx");
        println!("[ single-cell change, only dirty row re-shapes ]");
        h.render_term_instrumented(&term, false);
        term.clear_damage();

        // Worst case (full repaint) — emulates the pre-fix code path.
        term.mark_all_dirty();
        println!("[ full repaint, every row re-shapes (pre-fix path) ]");
        h.render_term_instrumented(&term, true);
        term.clear_damage();
    }

    c.bench_function("phase_breakdown_dummy", |b| {
        b.iter(|| {});
    });
}

criterion_group!(
    benches,
    bench_fullframe,
    bench_single_cell_change,
    bench_fullframe_fullscreen,
    bench_single_cell_change_fullscreen,
    bench_fullframe_unicode,
    bench_scroll_ascii,
    bench_scroll_unicode,
    bench_phase_breakdown,
    bench_min_times,
);
criterion_main!(benches);
