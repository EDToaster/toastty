# Benchmarks

Tools for measuring toastty's throughput, frame rate, CPU, memory and
latency. Everything runs off-screen on a private Xvfb display. The
safety rules in `AGENTS.md` ("Running toastty on a virtual screen")
apply, and the scripts follow them.

## Full suite

```sh
cargo build --release
bench/run.py --bin target/release/toastty --out after.json --work /tmp/toastty-bench
bench/compare.py before.json after.json      # Markdown table
bench/run.py ... --only dense_cells,idle_blink --reps 5
```

The workloads are generated once (seeded, so they're identical between runs)
and cached in `--work/data`. Every scenario reads toastty's own report,
which the binary writes when `TOASTTY_STATS=<path>` is set
(`crates/toastty/src/perf_stats.rs`). The idle scenarios also sample `/proc`.

| Scenario | What it does | Key metrics |
|---|---|---|
| `startup` | `toastty -- sleep 1.5` | launch → first frame, idle RSS |
| `idle_blink` / `idle_steady` | bash prompt for 10 s, cursor blinking / steady | CPU %, RSS |
| `throughput_ascii` / `_sgr` / `_unicode` / `_scroll` | `cat` 64 / 32 / 16 / 23 MB | MB/s, frames and FPS while streaming, CPU s, peak RSS |
| `dense_cells` / `_big` | every cell recoloured each frame, one synchronized-output frame at a time (128×42, and 256×73 at 2560×1400) | FPS, frame ms |
| `scrollback_memory` | 30k full-width coloured lines (fills the 10k scrollback) | RSS after |
| `key_latency` | types into `cat` at 25 keys/s | key → frame ms |
| `scroll_wheel` / `_big` | wheel-scrolls 20k lines of history | frame ms p50/p99 |

"Frame ms" is the CPU time of `Renderer::render_term`, including the
present. Output → frame is measured from the first PTY batch after a frame
to the end of the next frame.

## Headless parse throughput

No window or GPU, so it gives quick, low-noise numbers for the parser and the
terminal state:

```sh
cargo run --release -p toastty --example parse_bench -- /tmp/toastty-bench/data/*.txt
```

## CPU profiles

`perf`/`samply` need `perf_event_paranoid <= 1`. Without it, use the
in-process SIGPROF sampler behind the `pprof` feature:

```sh
cargo build --profile profiling --features pprof
bench/profile.sh /tmp/p.folded -- cat /tmp/toastty-bench/data/ascii.txt
bench/folded.py /tmp/p.folded --top 40 [--thread toastty] [--focus render_term]
# headless:
cargo build --profile profiling -p toastty --example parse_bench --features pprof
TOASTTY_PPROF=/tmp/p.folded target/profiling/examples/parse_bench FILE
```

## Caveats

- Only the X11 path with software presentation (`MESA_VK_WSI_DEBUG=sw`) is
  exercised. Absolute FPS and frame times differ from a real compositor, so
  compare runs against each other.
- Run comparisons one after another on a quiet machine. A parallel cargo
  build skews every number.
