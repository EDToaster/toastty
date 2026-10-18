//! Golden snapshots of whole documents, rendered with the bundled fonts
//! only (`Renderer::hermetic`) so they match on every machine.
//!
//! - `tests/golden/<name>.png`: the block image (straight-alpha RGBA),
//!   compared with a small per-pixel tolerance.
//! - `tests/golden/<name>.txt`: the text runs and link spans, compared
//!   exactly.
//!
//! Regenerate with `MD_UPDATE_GOLDEN=1 cargo test -p toastty-markdown --test golden`.
//! On a mismatch the actual output is written next to the test binary's
//! temp dir and its path is printed.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use toastty_markdown::{LayoutParams, RenderedBlock, Renderer, ResourcePolicy, Theme};

/// A channel may differ by this much before the pixel counts as changed.
const CHANNEL_TOLERANCE: u8 = 24;
/// Fraction of pixels allowed to differ.
const MAX_DIFF_FRACTION: f64 = 0.002;

fn light() -> Theme {
    Theme {
        fg: [0x1f, 0x23, 0x28, 0xff],
        bg: [0xff, 0xff, 0xff, 0xff],
        ansi: [
            [0x24, 0x29, 0x2f, 0xff],
            [0xcf, 0x22, 0x2e, 0xff],
            [0x11, 0x63, 0x29, 0xff],
            [0x4d, 0x2d, 0x00, 0xff],
            [0x09, 0x69, 0xda, 0xff],
            [0x82, 0x50, 0xdf, 0xff],
            [0x1b, 0x7c, 0x83, 0xff],
            [0x6e, 0x77, 0x81, 0xff],
            [0x57, 0x60, 0x6a, 0xff],
            [0xa4, 0x0e, 0x26, 0xff],
            [0x1a, 0x7f, 0x37, 0xff],
            [0x63, 0x3c, 0x01, 0xff],
            [0x21, 0x8b, 0xff, 0xff],
            [0xa4, 0x75, 0xf9, 0xff],
            [0x31, 0x92, 0xaa, 0xff],
            [0x8c, 0x95, 0x9f, 0xff],
        ],
    }
}

fn dark() -> Theme {
    Theme::default()
}

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn describe(b: &RenderedBlock) -> String {
    let mut s = format!("rows {}\n", b.rows);
    for (i, row) in b.text.iter().enumerate() {
        let _ = write!(s, "{i:3}");
        for r in row {
            let _ = write!(s, " @{}:{:?}", r.col, r.text);
        }
        s.push('\n');
    }
    for l in &b.links {
        let _ = writeln!(
            s,
            "link row {} cols {}..{} {}",
            l.row, l.cols.start, l.cols.end, l.url
        );
    }
    s
}

fn save_png(path: &Path, b: &RenderedBlock) {
    image::RgbaImage::from_raw(b.image.width, b.image.height, b.image.pixels.clone())
        .expect("size")
        .save(path)
        .expect("write png");
}

fn check(name: &str, src: &str, cols: u16, theme: Theme) {
    let params = LayoutParams {
        cols,
        cell_w: 10,
        cell_h: 22,
        font_px: 16.0,
        theme,
        resources: ResourcePolicy::default(),
        max_rows: 2000,
    };
    let mut r = Renderer::hermetic();
    let block = r.render(src, &params).expect("render");
    // Rendering is deterministic, including on a reused renderer.
    assert_eq!(r.render(src, &params).expect("render"), block);

    let dir = golden_dir();
    let png = dir.join(format!("{name}.png"));
    let txt = dir.join(format!("{name}.txt"));
    let runs = describe(&block);
    if std::env::var_os("MD_UPDATE_GOLDEN").is_some() {
        save_png(&png, &block);
        std::fs::write(&txt, &runs).expect("write txt");
        return;
    }
    let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let actual_png = out.join(format!("{name}.actual.png"));
    let expected_text = std::fs::read_to_string(&txt)
        .unwrap_or_else(|_| panic!("missing {} (run with MD_UPDATE_GOLDEN=1)", txt.display()))
        .replace("\r\n", "\n");
    if expected_text != runs {
        save_png(&actual_png, &block);
        std::fs::write(out.join(format!("{name}.actual.txt")), &runs).ok();
        panic!(
            "{name}: text differs from {}\nactual:\n{runs}",
            txt.display()
        );
    }
    let expected = image::open(&png)
        .unwrap_or_else(|e| {
            panic!(
                "missing {} ({e}); run with MD_UPDATE_GOLDEN=1",
                png.display()
            )
        })
        .into_rgba8();
    assert_eq!(
        (expected.width(), expected.height()),
        (block.image.width, block.image.height),
        "{name}: size differs"
    );
    let diff = expected
        .as_raw()
        .as_chunks::<4>()
        .0
        .iter()
        .zip(block.image.pixels.as_chunks::<4>().0)
        .filter(|(a, b)| {
            a.iter()
                .zip(b.iter())
                .any(|(x, y)| x.abs_diff(*y) > CHANNEL_TOLERANCE)
        })
        .count();
    let total = (block.image.width * block.image.height) as usize;
    let frac = f64::from(u32::try_from(diff).unwrap_or(u32::MAX))
        / f64::from(u32::try_from(total).unwrap_or(u32::MAX));
    if frac > MAX_DIFF_FRACTION {
        save_png(&actual_png, &block);
        panic!(
            "{name}: {diff} of {total} pixels differ ({:.3}%); actual written to {}",
            frac * 100.0,
            actual_png.display()
        );
    }
}

const KITCHEN_SINK: &str = include_str!("golden/kitchen_sink.md");

#[test]
fn kitchen_sink_dark_80() {
    check("kitchen_sink_dark_80", KITCHEN_SINK, 80, dark());
}

#[test]
fn kitchen_sink_dark_40() {
    check("kitchen_sink_dark_40", KITCHEN_SINK, 40, dark());
}

#[test]
fn kitchen_sink_light_80() {
    check("kitchen_sink_light_80", KITCHEN_SINK, 80, light());
}

#[test]
fn kitchen_sink_light_40() {
    check("kitchen_sink_light_40", KITCHEN_SINK, 40, light());
}
