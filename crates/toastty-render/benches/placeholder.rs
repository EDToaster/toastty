//! Benchmark for building kitty Unicode-placeholder image instances
//! (`image::placeholder::build_placeholder_instances`), the per-frame
//! CPU cost of placeholder images. No GPU needed.
//!
//! Scenarios, on a 200×60 screen full of placeholder cells:
//!
//! - `image_200x60`: one image covering the screen (the common case; it
//!   merges into a single instance).
//! - `hostile_runs_4096_placements`: 4096 virtual placements of one image,
//!   and every cell is its own run naming a placement id that doesn't
//!   exist. With a linear lookup this was ~105 ms/frame; the indexed table
//!   makes it one hash lookup per run.
//!
//! Run with `cargo bench -p toastty-render --bench placeholder -- --quick`.

use criterion::{Criterion, criterion_group, criterion_main};
use toastty_graphics::KittySink;
use toastty_parser::Parser;
use toastty_render::image::placeholder::{RowLayout, build_placeholder_instances};
use toastty_term::{ImageData, Term, VirtualPlacement};

const COLS: u16 = 200;
const ROWS: u16 = 60;
const CELL: (f32, f32) = (10.0, 19.0);

/// Kitty's first diacritics; enough for 60 rows and 200 columns.
fn diacritic(i: usize) -> char {
    toastty_graphics::kitty::placeholder::index_to_diacritic(u16::try_from(i).unwrap()).unwrap()
}

fn setup(placements: u32) -> Term {
    let mut t = Term::new(ROWS, COLS, 0);
    let data = ImageData {
        width: 400,
        height: 240,
        pixels: vec![255; 400 * 240 * 4],
    };
    KittySink::register_image(&mut t, 42, 0, data);
    for p in 0..placements {
        KittySink::place_virtual(
            &mut t,
            VirtualPlacement {
                image_id: 42,
                placement_id: p,
                cols: u32::from(COLS),
                rows: u32::from(ROWS),
                z: 0,
            },
        );
    }
    t
}

fn feed(t: &mut Term, s: &str) {
    Parser::new().advance(t, s.as_bytes());
}

fn bench(c: &mut Criterion) {
    let layout = RowLayout {
        rows_rendered: ROWS,
        y_translate: 0.0,
    };

    // One full-screen image.
    let mut t = setup(1);
    let mut s = String::from("\x1b[H\x1b[38;5;42m");
    for r in 0..usize::from(ROWS) {
        for col in 0..usize::from(COLS) {
            s.push(toastty_term::PLACEHOLDER);
            s.push(diacritic(r));
            s.push(diacritic(col));
        }
    }
    feed(&mut t, &s);
    let mut out = Vec::new();
    c.bench_function("image_200x60", |b| {
        b.iter(|| {
            out.clear();
            build_placeholder_instances(&mut out, &t, CELL, layout, None);
            assert_eq!(out.len(), 1);
        });
    });

    // Hostile: 4096 placements, every cell a separate run naming a
    // placement id (underline colour 58;2;1;0;0 = 65536) that does not exist.
    let mut t = setup(4096);
    let mut s = String::from("\x1b[H\x1b[38;5;42m\x1b[58;2;1;0;0m");
    for r in 0..usize::from(ROWS) {
        for _ in 0..COLS {
            s.push(toastty_term::PLACEHOLDER);
            s.push(diacritic(r));
            s.push(diacritic(0));
        }
    }
    feed(&mut t, &s);
    c.bench_function("hostile_runs_4096_placements", |b| {
        b.iter(|| {
            out.clear();
            build_placeholder_instances(&mut out, &t, CELL, layout, None);
            assert!(out.is_empty());
        });
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
