# Native Mermaid benchmark

The `mermaid_bench` example measures the public Rust APIs directly, using
bundled fonts, the default dark palette, a 16 px label size, and the
checked-in stress fixture. It also parses all 23 gallery diagrams and
renders each at 320, 640, 960, 1600 and 4096 px where the renderer's
legibility guard permits it. A rejected narrow size is reported as a
fallback, not silently discarded. Every gallery diagram and the first
two positive stress fixtures must render at least once; the final three
stress fixtures deliberately permit readable fallbacks at all widths.

## Run

```sh
cargo build --release -p toastty-markdown --example mermaid_bench
./target/release/examples/mermaid_bench \
  --label before --rounds 12 --size-passes 32 > /tmp/mermaid-before.csv
```

Run the same harness, fixture, compiler, release profile and arguments on
both revisions. Build before timing; Cargo compilation is not a renderer
measurement. Use an otherwise idle machine, alternate before/after runs
and retain several runs rather than selecting the fastest result. Record
`git rev-parse HEAD`, `rustc -Vv`, `lscpu`, and any local source changes
alongside the CSV. The example rejects debug builds.

Optional `--png-dir /tmp/mermaid-pngs` writes the coverage rasters outside
the measured sections. For the actual terminal demo, run
`toastty md docs/fixtures/mermaid-responsive.md` and resize the window.

## What the phases measure

| Phase | Included work |
| --- | --- |
| `prepare-uncached-fonts-warm` | Parse, graph layout, SVG construction/parsing, and prepared-object destruction; global bundled font initialization is already warm. |
| `size-first-pixel-sweep` | `size_for` at every integer width 320 through 1200 and back, starting with a fresh prepared diagram. |
| `size-warm-pixel-sweep` | The same 1,762-width round trip, previsited once, repeated `--size-passes` times per sample. No pixels are requested. |
| `export-first-full-960` | First public `rasterize(960)` on a prepared diagram, including allocation and straight-alpha conversion. |
| `export-repeat-full-960` | Same-width public `rasterize(960)` after one unmeasured export. The responsive implementation deliberately does not cache full exports; this is not an application cache measurement. |
| `export-revisit-full-640-960-1200` | Previsit 640/960/1200, then export 640/960/1200/960/640/1200. Includes full allocation/conversion on every success. |
| `paint-first-viewport-960` | `Renderer::paint` on the top 50 terminal rows after layout, including lazy Mermaid rasterization and block compositing/conversion. |
| `paint-same-viewport-960` | Paint the same viewport after one unmeasured paint. |
| `paint-first-full-960` | Paint the whole Markdown block after layout, including first lazy Mermaid rasterization and block compositing/conversion. |
| `paint-revisit-viewport-640-960-1200` | Prepare and paint three widths, then repaint their first 50 rows in the order 640/960/1200/960/640/1200 using the same `Renderer`. Measures the actual application band-cache path; layout is outside the timer. |
| `paint-scroll-revisit-viewport-960` | Prepare and paint the top, one-third, and bottom viewports, then revisit them in the order 0/1/2/1/0/2 using the same `Renderer`. |

Preparation and Markdown layout for raster/paint phases happen outside
the timer. Each sample gets fresh prepared state unless the phase says
warm; output allocation, conversion and destruction are inside the
timer, while prepared-state destruction is outside. The prepare phase
necessarily destroys the newly prepared object inside its timer.
`process_first_prepare_ms` separately reports one process-first tiny
diagram including lazy font initialization; it is not a representative
stress-diagram cold-prepare result.

The benchmark reports completed operations, legibility fallbacks, output
pixels and output bytes as workload counters. Warm size sweeps report
the distribution of per-operation averages over batches, not individual
call latency percentiles. Raster/paint samples use one operation except
the six-operation revisit batches. Fallback operations are included in
timings; compare success counts before comparing averages. Paint phases
classify source-code fallbacks using the same available width as the
top-level layout (the requested width minus one 10 px cell on each side);
painting source is not evidence of faster diagram rendering.

## Interpretation limits

The public API does not expose pure resvg duration, cache hit counters,
or an allocation counter. The baseline implementation retains the last
full raster, whereas the responsive implementation caches bands in
`Renderer` and does not retain public full-image exports. Do not compare
repeated exports as evidence about application cache performance.
A larger readable output can
take longer to paint simply because it has more pixels; always compare
pixel and byte counts as well as latency.

On Linux, process user+system CPU time comes from `/proc/self/stat` with
the tick frequency reported by `getconf CLK_TCK`. It is quantized
(typically 10 ms), includes small measurement overhead, and is too
coarse for short phases. Every sample interval is quantized, so summing
many intervals does not guarantee sub-tick precision. A zero CPU value
means no CPU-clock tick was observed in the timed intervals, not free
work. Other platforms report `n/a`. No hardware counter claims are made;
if available, collect them separately:

```sh
perf stat -e task-clock,cycles,instructions,cache-misses \
  ./target/release/examples/mermaid_bench --label after
```

Hardware counters or a release run on a shared virtual CPU are not a
guarantee of interactive frame rate. These measurements exclude the PTY,
GPU upload, terminal frame scheduling, and window-system presentation.

## Recorded comparison

This section records the viewport-aware renderer before the modern
styling changes, through commit `9651fa3`. Its numbers are historical,
not measurements of the subsequently styled renderer. See the
[modern styling comparison](mermaid-style-benchmark.md) for fresh
pre-style versus styled measurements using the same harness.

Measured on 2026-11-13 with Rust 1.100.0, the repository's release
profile, and four KVM vCPUs reporting Intel Xeon 2.90 GHz. The baseline
is `546c7a337f2daab5cb5c4b7251d06b2b419855db`; the after binary is the
responsive implementation with the final 64 MiB shared scaled-raster
cache budget. Both used byte-identical benchmark and fixture files.
Other Cargo, rustc and test processes were paused during the runs.

The order was before/after/before/after, with `--rounds 50
--size-passes 32` for every run. The tables average both runs, not the
fastest result. Raw output and build/environment identifiers are kept in
`docs/benchmarks/mermaid-responsive/`:

- `before-1.csv` and `before-2.csv`
- `after-1.csv` and `after-2.csv`
- `environment.txt`

### Matched tall-sequence workload

The 80-message sequence has the same 364 × 4787 intrinsic raster in
both implementations. Every operation below succeeds without fallback,
and the before/after output pixel counts match exactly. The 960 px
viewport is 960 × 1100; the full Markdown block is 960 × 4840.

| Phase | Before mean, ms/op | After mean, ms/op | Observation |
| --- | ---: | ---: | --- |
| Uncached prepare, global fonts warm | 3.570 | 3.165 | Cold setup remains millisecond-scale; run variation is visible in the CSV. |
| First viewport paint | 20.181 | 6.559 | 3.08× lower latency for the visible part. |
| Same viewport repaint | 1.731 | 1.663 | Little change; output allocation/compositing/conversion still occur. |
| Width-revisit viewport paint | 11.180 | 1.510 | 7.41× lower latency on the application cache path. |
| Scroll-revisit viewport paint | 1.524 | 1.459 | Little change after both implementations are warm. |
| First whole-block paint | 15.329 | 25.761 | Regression: 68% slower when the entire tall block is requested. |
| First public full-image export | 27.289 | 28.602 | Similar full-output cost; not a viewport measurement. |
| Repeated public full-image export | 11.034 | 27.678 | Regression by design: the public export no longer retains a full-image cache. |

Both first-viewport after runs improved independently: 6.725 and
6.393 ms/op versus 20.094 and 20.269 ms/op before. Both width-revisit
after runs improved independently: 1.557 and 1.462 ms/op versus
11.259 and 11.101 ms/op before. The full-paint regression also appears
in both runs: 27.292 and 24.229 ms/op after versus 15.591 and
15.068 ms/op before.

Process CPU totals below sum the same timed intervals across both
runs. They support the direction of the substantial viewport changes,
but remain 100 Hz tick measurements, not precise per-call CPU timings.
They say nothing about idle CPU usage.

| Matched phase | Total operations | Total output pixels, each version | Before CPU, ms | After CPU, ms |
| --- | ---: | ---: | ---: | ---: |
| First viewport paint | 100 | 105,600,000 | 1,970 | 620 |
| Width-revisit viewport paint | 600 | 616,000,000 | 6,650 | 910 |
| First whole-block paint | 100 | 464,640,000 | 1,560 | 2,570 |

### Wrapping changes the work

The positive rectangular-node fixture is not a matched-pixel
performance comparison. At a requested width of 320 px, the native
diagram changes from 320 × 105 before to 276 × 778 after; at 960 px
it changes from 960 × 315 to 405 × 595. The additional vertical space
preserves wrapped labels instead of shrinking the entire diagram.

Its uncached preparation regresses from 1.005 to 6.312 ms/op. The first
1,762-call size sweep takes about 4.10 ms after, including the first
compact-scene preparation, while the warmed size calls average
0.026 microseconds each. The tall sequence's warmed size calls also
average 0.026 microseconds. These are batch means, not nanosecond-precise
individual-call measurements.

For the rectangular fixture, first viewport paint rises from 2.682 to
4.155 ms while its Markdown output grows from 359,040 to 633,600
pixels per call. Width revisits fall from 2.714 to 0.549 ms/op, even
with the larger output. Do not combine those changes with the matched
tall-sequence results into one universal speedup number.

### Coverage and fallbacks

Every run checks 140 diagram/width pairs and produces 55 timing rows.
All 23 gallery types parse and render at 4096 px after the change.
Every successful raster is nonblank, matches `size_for`, and respects
the width, height and pixel-count caps.

| Cases per run | Before rendered / fallback | After rendered / fallback |
| --- | ---: | ---: |
| 23 gallery types × 5 widths | 114 / 1 | 74 / 41 |
| 5 stress fixtures × 5 widths | 22 / 3 | 17 / 8 |

The stricter legibility floor intentionally chooses source fallback
more often at narrow sizes. The three long-edge/LR/descriptive-message
stress cases are retained to expose that limitation, not counted as
rendering speedups. The results show benefits for partial painting and
width reuse, alongside real cold-setup and whole-image regressions.
