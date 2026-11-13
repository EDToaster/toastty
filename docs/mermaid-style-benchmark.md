# Modern Mermaid styling benchmark

The modern styling preserves every measured raster size and fallback
decision relative to the pre-style responsive renderer. Preparation and
first-viewport means are close in these samples, but warm scroll revisits
are slower: **+0.138 ms/op (+24.3%)** for the rectangular fixture and
**+0.242 ms/op (+17.5%)** for the tall sequence. This is not evidence of
unchanged overall performance or a universal styling speedup.

## Revisions and method

The before revision is `9651fa30cc68c615722884f2c8f0fd61f51075b9`.
The styled executable was built from that parent plus the final styling
worktree on 2026-11-13; its source and executable hashes are recorded in
[environment.txt](benchmarks/mermaid-style/environment.txt). The earlier
`546c7a3` comparison measures the viewport-aware renderer implementation,
not this styling change, and its performance claims are not reused here.

The clean pre-style release executable was copied to a separate path and
hashed before the styled build. Both builds used `cargo build --release
-j 2 -p toastty-markdown --example mermaid_bench`, the same harness,
Mermaid fixture content, compiler, release profile, and 64 MiB shared
scaled-raster cache budget. The lockfile change adds the already-resolved
`roxmltree 0.21.1` as a direct dependency; no package version changes.

Four complete runs alternated before/after/before/after, each with
`--rounds 50 --size-passes 32`. Cargo, rustc, and test work from the
collaborating agents was paused during these runs. Every run exited
successfully with empty stderr; no fastest-run selection or obsolete
32 MiB-cache measurements are included.

Measurements used four KVM vCPUs reporting Intel Xeon 2.90 GHz, Linux
6.1.155, Rust 1.100.0, bundled fonts, the default dark palette, and the
harness's 16 px font parameter. No CPU affinity, hardware counters,
allocation counters, or cache-hit counters were collected. This is a
shared virtual-machine sample, not an interactive-frame-rate guarantee.

## Raster-size and workload checks

Each run checks 23 gallery types and five stress fixtures at 320, 640,
960, 1600, and 4096 px, for 140 diagram/width pairs. All four runs have
identical statuses and output dimensions for every pair; the
[coverage comparison](benchmarks/mermaid-style/coverage-comparison.csv)
lists the before/after values explicitly.

| Cases per run | Before rendered / fallback | Styled rendered / fallback |
| --- | ---: | ---: |
| 23 gallery types × 5 widths | 74 / 41 | 74 / 41 |
| Five stress fixtures × 5 widths | 17 / 8 | 17 / 8 |
| Total | 91 / 49 | 91 / 49 |

All 23 gallery types render at 4096 px in both versions. Every successful
raster is nonblank, agrees with `size_for`, and passes the harness's
width, height, pixel-count, and buffer-length assertions. These checks
establish output-size equivalence, not identical SVG geometry or pixel
colors; different visible styling is expected.

All 55 metric rows also match across all four runs in rounds,
operations, successes, fallbacks, output pixels, and output bytes.
The timing tables below focus on the two positive fixtures that render
throughout the measured application-paint workloads. The three
long-edge/LR/descriptive-message fixtures remain in every raw CSV;
their source-fallback timings are not presented as diagram speedups.

## Fresh preparation and painting costs

Values are milliseconds per completed operation, averaged across both
equal-sized runs. Each version contributes 100 timed operations per
one-operation phase and 600 per six-operation revisit phase; rounding
is for readability, not a claim of measurement precision.

| Phase | Rectangular before | Rectangular styled | Tall sequence before | Tall sequence styled |
| --- | ---: | ---: | ---: | ---: |
| Uncached prepare, global fonts warm | 5.206 | 5.060 | 3.157 | 3.124 |
| First viewport paint | 3.834 | 3.818 | 6.150 | 6.064 |
| Same viewport repaint | 0.588 | 0.543 | 1.431 | 1.494 |
| Width-revisit viewport paint | 0.556 | 0.547 | 1.399 | 1.396 |
| Scroll-revisit viewport paint | 0.569 | 0.707 | 1.385 | 1.627 |
| First whole-block paint | 3.923 | 3.806 | 23.979 | 23.584 |
| First public full-image export | 3.631 | 3.797 | 28.291 | 27.108 |
| Repeated public full-image export | 3.768 | 3.770 | 27.198 | 27.532 |
| Width-revisit public full-image export | 3.587 | 3.635 | 26.650 | 26.992 |

Preparation includes parsing, graph layout, SVG production and parsing,
the new style pass, and prepared-object destruction, with global fonts
already initialized. It is not a separately isolated measurement of the
style pass. The one process-first tiny warmup diagram takes 1.832/1.997 ms
before and 2.238/1.900 ms styled; those single samples are not stress-case
cold-start estimates.

The first rectangular 1,762-call size sweep averages about 4.08 ms before
and 4.29 ms styled per complete sweep, including lazy compact-scene
preparation. Warm size sweeps average approximately 0.03 microseconds
per call for both positive fixtures and versions. These are large-batch
per-operation means, not nanosecond-precise individual-call latencies.

Raster and paint phases exclude preparation and Markdown layout, but
include output allocation, conversion, and output-buffer destruction.
The first tall viewport still costs about 6.06 ms, and painting the
entire tall block costs about 23.58 ms in the styled samples. Public
full-image export remains uncached; repeated exports are not a test of
the application's band cache.

### Scroll-revisit regression

Both styled runs are slower than both pre-style runs for each positive
fixture's warm scroll revisits. The rectangular regression varies more
between runs, while the tall-sequence regression is consistent in this
small sample.

| Fixture | Before run 1 | Styled run 1 | Before run 2 | Styled run 2 |
| --- | ---: | ---: | ---: | ---: |
| Rectangular, ms/op | 0.556 | 0.778 | 0.582 | 0.636 |
| Tall sequence, ms/op | 1.384 | 1.610 | 1.387 | 1.644 |

The rectangular scroll workload produces 337,920,000 output pixels
across 600 timed operations in each version. The tall scroll workload
produces 633,600,000 pixels across 600 operations in each version.
The higher latency therefore cannot be explained by different output
dimensions or fallback counts in these measurements.

A bounded read-only review found no obvious cache-contract change:
`layout/paint.rs` is byte-identical between versions, including its
canonical keys, 256-row bands, 64 MiB byte limit, and 512-entry limit.
The harness warms all three scroll offsets before timing the six revisits
on the same renderer and layout. Even the entire 364 × 4787 tall diagram
would occupy only about 6.65 MiB and 19 bands, so capacity eviction is
not expected here; actual hits and misses were not instrumented.

Warm paint still allocates a destination, looks up and touches cached
bands, composites their pixels, converts to straight alpha, and drops
the output. The blitter branches between opaque copy, transparent skip,
and partial-alpha blending; conversion performs RGB division only for
partial alpha. Stroke and radius changes can alter coverage, and
scrolling samples different regions from a top-only repaint, but this
review did not measure those effects or establish a cause. The
preparation-only XML style pass cannot directly explain a warmed
repaint's timed work, so the regression remains an observed tradeoff,
not an attributed explanation.

### Coarse process CPU totals

These totals sum process user+system CPU ticks over the same timed
intervals in both runs. The clock runs at 100 Hz, so each interval is
quantized in 10 ms steps; adding intervals does not create sub-tick
precision, and short phases can disagree with wall-time trends.

| Fixture and phase | Operations | Output pixels per version | Before CPU, ms | Styled CPU, ms |
| --- | ---: | ---: | ---: | ---: |
| Rectangular preparation | 100 | No raster requested | 530 | 500 |
| Rectangular first viewport | 100 | 63,360,000 | 350 | 390 |
| Rectangular width revisit | 600 | 369,600,000 | 350 | 320 |
| Rectangular scroll revisit | 600 | 337,920,000 | 340 | 450 |
| Tall preparation | 100 | No raster requested | 340 | 320 |
| Tall first viewport | 100 | 105,600,000 | 620 | 630 |
| Tall width revisit | 600 | 616,000,000 | 810 | 950 |
| Tall scroll revisit | 600 | 633,600,000 | 890 | 1,010 |
| Tall whole-block paint | 100 | 464,640,000 | 2,410 | 2,360 |

These are not hardware-cycle counts, precise per-call CPU measurements,
or idle-CPU measurements. The benchmark excludes PTY output, GPU upload,
terminal scheduling, and window-system presentation, and does not time
light-theme performance.

## Raw evidence and reproduction

The final raw outputs are preserved without filtering or relabeling.
Each contains all 140 coverage rows, 55 metric rows, and process metadata.

- [Before run 1](benchmarks/mermaid-style/before-1.csv)
- [Styled run 1](benchmarks/mermaid-style/after-1.csv)
- [Before run 2](benchmarks/mermaid-style/before-2.csv)
- [Styled run 2](benchmarks/mermaid-style/after-2.csv)
- [Coverage comparison](benchmarks/mermaid-style/coverage-comparison.csv)
- [Environment and hashes](benchmarks/mermaid-style/environment.txt)

After building each revision separately and preserving its executable,
run the binaries in the recorded alternating order. The first pair's
commands are below; repeat with `run2` labels and `-2.csv` output names.

```sh
/tmp/toastty-style-bench-binaries/before-9651fa3 \
  --label before-style-9651-run1 --rounds 50 --size-passes 32 \
  > before-1.csv
/tmp/toastty-style-bench-binaries/after-style-final \
  --label after-style-run1 --rounds 50 --size-passes 32 \
  > after-1.csv
```

To compare coverage and workload identity independently of timing or
labels, run the following Bash checks from the repository root. A
successful comparison prints nothing; all four final files passed.

```bash
base=docs/benchmarks/mermaid-style/before-1.csv
for candidate in docs/benchmarks/mermaid-style/{before-2,after-1,after-2}.csv; do
  diff -u \
    <(awk -F, '$1=="coverage"{print $3,$4,$5,$6,$7}' "$base") \
    <(awk -F, '$1=="coverage"{print $3,$4,$5,$6,$7}' "$candidate")
  diff -u \
    <(awk -F, '$1=="metric"{print $3,$4,$5,$6,$7,$8,$9,$10}' "$base") \
    <(awk -F, '$1=="metric"{print $3,$4,$5,$6,$7,$8,$9,$10}' "$candidate")
done
```

The [harness documentation](mermaid-benchmark.md) defines every phase,
batch percentile, and fallback classification. These two alternating
pairs characterize this machine and fixture set; they do not justify a
universal speed promise or a causal performance claim about the style
pass in isolation.
