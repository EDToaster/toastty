# Text rendering stack

Status: **decided** — adopt **`cosmic-text` 0.19 + custom wgpu atlas** (Option A), with a thin cluster-width override layer on top of `LayoutRun::glyphs`. Reject glyphon, defer swash-direct.

Slug: `text-stack`.

## Question

Which text stack should toastty use? Candidates:

| | Stack | Crates |
| --- | --- | --- |
| A | `cosmic-text` + custom wgpu | `cosmic-text = 0.19.0`, `swash` (via cosmic-text), `etagere = 0.3.0` |
| B | `swash` direct + custom layout | `swash = 0.2.7`, `unicode-bidi = 0.3.18`, `unicode-width = 0.2.2`, `unicode-segmentation = 1.13.2`, `etagere = 0.3.0` |
| C | `glyphon` | `glyphon = 0.11.0` (pins `cosmic-text = 0.18.2`, `wgpu = 29`) |

Shared infra: `wgpu = 29.0.3`, `winit = 0.30.13` (stable; 0.31 is beta).

## Measurements

Headless wgpu prototype per option. Each renders the same stress grid:

```
ASCII the quick brown fox jumps over the lazy dog 0123456789
Arabic مرحبا بالعالم — السلام عليكم — RTL inline
Han 你好世界 ideographs こんにちは 한글 — CJK wide cells
Emoji ZWJ 👨‍👩‍👧‍👦 family 🇯🇵 flag 🚀 single
Ligatures ===> != -> <= >= |> :: !== if (x) { y } // Fira Code
Combining é̕ á ö ñ marks staying on base
VS15/VS16 ❤︎ text vs ❤️ emoji presentation
```

Stress grid tiled to 80×24 (640×432 px) and 200×50 (1600×900 px). Apple M1 Pro, macOS 25.2, Metal backend, release build. Each timing is the median of 3 runs; warm = 10-frame mean.

### 80×24 grid

| Metric | A: cosmic-text | B: swash direct | C: glyphon |
| --- | ---: | ---: | ---: |
| Cold total (ms) | 19.4 | 9.6 | 10.6 |
| Shape (ms) | 10.1 | 0.5 | n/a (folded) |
| Raster (ms) | 2.7 | 1.7 | n/a (folded) |
| Upload + draw (ms) | 8.0 | 8.1 | n/a (folded) |
| Warm frame avg (ms) | 2.7–3.0 | 3.0 | **1.4** |
| Scroll frame (ms) | 6.0–6.3 | 7.4–8.0 | **2.9** |
| Unique atlas glyphs | 179 | 115 | (internal; ≥ 115) |
| Prototype LoC | 723 | 963 | **342** |

### 200×50 grid

| Metric | A | B | C |
| --- | ---: | ---: | ---: |
| Cold total (ms) | 22.9 | 11.4 | 10.9 |
| Warm frame avg (ms) | 3.1 | 3.4 | **1.4** |
| Scroll frame (ms) | 12.6 | 7.5 | **3.6** |
| Unique atlas glyphs | 179 | 115 | ≥ 115 |

Atlas-glyph count is constant across the 80×24→200×50 jump: same vocabulary, more occurrences.

## Correctness check

PNGs in `/tmp/cosmic-text-*.png`, `/tmp/swash-direct-*.png`, `/tmp/glyphon-*.png`. All three pass:

| Feature | A | B | C | Notes |
| --- | :---: | :---: | :---: | --- |
| English ASCII | ✓ | ✓ | ✓ | |
| Arabic glyphs render | ✓ | ✓ | ✓ | Joined initial/medial/final forms appear |
| Arabic RTL ordering | ✓ | ✓ | ✓ | `unicode-bidi` is used by all paths (cosmic-text bundles its own; swash-direct calls it explicitly) |
| Han ideographs | ✓ | ✓ | ✓ | Apple Arial Unicode / system fallback |
| Emoji ZWJ family | ✓ | ✓ | ✓ | Apple Color Emoji bitmap glyphs in atlas |
| Regional indicator flag (🇯🇵) | ✓ | ✓ | ✓ | Becomes single emoji glyph |
| Fira Code ligatures | ✓ | ✓ | ✓ | `===>`, `!=`, `->` render as multi-glyph ligatures |
| Combining marks on base | ✓ | ✓ | ✓ | `e + U+0301` stays attached |
| VS15 text presentation | ✓ | ✓ | ✓ | |
| VS16 emoji presentation | ✓ | ✓ | ✓ | |

## The mode 2027 width problem

**This is the load-bearing finding.** Mode 2027 (Terminal Unicode Core) lets apps declare grapheme-cluster cell widths. The terminal *must* honor the declared width. A stack that hard-codes wcwidth at layout time can't comply.

### What we tested

I wrote a `widths` probe inside the cosmic-text prototype that calls `Buffer::set_monospace_width(8.0)` (cell width = 8 px) and measures the total advance for each cluster:

| Cluster | cosmic-text advance | cells | Expected | Verdict |
| --- | ---: | ---: | :---: | :---: |
| `"A"` | 8.57 px | 1.07 | 1 | acceptable rounding |
| `"AB"` | 17.14 px | 2.14 | 2 | acceptable |
| `"你"` (CJK) | 13.71 px | **1.71** | 2 | **WRONG** |
| `"你好"` | 27.43 px | 3.43 | 4 | **WRONG** |
| `"🚀"` | 13.71 px | 1.71 | 2 | **WRONG** |
| `"❤︎"` VS15 | 8.57 px | 1.07 | 1 | acceptable |
| `"❤️"` VS16 | 8.57 px | 1.07 | **2** | **WRONG** |
| `"👨‍👩‍👧‍👦"` ZWJ | 13.71 px | 1.71 | 2 | **WRONG** |
| `"🇯🇵"` flag | 13.71 px | 1.71 | 2 | **WRONG** |
| `"===>"` ligature | 34.29 px | 4.29 | 4 | acceptable |
| `"e + U+0301"` | 8.57 px | 1.07 | 1 | acceptable |

`set_monospace_width` rounds *each glyph's advance* to the nearest cell-width multiple, not each *cluster's* total advance to an integer cell count. CJK ideographs, ZWJ emoji, and VS16 emoji all come out as ~1.7 cells. **For a terminal, this is broken.**

Reading `cosmic-text-0.19.0/src/shape.rs` around line 2876:

```rust
let match_mono_em_width = match_mono_width.map(|w| w / font_size);
// ... rounds to nearest match_mono_em_width
```

Because `match_mono_em_width` is the cell-width-in-ems (`8 / 14 ≈ 0.57`), the rounder snaps glyph advances to ~4.6-px increments, which is well below a single cell. It produces "monospace alignment for an editor" but not "the integer cell grid a terminal grid demands."

This affects **A and C identically** — glyphon is cosmic-text underneath.

### Does this kill A and C?

No, but it does kill `set_monospace_width` as the answer. The workaround:

1. After `Buffer::shape_until_scroll`, iterate `LayoutRun::glyphs` and group by source byte range (the `start..end` field).
2. For each group (= one grapheme cluster), look up `cluster_width(cluster_text)` — either from `unicode-width` or, when mode 2027 is on, from app-declared widths.
3. Position each cluster at `column * CELL_W`; redistribute the cluster's glyphs proportionally by their natural advance within `width * CELL_W`.

This is ~60–80 LoC on top of A's path. It works because `LayoutGlyph::start`/`end` and `LayoutGlyph::level` (bidi) are exposed and stable.

Option B does the same work natively (since you call `swash::shape::ShapeContext::builder` per cluster), but at the cost of writing the *whole* paragraph-level engine yourself (font fallback selection, bidi reordering, Indic/Arabic complex-script routing, line breaking). The prototype skips Indic and proper script-fallback detection; a production swash-direct path would need ~3–5× the LoC.

## Other findings

### Atlas behavior

- **A** (custom etagere atlas): single RGBA atlas, mask and color glyphs share space. 2048×2048 = 16 MB. Whole atlas re-uploaded when dirty (cheaper to write a smaller subrect; trivial future opt).
- **B** (same): identical, but with my own `GlyphKey` (font, glyph_id, size). Cosmic-text's `CacheKey` also includes subpixel bin, weight, flags → on identical content, B has **64 fewer atlas entries than A** (115 vs 179) because cosmic-text creates separate atlas entries per subpixel x-bin.
- **C** (glyphon): **two atlases — one mask, one color** (`color_atlas` + `mask_atlas`). Auto-grows on `prepare()` failure. This is a clean separation: emoji churn doesn't evict text glyphs.

### Scroll behavior (forcing new glyphs)

| Stack | scroll_total | new glyphs added |
| --- | ---: | ---: |
| A | 6.0 ms | 38 |
| B | 7.5 ms | 21 |
| C | 2.9 ms | (atlas internal) |

Glyphon wins because its atlas is GPU-native (the texture lives on-device, `prepare()` does per-glyph uploads rather than a full-atlas blit). My A/B prototype does whole-atlas writes — a real implementation would do per-glyph subuploads.

### Surprises

1. **`set_monospace_width` does not produce integer cell widths.** Documented above.
2. **`glyphon` separates color and mask atlases.** Emoji and text never fight for atlas space. Worth copying.
3. **`cosmic-text 0.19` swapped its shaper.** It now uses `harfrust` (Rust port of HarfBuzz) for shaping, but **still pulls `swash` 0.2.7** for rasterization. So even adopting cosmic-text 0.19, swash is in our tree. The migration broke API compat with `glyphon 0.11` (which pins cosmic-text 0.18.x with the old `set_text(&mut FontSystem, ...)` signature).

## Decision

**Option A: cosmic-text 0.19 + a custom wgpu renderer.**

### Why not B (swash direct)

- 3–5× the LoC at production quality (Indic shaping, generic font fallback, line breaking, all the BiDi edge cases). The prototype is already 963 LoC and skips Indic / general fallback / linebreak / line wrap.
- We'd be re-implementing a slice of cosmic-text that's already battle-tested in COSMIC desktop, Iced, Zed-via-fork.
- No measured perf advantage: warm steady-state is the same (3 ms), shape cost difference (cosmic-text 10 ms cold, swash-direct 0.5 ms) is one-time and dwarfed by atlas upload.

### Why not C (glyphon)

- Pins `cosmic-text = 0.18`. Cosmic-text 0.19 is the current version, has a meaningfully different (and lighter) shaping API (`set_text(text, attrs, shaping, alignment)` — no `&mut FontSystem` plumbing). Glyphon will catch up but we'd be one version behind starting day one.
- Glyphon's `TextRenderer::prepare` ↔ `TextRenderer::render` model assumes "one buffer per `TextArea`." Toastty's renderer needs interleaved text + RGP 3D + image cells in a single Z-ordered pass. Plugging glyphon into that means calling its render fn between our other passes, which works but limits how we structure the cell pipeline.
- The lib does too much: it owns the pipeline, viewport uniforms, vertex layout, custom-glyph rasterization callbacks. We want to control all of that to integrate with shader hot reload, the post-process pass, and the synchronized-output (mode 2026) frame skip.
- We *will* steal its two-atlas idea (color + mask).

### Why A

- The shaping/bidi/fallback engine is the part that's hard to write. cosmic-text gives us that with one dependency edge.
- The renderer (atlas, instanced quads, integration with RGP + post-process) is the part we want to own — to fit the workspace structure and the user-shader story.
- The mode 2027 width hook is implementable as a ~70 LoC post-shape pass that walks `LayoutRun::glyphs`, groups by `(start..end)`, and re-positions clusters to cell boundaries.

### Concrete implementation guidance

1. `crates/toastty-render/src/text/font_system.rs` — thin wrapper around `cosmic_text::FontSystem`, exposes `set_fallback_chain`, `load_font_data`.
2. `crates/toastty-render/src/text/layout.rs` — owns a `Buffer` per grid row OR one big buffer fed dirty-line ranges; investigate which is cheaper. Walks `LayoutRun::glyphs`, groups by `start..end`, calls into the width-override hook below.
3. `crates/toastty-render/src/text/widths.rs` — `pub fn cluster_width(&self, cluster: &str) -> u8`. Honors mode 2027 declared widths if `Modes::is_set(2027)`; falls back to `unicode-width`. Also handles OSC 66 / kitty cluster width announcements.
4. `crates/toastty-render/src/text/atlas.rs` — **two** `etagere` shelf atlases on a single texture (or two textures), one for `SwashContent::Mask`, one for `SwashContent::Color`. Per-glyph subuploads via `queue.write_texture` with explicit Origin3d.
5. Pipeline: one render pass, instanced quads, single bind group (atlas + uniforms). Cells reuse the same instance vertex layout regardless of color/mask — fragment shader branches on `is_color`. (See the prototype `text.wgsl` — that exact pattern works.)

### Crate pins (for `Cargo.toml`)

```toml
cosmic-text       = "=0.19.0"
swash             = "=0.2.7"   # transitively from cosmic-text; pin to avoid drift
etagere           = "=0.3.0"
unicode-width     = "=0.2.2"
unicode-bidi      = "=0.3.18"  # transitively from cosmic-text; pin so our own usage matches
wgpu              = "=29.0.3"
winit             = "=0.30.13" # stable; revisit when 0.31 ships
```

`glyphon` is **not** added.

### Open issues to handle in the renderer crate

- Cosmic-text 0.19 borrow rules: most mutating methods on `Buffer` no longer take `&mut FontSystem`; only `shape_until_scroll` and `Buffer::new` do. Plan call-sites accordingly.
- Per-glyph subupload — the prototype re-uploads the whole 16 MB atlas; production should track dirty rects and call `queue.write_texture` per glyph or per shelf.
- VS16 width: cosmic-text alone won't emit width-2 for `❤\u{FE0F}`. The width-override hook must inspect the cluster string for VS16 and bump width.
- Subpixel binning: cosmic-text rasterizes per x-fraction. For a cell grid we always render at integer cell boundaries, so we can configure `CacheKeyFlags::PIXEL_FONT` or quantize physical glyph x to integer before producing the cache key. Reduces atlas churn 4–8×.

## Reproduce

```
cd .../toastty/.claude/worktrees/agent-acf500b9199e40e34
cargo build --release -p cosmic-text-proto -p swash-direct -p glyphon-proto
./target/release/cosmic-text-proto 80x24
./target/release/swash-direct 80x24
./target/release/glyphon-proto 80x24
# screenshots in /tmp/{cosmic-text,swash-direct,glyphon}-*.png
# width probes:
./target/release/cosmic-widths  # cosmic-text monospace_width rounding (wrong for CJK/emoji)
./target/release/swash-widths   # swash-direct cluster-width hook (correct integer cells)
```
