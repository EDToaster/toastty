//! Reproducible native Mermaid benchmark; no browser, JS, or system fonts.
//!
//! `cargo run --release -p toastty-markdown --example mermaid_bench -- --label before`
//!
//! See `docs/mermaid-benchmark.md` for stage definitions and comparison caveats.
use std::hint::black_box;
use std::path::PathBuf;
use std::time::Instant;

use toastty_markdown::mermaid::{
    MAX_HEIGHT_PX, MAX_PIXELS, MAX_WIDTH_PX, PreparedDiagram, prepare_mermaid,
};
use toastty_markdown::{LayoutParams, Raster, Renderer, ResourcePolicy, Theme};

const FONT_PX: f32 = 16.0;
const COVERAGE_WIDTHS: [u32; 5] = [320, 640, 960, 1600, 4096];
const REVISIT_WIDTHS: [u32; 6] = [640, 960, 1200, 960, 640, 1200];
const GALLERY: &str = include_str!("../../../docs/mermaid-gallery.md");
const STRESS: &str = include_str!("../../../docs/fixtures/mermaid-responsive.md");

struct Options {
    label: String,
    rounds: usize,
    size_passes: usize,
    png_dir: Option<PathBuf>,
}

impl Options {
    fn parse() -> Result<Self, String> {
        let mut options = Self {
            label: "unlabelled".into(),
            rounds: 12,
            size_passes: 32,
            png_dir: None,
        };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            let value = args.next().ok_or_else(|| {
                "usage: mermaid_bench [--label NAME] [--rounds N] [--size-passes N] [--png-dir DIR]"
                    .to_owned()
            })?;
            match arg.as_str() {
                "--label" => options.label = value.replace([',', '\n', '\r'], "_"),
                "--rounds" => {
                    options.rounds = value.parse().map_err(|_| "invalid --rounds")?;
                }
                "--size-passes" => {
                    options.size_passes = value.parse().map_err(|_| "invalid --size-passes")?;
                }
                "--png-dir" => options.png_dir = Some(PathBuf::from(value)),
                _ => return Err(format!("unknown argument {arg}")),
            }
        }
        if options.rounds == 0 || options.size_passes == 0 {
            return Err("rounds and size-passes must be positive".into());
        }
        Ok(options)
    }
}

struct Case {
    name: String,
    source: String,
}

/// The checked-in files use ordinary, unindented Mermaid fences.
fn cases(markdown: &str, prefix: &str) -> Vec<Case> {
    let mut cases = Vec::new();
    let mut heading = String::new();
    let mut source = None::<String>;
    for line in markdown.lines() {
        if let Some(body) = source.as_mut() {
            if line.trim() == "```" {
                cases.push(Case {
                    name: format!("{prefix}-{:02}-{heading}", cases.len() + 1),
                    source: source.take().unwrap(),
                });
            } else {
                body.push_str(line);
                body.push('\n');
            }
        } else if line.trim() == "```mermaid" {
            source = Some(String::new());
        } else if let Some(title) = line.strip_prefix("## ") {
            heading = title.to_ascii_lowercase().replace([' ', ','], "-");
        }
    }
    assert!(source.is_none(), "unterminated Mermaid fence");
    cases
}

/// Linux process user+system CPU ticks. Not hardware cycles/instructions.
/// The tick rate comes from getconf, and unavailable platforms report n/a.
struct CpuClock {
    ticks_per_second: Option<f64>,
}

impl CpuClock {
    fn new() -> Self {
        let ticks_per_second = std::process::Command::new("getconf")
            .arg("CLK_TCK")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .and_then(|value| value.trim().parse::<f64>().ok())
            .filter(|rate| *rate > 0.0);
        Self { ticks_per_second }
    }

    #[allow(clippy::cast_precision_loss)] // Approximate durations, not exact integer accounting.
    fn seconds(&self) -> Option<f64> {
        let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
        // comm can contain spaces and parentheses. Fields after its final ')'
        // start with field 3; utime and stime are fields 14 and 15.
        let (_, fields) = stat.rsplit_once(')')?;
        let mut fields = fields.split_whitespace();
        let user = fields.nth(11)?.parse::<u64>().ok()?;
        let system = fields.next()?.parse::<u64>().ok()?;
        Some((user + system) as f64 / self.ticks_per_second?)
    }
}

#[derive(Default)]
struct Work {
    ok: u64,
    fallback: u64,
    pixels: u64,
    output_bytes: u64,
}

impl Work {
    fn size(&mut self, result: Result<(u32, u32), String>) {
        match black_box(result) {
            Ok((width, height)) => {
                self.ok += 1;
                black_box((width, height));
            }
            Err(error) => {
                self.fallback += 1;
                black_box(error);
            }
        }
    }

    fn raster(&mut self, result: Result<Raster, String>) {
        match black_box(result) {
            Ok(raster) => {
                self.ok += 1;
                self.pixels += u64::from(raster.width) * u64::from(raster.height);
                self.output_bytes += raster.pixels.len() as u64;
                black_box(raster);
            }
            Err(error) => {
                self.fallback += 1;
                black_box(error);
            }
        }
    }

    fn paint(&mut self, raster: Raster, diagram_visible: bool) {
        self.raster(Ok(raster));
        if !diagram_visible {
            self.ok -= 1;
            self.fallback += 1;
        }
    }
}

/// Setup, CSV output, and prepared-state destruction are outside the timer.
/// Output allocation, conversion and output-buffer destruction are inside.
#[allow(clippy::cast_precision_loss)] // Timing summaries intentionally use floating-point means.
fn measure<S>(
    options: &Options,
    clock: &CpuClock,
    case: &Case,
    phase: &str,
    mut setup: impl FnMut() -> Result<S, String>,
    mut run: impl FnMut(&mut S) -> Work,
) -> Result<(), String> {
    let mut durations = Vec::with_capacity(options.rounds);
    let mut totals = Work::default();
    let mut cpu_seconds = 0.0;
    let mut cpu_available = true;
    for _ in 0..options.rounds {
        let mut state = setup()?;
        let cpu_start = clock.seconds();
        let start = Instant::now();
        let work = black_box(run(&mut state));
        let elapsed = start.elapsed().as_secs_f64();
        let cpu_end = clock.seconds();
        if let Some((start, end)) = cpu_start.zip(cpu_end) {
            cpu_seconds += end - start;
        } else {
            cpu_available = false;
        }
        let operations = work.ok + work.fallback;
        assert!(operations > 0);
        durations.push(elapsed / operations as f64);
        totals.ok += work.ok;
        totals.fallback += work.fallback;
        totals.pixels += work.pixels;
        totals.output_bytes += work.output_bytes;
    }
    durations.sort_by(f64::total_cmp);
    let mean = durations.iter().sum::<f64>() / durations.len() as f64;
    let percentile = |pct: usize| durations[(durations.len() * pct).div_ceil(100) - 1] * 1e6;
    let cpu = if cpu_available {
        format!("{:.3}", cpu_seconds * 1000.0)
    } else {
        "n/a".into()
    };
    println!(
        "metric,{},{},{},{},{},{},{},{},{},{:.3},{:.3},{:.3},{:.3},{}",
        options.label,
        case.name,
        phase,
        options.rounds,
        totals.ok + totals.fallback,
        totals.ok,
        totals.fallback,
        totals.pixels,
        totals.output_bytes,
        mean * (totals.ok + totals.fallback) as f64 * 1000.0,
        mean * 1e6,
        percentile(50),
        percentile(95),
        cpu,
    );
    Ok(())
}

fn params(width: u32) -> LayoutParams {
    LayoutParams {
        cols: (width / 10) as u16,
        cell_w: 10,
        cell_h: 22,
        font_px: FONT_PX,
        theme: Theme::default(),
        resources: ResourcePolicy::default(),
        max_rows: 2000,
        mono_family: None,
    }
}

fn benchmark(options: &Options, clock: &CpuClock, case: &Case) -> Result<(), String> {
    let theme = Theme::default();
    let prepare = || prepare_mermaid(black_box(&case.source), FONT_PX, &theme);
    measure(
        options,
        clock,
        case,
        "prepare-uncached-fonts-warm",
        || Ok(()),
        |()| {
            let prepared = black_box(prepare().expect("coverage checked preparation"));
            black_box(prepared);
            Work {
                ok: 1,
                ..Work::default()
            }
        },
    )?;

    let widths: Vec<u32> = (320..=1200).chain((320..=1200).rev()).collect();
    measure(
        options,
        clock,
        case,
        "size-first-pixel-sweep",
        prepare,
        |prepared| {
            let mut work = Work::default();
            for &width in &widths {
                work.size(prepared.size_for(black_box(width)));
            }
            work
        },
    )?;
    let warmed = prepare()?;
    for &width in &widths {
        black_box(warmed.size_for(width)).ok();
    }
    measure(
        options,
        clock,
        case,
        "size-warm-pixel-sweep",
        || Ok(warmed.clone()),
        |prepared| {
            let mut work = Work::default();
            for _ in 0..options.size_passes {
                for &width in &widths {
                    work.size(prepared.size_for(black_box(width)));
                }
            }
            work
        },
    )?;

    for (phase, warm) in [
        ("export-first-full-960", false),
        ("export-repeat-full-960", true),
    ] {
        measure(
            options,
            clock,
            case,
            phase,
            || {
                let prepared = prepare()?;
                if warm {
                    black_box(prepared.rasterize(960)).ok();
                }
                Ok(prepared)
            },
            |prepared| {
                let mut work = Work::default();
                work.raster(prepared.rasterize(black_box(960)));
                work
            },
        )?;
    }
    measure(
        options,
        clock,
        case,
        "export-revisit-full-640-960-1200",
        || {
            let prepared = prepare()?;
            for width in [640, 960, 1200] {
                black_box(prepared.rasterize(width)).ok();
            }
            Ok(prepared)
        },
        |prepared| {
            let mut work = Work::default();
            for width in REVISIT_WIDTHS {
                work.raster(prepared.rasterize(black_box(width)));
            }
            work
        },
    )?;

    benchmark_paint(options, clock, case)
}

fn benchmark_paint(options: &Options, clock: &CpuClock, case: &Case) -> Result<(), String> {
    let markdown = format!("```mermaid\n{}```\n", case.source);
    // The layout engine pads this top-level block by one cell per side.
    // Distinguish painting a readable source fallback from painting a diagram.
    let app_probe = prepare_mermaid(&case.source, FONT_PX, &Theme::default())?;
    let diagram_visible = |width| app_probe.size_for(width - 2 * params(width).cell_w).is_ok();
    let visible_960 = diagram_visible(960);
    for (phase, warm, full) in [
        ("paint-first-viewport-960", false, false),
        ("paint-same-viewport-960", true, false),
        ("paint-first-full-960", false, true),
    ] {
        measure(
            options,
            clock,
            case,
            phase,
            || {
                let mut renderer = Renderer::hermetic();
                let layout = renderer
                    .layout(&markdown, &params(960))
                    .map_err(|error| error.to_string())?;
                let height = if full { layout.height } else { 50 * 22 };
                if warm {
                    black_box(renderer.paint(&layout, 0, height));
                }
                Ok((renderer, layout, height))
            },
            |(renderer, layout, height)| {
                let mut work = Work::default();
                work.paint(renderer.paint(layout, 0, *height), visible_960);
                work
            },
        )?;
    }
    measure(
        options,
        clock,
        case,
        "paint-revisit-viewport-640-960-1200",
        || {
            let mut renderer = Renderer::hermetic();
            let mut layouts = Vec::new();
            for width in [640, 960, 1200] {
                let layout = renderer
                    .layout(&markdown, &params(width))
                    .map_err(|error| error.to_string())?;
                black_box(renderer.paint(&layout, 0, 50 * 22));
                layouts.push((layout, diagram_visible(width)));
            }
            Ok((renderer, layouts))
        },
        |(renderer, layouts)| {
            let mut work = Work::default();
            for index in [0, 1, 2, 1, 0, 2] {
                let (layout, visible) = &layouts[index];
                work.paint(renderer.paint(layout, 0, 50 * 22), *visible);
            }
            work
        },
    )?;
    measure(
        options,
        clock,
        case,
        "paint-scroll-revisit-viewport-960",
        || {
            let mut renderer = Renderer::hermetic();
            let layout = renderer
                .layout(&markdown, &params(960))
                .map_err(|error| error.to_string())?;
            let offsets = [0, layout.height / 3, layout.height.saturating_sub(50 * 22)];
            for offset in offsets {
                black_box(renderer.paint(&layout, offset, 50 * 22));
            }
            Ok((renderer, layout, offsets))
        },
        |(renderer, layout, offsets)| {
            let mut work = Work::default();
            for index in [0, 1, 2, 1, 0, 2] {
                work.paint(renderer.paint(layout, offsets[index], 50 * 22), visible_960);
            }
            work
        },
    )?;
    Ok(())
}

fn check_raster(raster: &Raster, width: u32) {
    assert!(raster.width > 0 && raster.width <= width.min(MAX_WIDTH_PX));
    assert!(raster.height > 0 && raster.height <= MAX_HEIGHT_PX);
    assert!(u64::from(raster.width) * u64::from(raster.height) <= MAX_PIXELS);
    assert_eq!(
        raster.pixels.len() as u64,
        u64::from(raster.width) * u64::from(raster.height) * 4
    );
    assert!(
        raster
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel[3] != 0)
    );
}

fn coverage(
    options: &Options,
    case: &Case,
    prepared: &PreparedDiagram,
    require_render: bool,
) -> Result<(), String> {
    let mut rendered = 0;
    for width in COVERAGE_WIDTHS {
        match prepared.size_for(width) {
            Ok(size) => {
                let raster = prepared.rasterize(width)?;
                check_raster(&raster, width);
                assert_eq!((raster.width, raster.height), size);
                println!(
                    "coverage,{},{},{width},rendered,{},{}",
                    options.label, case.name, raster.width, raster.height
                );
                if let Some(directory) = &options.png_dir {
                    image::save_buffer(
                        directory.join(format!("{}-{width}.png", case.name)),
                        &raster.pixels,
                        raster.width,
                        raster.height,
                        image::ColorType::Rgba8,
                    )
                    .map_err(|error| error.to_string())?;
                }
                rendered += 1;
            }
            Err(error) if error.contains("legibly") => {
                println!(
                    "coverage,{},{},{width},legibility-fallback,0,0",
                    options.label, case.name
                );
            }
            Err(error) => return Err(format!("{} at {width}: {error}", case.name)),
        }
    }
    if rendered == 0 && require_render {
        return Err(format!(
            "{} failed to render at every supported width",
            case.name
        ));
    }
    Ok(())
}

fn main() -> Result<(), String> {
    let options = Options::parse()?;
    if cfg!(debug_assertions) {
        return Err("benchmark requires --release (debug assertions are enabled)".into());
    }
    if let Some(directory) = &options.png_dir {
        std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    }
    let clock = CpuClock::new();
    println!("# native Mermaid benchmark; bundled fonts; font_px={FONT_PX}");
    println!("# CPU is process user+system time; quantized ticks, not hardware counters");
    println!(
        "# tick_hz={:?}; timings exclude preparation unless phase says prepare",
        clock.ticks_per_second
    );
    println!(
        "# metric,label,case,phase,rounds,operations,ok,fallback,pixels,output_bytes,wall_total_ms,mean_us_per_op,p50_batch_us_per_op,p95_batch_us_per_op,cpu_total_ms"
    );
    println!("# coverage,label,case,width,status,raster_width,raster_height");
    let gallery = cases(GALLERY, "gallery");
    let stress = cases(STRESS, "stress");
    assert_eq!(
        gallery.len(),
        23,
        "gallery coverage must include all 23 types"
    );
    assert_eq!(
        stress.len(),
        5,
        "stress fixture must contain all five scenarios"
    );

    // Record process-first font loading separately, then keep global fonts warm.
    let first = Instant::now();
    black_box(prepare_mermaid(
        "flowchart TD\nA[Warmup] --> B[Fonts]",
        FONT_PX,
        &Theme::default(),
    )?);
    println!(
        "# process_first_prepare_ms={:.3}",
        first.elapsed().as_secs_f64() * 1000.0
    );
    for case in &gallery {
        let prepared = prepare_mermaid(&case.source, FONT_PX, &Theme::default())
            .map_err(|error| format!("{}: {error}", case.name))?;
        coverage(&options, case, &prepared, true)?;
    }
    for (index, case) in stress.iter().enumerate() {
        let prepared = prepare_mermaid(&case.source, FONT_PX, &Theme::default())
            .map_err(|error| format!("{}: {error}", case.name))?;
        coverage(&options, case, &prepared, index < 2)?;
    }
    println!("# coverage complete: 23 gallery and 5 stress diagrams, 5 widths each");
    for case in &stress {
        benchmark(&options, &clock, case)?;
    }
    Ok(())
}
