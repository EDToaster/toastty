//! Headless parser + terminal-state throughput, no window or GPU.
//!
//!     cargo run --release -p toastty --example parse_bench -- FILE... [--cols 128 --rows 42 --reps 3]
//!
//! Feeds each file through `Parser::advance` into a `Term` in 64 KiB
//! chunks (the shape of real PTY reads) and prints MB/s. With
//! `--features pprof` and `TOASTTY_PPROF=<path>`, also writes a folded
//! CPU profile (see `bench/folded.py`).

use std::time::Instant;

use toastty_parser::Parser;
use toastty_term::Term;

#[allow(clippy::cast_precision_loss)] // MB/s for a report
fn main() {
    let mut files = Vec::new();
    let (mut cols, mut rows, mut reps, mut scrollback) = (128u16, 42u16, 3u32, 10_000u16);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut num = || args.next().and_then(|v| v.parse().ok()).expect("number");
        match a.as_str() {
            "--cols" => cols = num(),
            "--rows" => rows = num(),
            "--reps" => reps = u32::from(num()),
            "--scrollback" => scrollback = num(),
            _ => files.push(a),
        }
    }
    toastty::perf_stats::start_profiler();
    for f in &files {
        let data = std::fs::read(f).expect("read workload");
        let mut best = f64::MAX;
        for _ in 0..reps {
            let mut term = Term::new(rows, cols, scrollback);
            let mut parser = Parser::new();
            let t = Instant::now();
            for chunk in data.chunks(64 << 10) {
                parser.advance(&mut term, chunk);
                term.drain_pty_replies();
            }
            best = best.min(t.elapsed().as_secs_f64());
        }
        let name = std::path::Path::new(f)
            .file_name()
            .unwrap()
            .to_string_lossy();
        println!(
            "{name:<16} {:>8.1} MB/s  ({:.3} s for {:.1} MB, best of {reps})",
            data.len() as f64 / best / 1e6,
            best,
            data.len() as f64 / 1e6
        );
    }
    toastty::perf_stats::finish_profiler();
}
