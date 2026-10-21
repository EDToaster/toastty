//! Times re-layout across a sweep of widths, like an interactive window
//! resize: `layout` per width plus painting one screen (50 rows) of it,
//! versus the full `render`.
//!
//! `cargo run --release -p toastty-markdown --example resize_bench -- doc.md`
//! (`MD_SYSTEM_FONTS=1` to use system fonts.)
use std::time::{Duration, Instant};
use toastty_markdown::{LayoutParams, Renderer, ResourcePolicy, Theme};

fn stats(name: &str, v: &[Duration]) {
    let total: Duration = v.iter().sum();
    let max = v.iter().max().copied().unwrap_or_default();
    println!(
        "{name:>18}: avg {:>9.3?}  max {:>9.3?}",
        total / v.len() as u32,
        max
    );
}

#[allow(clippy::many_single_char_names)]
fn main() {
    let src = std::fs::read_to_string(std::env::args().nth(1).expect("markdown file")).unwrap();
    let mut r = if std::env::var_os("MD_SYSTEM_FONTS").is_some() {
        Renderer::new()
    } else {
        Renderer::hermetic()
    };
    let mut p = LayoutParams {
        cols: 90,
        cell_w: 10,
        cell_h: 22,
        font_px: 16.0,
        theme: Theme::default(),
        resources: ResourcePolicy::default(),
        max_rows: 2000,
        mono_family: None,
    };
    let t = Instant::now();
    let b = r.render(&src, &p).unwrap();
    println!("first render {:?}, {} rows", t.elapsed(), b.rows);
    let (mut lay, mut screen, mut full) = (vec![], vec![], vec![]);
    for cols in (40..=120).rev() {
        p.cols = cols;
        let t = Instant::now();
        let l = r.layout(&src, &p).unwrap();
        lay.push(t.elapsed());
        let t = Instant::now();
        let px = r.paint(&l, l.height / 3, 50 * p.cell_h);
        screen.push(t.elapsed());
        assert!(px.height > 0);
        let t = Instant::now();
        let _ = r.render(&src, &p).unwrap();
        full.push(t.elapsed());
    }
    println!("sweep cols 120 -> 40 ({} widths):", lay.len());
    stats("layout", &lay);
    stats("paint 50 rows", &screen);
    stats("full render", &full);
}
