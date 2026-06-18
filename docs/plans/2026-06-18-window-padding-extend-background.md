<!-- Generated 2026-06-18 by the plan-window-padding-extend-bg planning workflow (13 agents). -->

# Configurable Window Padding + `extend_background` (Edge-Cell Bleed)

## Summary

Add two `[window]` config knobs to toastty:

- **`[window.padding]`** — `{ top, right, bottom, left }` (each `u16`, logical px, default `0`) — insets the cell grid from the window edges.
- **`extend_background`** — `Never | Always | AltScreen` (kebab-case: `"never" | "always" | "alt-screen"`, default `Never`) — paints each *edge* cell's background outward into the padding to the physical window edge ("overscan/bleed"), so a full-page TUI's colors don't disappear at the border.

The rendering approach is **Option B1**: a per-pipeline `content_origin = (pad_left, pad_top)` physical-px uniform is added to the px-space position in each vertex shader *before* the px→NDC map. The viewport (the px→NDC divisor) stays full-surface. CPU instance positions stay `col*cell_w`. The edge-bleed is implemented entirely on the CPU in the bg-quad emission by *growing* edge cells' quads outward in pre-origin space.

`toastty-config` stays a leaf crate (no `toastty-render` dep, `lib.rs:1-20`); the binary bridges config enums → renderer enums exactly as it already does for `Theme` / `ScrollButtonCorner`.

Key verified anchors: `ConfirmClose` enum template (`crates/toastty-config/src/window.rs:6-17`); `WindowConfig` `#[serde(default, deny_unknown_fields)]` + `defaults()` (`window.rs:20-56`); re-exports (`lib.rs:47`, currently `pub use window::{ConfirmClose, WindowConfig};`); `GlobalsUbo` (`crates/toastty-render/src/text/pipeline.rs:23-36`) and WGSL `Globals` (`crates/toastty-render/shaders/text.wgsl:24-37`); text vertex px,py (`text.wgsl:78-79`); `ImageGlobals` (`crates/toastty-render/src/image/pipeline.rs:18-21`) and `crates/toastty-render/shaders/image.wgsl:18-23,52-56`; RGP `render()` with `ortho_screen` (`crates/toastty-render/src/rgp/pipeline.rs:164-185`, `center_px_x/center_px_y` at `:202-203`) and `ortho_screen` (`crates/toastty-render/src/rgp/matrix.rs:118-127`); bg/dirty builders (`crates/toastty-render/src/text/instance.rs:540-616`, `:792-845`); GlobalsUbo fill + cursor_rect (`crates/toastty-render/src/lib.rs:2154-2169`), full-surface viewport tuple (`:2263`), image/rgp call sites (`:2270`, `:2295`, `:2309`, `:2325`), `LoadOp::Clear(premultiplied_color(theme.bg))` (`:2181`); `grid_dims_from_pixels` returns `(cols, rows)` and `effective_font_size_px` (`crates/toastty/src/geometry.rs:9,33`); `resync_grid` (`crates/toastty/src/main.rs:623-652`); `pixel_to_cell` (`crates/toastty/src/mouse.rs:80-91`); `Term::is_alt_active()` (`crates/toastty-term/src/term.rs:1136`).

---

## Config schema

All changes confined to `crates/toastty-config`. Mirror `ConfirmClose` (`window.rs:6-17`) and `WindowConfig` (`window.rs:20-56`) style exactly.

### `ExtendBackground` enum (new, in `window.rs`)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ExtendBackground {
    #[default]
    Never,
    Always,
    AltScreen, // serializes as "alt-screen"
}
```

### `PaddingConfig` struct (new, in `window.rs`)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PaddingConfig {
    pub top: u16,
    pub right: u16,
    pub bottom: u16,
    pub left: u16,
}
impl PaddingConfig {
    #[must_use]
    pub fn defaults() -> Self { Self { top: 0, right: 0, bottom: 0, left: 0 } }
}
impl Default for PaddingConfig {
    fn default() -> Self { Self::defaults() }
}
```

`#[serde(default, deny_unknown_fields)]` is on `PaddingConfig` itself (not just `WindowConfig`) so a partial `[window.padding]` table fills missing sides with `0` and a typo'd key is rejected. The manual `defaults()` matches the house style (`WindowConfig::defaults`, `ScrollButtonConfig::defaults`).

### Wire into `WindowConfig` (`window.rs:20-56`)

Add the two fields, **`padding` declared LAST**:

```rust
    pub confirm_close: ConfirmClose,
    pub extend_background: ExtendBackground,
    pub padding: PaddingConfig, // MUST be last — see Risks
```

and in `defaults()`:

```rust
    confirm_close: ConfirmClose::IfRunningProgram,
    extend_background: ExtendBackground::Never,
    padding: PaddingConfig::defaults(),
```

`WindowConfig` stays `Copy` (both new types are `Copy`).

### Re-export (`lib.rs:47`)

Replace `pub use window::{ConfirmClose, WindowConfig};` with:

```rust
pub use window::{ConfirmClose, ExtendBackground, PaddingConfig, WindowConfig};
```

No change to the top-level `Config` struct or `Config::defaults` — these fields live under the existing `window: WindowConfig`. The binary reads `config.window.padding` / `config.window.extend_background`.

### Fixture (`tests/fixtures/full.toml`, extend the `[window]` block)

```toml
[window]
width             = 1280
height            = 800
vsync             = true
confirm_close     = "always"
extend_background = "alt-screen"

[window.padding]
top    = 8
right  = 8
bottom = 8
left   = 8
```

Insert `[window.padding]` right after `confirm_close`/`extend_background` and **before `[scroll_button]`** (currently begins right after the `[window]` block). File placement is parse-tolerant (`fully_populated_toml_parses` round-trips the parsed `Config`, not the file text, `lib.rs:223`); the only hard requirement is the in-memory struct field order (`padding` LAST), enforced by `defaults_round_trip_through_toml`.

---

## Rendering design (origin uniform + shaders)

There are **three independent globals paths**; they are NOT shared, and RGP is structurally different. Shader files live under **`crates/toastty-render/shaders/`** (verified `include_str!("../../shaders/...")` from `text/pipeline.rs:73`, `image/pipeline.rs:47`, `rgp/pipeline.rs:57`). Do NOT edit the stale duplicate tree under `.claude/worktrees/.../crates/toastty-render/shaders/`.

### Text pipeline — `crates/toastty-render/shaders/text.wgsl` + `GlobalsUbo`

Has a dedicated shared globals buffer at `@group(0) @binding(0)`, used by both bg and glyph passes (`pipeline.rs:57`). The buffer is sized via `size_of::<GlobalsUbo>()` and uploaded with `bytemuck::bytes_of`, and `min_binding_size: None` on binding 0 — so adding a field is automatic, no layout-size edit.

**CPU struct** (`pipeline.rs:23-36`) — append a `vec4` (not `vec2`, to keep the clean 16-byte std140 stride and a `Pod` layout with no implicit padding). 48 → 64 bytes, stays 4×vec4 / 16-aligned:

```rust
    pub cursor_color: [f32; 4],
    /// (origin_x_px, origin_y_px, _pad, _pad). Added to px,py in the
    /// vertex shader before the px->NDC map (window-padding inset).
    pub content_origin: [f32; 4],
```

**WGSL `Globals`** (`crates/toastty-render/shaders/text.wgsl:24-37`) — append matching field:

```wgsl
    cursor_color: vec4<f32>,
    content_origin: vec4<f32>, // (origin_x_px, origin_y_px, _, _)
```

**Vertex edit** (`text.wgsl:78-79`):

```wgsl
    let px = inst.pos.x + cx * inst.size.x + globals.content_origin.x;
    let py = inst.pos.y + cy * inst.size.y + globals.content_origin.y;
```

The clip map (`:83-86`) and atlas-UV math (`:89-92`) are unchanged. `fs_bg`/`fs_glyph` compare raw `in.clip.xy` (post-origin framebuffer px) against `cursor_rect`, so the cursor rect must be **pre-offset on the CPU** (see Renderer plumbing §"GlobalsUbo fill"). Do NOT offset cursor_rect in the shader.

### Image pipeline — `crates/toastty-render/shaders/image.wgsl` + `ImageGlobals`

Has its own dedicated globals buffer, rewritten every `render()` (`image/pipeline.rs:135` alloc, `:277-281` write). The `tex_dims` field is documented-dead (`image.wgsl:20-22`). **Repurpose it as `content_origin`** — keeps the struct at exactly 16 bytes so `min_binding_size: NonZeroU64::new(size_of::<ImageGlobals>())` (`pipeline.rs:60-62`) stays valid with no edit.

CPU (`image/pipeline.rs:18-21`):

```rust
pub struct ImageGlobals {
    pub viewport: [f32; 2],
    pub content_origin: [f32; 2], // was tex_dims (unused)
}
```

WGSL (`image.wgsl:18-23`): rename `tex_dims` → `content_origin`, **and update the doc comment at `image.wgsl:20-22`** (currently documents the field as "not currently used") so a future reader does not re-dead the field.

Vertex edit (`image.wgsl:52-56`):

```wgsl
    let p = instance.pos + c * instance.size + globals.content_origin;
```

`ImagePipeline::render(...)` gains a `content_origin: [f32; 2]` param. **The per-frame write at `image/pipeline.rs:279` currently hardcodes `tex_dims: [0.0, 0.0]` — change that line to `content_origin: <param>`.** The signature change propagates to **three** call sites (`lib.rs:2270`, `:2309`, `:2325` — verified the only `img_pipe.render(` sites).

### RGP pipeline — **no shared globals UBO** (resolved conflict)

**Genuine conflict with locked decision #3** ("add content_origin to the … rgp globals UBOs"): RGP has no shared/per-frame globals buffer. The only uniform is the per-draw `DrawUniforms { mvp, normal, color_tint }` (`rgp/pipeline.rs:23-27`, `shaders/rgp.wgsl:16-23`), and the vertex shader does `out.clip = draw.mvp * vec4(...)` (`rgp.wgsl:48`) — there is no separate px→NDC stage to patch and no per-frame field to add.

**Resolution (architect decision): leave `crates/toastty-render/shaders/rgp.wgsl` byte-for-byte unchanged, and fold the origin into the CPU-side per-placement anchor — NOT into a new matrix.** This is the least error-prone option and makes the Y-sign self-evident next to the existing comment.

`render()` (`rgp/pipeline.rs:164`) computes `proj = ortho_screen(viewport.0, viewport.1)` (`:185`) — leave `ortho_screen` (`matrix.rs:118-127`) **untouched**. The per-placement anchor at `:202-203` is:

```rust
let center_px_x = (f32::from(p.anchor.col) + 0.5) * cell_size.0;
let center_px_y = viewport.1 - (f32::from(p.anchor.row) + 0.5) * cell_size.1;
```

Thread `content_origin: (f32, f32)` (= `(pad_left, pad_top)`, physical px) into `render()` and bias the anchor:

```rust
let center_px_x = (f32::from(p.anchor.col) + 0.5) * cell_size.0 + content_origin.0;       // +pad_left
let center_px_y = viewport.1 - (f32::from(p.anchor.row) + 0.5) * cell_size.1 - content_origin.1; // -pad_top
```

**Y-sign derivation (exact, must be unit-tested):** `ortho_screen` maps `worldY → ndc = 2*worldY/h - 1` over the **full-surface** `h`. The text path puts a content-px cell at `py = r*cell_h` at physical `y = r*cell_h + pad_top`. For an RGP placement to land at the same physical y, its world Y must shift by `-pad_top` (a screen-down inset is a world-Y-**down** shift, since RGP world is Y-up). Hence `center_px_y` subtracts `pad_top`. X is a straight `+pad_left`. This keeps `ortho_screen` and `rgp.wgsl` unchanged; the entire origin fold is two `+`/`-` terms next to the existing Y-inversion comment. (We deliberately do NOT compose a `translate` into the MVP, which would be wrong if applied post-projection in NDC units, and do NOT add `ortho_screen_with_origin`.)

### build.rs validity

`crates/toastty-render/build.rs` validates every `shaders/*.wgsl` with `naga`. The text + image edits are one struct member + one `+ content_origin` per affected vertex line — valid WGSL, `globals` already bound. `rgp.wgsl` is untouched. No new bindings/layout attributes → validation passes.

### Alignment summary

- text `GlobalsUbo`: 48 → 64 bytes, 4×vec4 / 16-aligned, `Pod` clean (every field `[f32;4]`). Buffer + upload auto-size.
- image `ImageGlobals`: stays 16 bytes (rename dead `tex_dims`); `min_binding_size` unchanged.
- RGP: no UBO change.

---

## Edge-background extension (incl. partial-redraw correctness)

Implemented purely on the CPU in `crates/toastty-render/src/text/instance.rs`. Instances are emitted in **pre-origin space** (`pos = [col*cell_w, row*cell_h (+y_translate)]`, `:586,816`); the shader adds `content_origin` afterward. To make an edge cell's bg reach the physical window edge, grow its bg quad outward in pre-origin space:

- **Left (`col 0`):** `pos.x -= pad_left; size.x += pad_left` → after shader `+pad_left`, left lands at `x=0`.
- **Right (last col):** `size.x += pad_right` → right lands at `surface_w`.
- **Top:** `pos.y -= pad_top; size.y += pad_top`.
- **Bottom:** `size.y += pad_bottom`.

Corner cells satisfy two predicates and compose both grows automatically — no dedicated corner code. This is **exact** bleed, and because the bg pass uses **REPLACE blend (One/Zero)**, grown quads must never overlap a neighbor's cell rect — they only extend into the padding gutter (occupied by nobody else); corner overlaps are confined to corner padding. The `LoadOp::Clear(premultiplied_color(theme.bg))` (`lib.rs:2181`) paints the whole attachment (including padding) to base bg first, so bleed only overpaints the gutter.

### The bleed parameter (single resolved type)

Define **one** `Copy` struct in `instance.rs` next to `CellInstance`, re-exported from `lib.rs`. **Decision: name it `EdgeBleed`** (the renderer-plumbing section's `BleedParams` is the same type; both areas import `crate::text::instance::EdgeBleed`). Field order `[top, right, bottom, left]` matches the pad ordering everywhere:

```rust
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct EdgeBleed {
    /// Physical-px padding [top, right, bottom, left].
    pub pad: [f32; 4],
    /// Pre-resolved gate: mode==Always || (mode==AltScreen && term.is_alt_active()).
    pub enabled: bool,
}
impl EdgeBleed {
    const TOP: usize = 0; const RIGHT: usize = 1;
    const BOTTOM: usize = 2; const LEFT: usize = 3;
}
```

The `AltScreen && is_alt_active()` gate is resolved to a `bool` by `render_term` (where `Term` and config meet), keeping this leaf code trivially unit-testable. `EdgeBleed::default()` (`enabled=false`, zero pad) is a no-op so existing callers/benches pass it.

> **Ordering note (two intentional, distinct orderings — do NOT "fix" them into agreement):** the **pads** are `[top, right, bottom, left]` (EdgeBleed.pad, `set_padding(top,right,bottom,left)`, `PaddingConfig`). The **origin** is `(x = pad_left, y = pad_top)` — a left-then-top `(x,y)` pair. `content_rect_from_padding` returns `(origin_x, origin_y, …)` = (left-derived, top-derived), matching the origin convention. These are correct as-is.

### The grow helper

```rust
#[inline]
fn extend_edge_quad(
    mut pos: [f32; 2], mut size: [f32; 2],
    c: u16, r: u16, cols: u16, rows: u16, top_row: u16,
    is_wide: bool, bleed: EdgeBleed,
) -> ([f32; 2], [f32; 2]) {
    if !bleed.enabled { return (pos, size); }
    if c == 0 { pos[0] -= bleed.pad[EdgeBleed::LEFT]; size[0] += bleed.pad[EdgeBleed::LEFT]; }
    // Wide primary occupies cols-2 (continuation at cols-1) — OR it in.
    let touches_right = c == cols.saturating_sub(1)
        || (is_wide && c + 1 == cols.saturating_sub(1));
    if touches_right { size[0] += bleed.pad[EdgeBleed::RIGHT]; }
    if r == top_row { pos[1] -= bleed.pad[EdgeBleed::TOP]; size[1] += bleed.pad[EdgeBleed::TOP]; }
    if r == rows.saturating_sub(1) { size[1] += bleed.pad[EdgeBleed::BOTTOM]; }
    (pos, size)
}
```

### Edge detection, content dims, and the two row spaces (resolved)

Both builders bind `let (rows, cols) = term.size();` (`:536, :768`). **Edge-bleed correctness depends on Stage 3's content-aware `term.resize`** (`main.rs:633`): once that lands, `term.size()` reflects the **content** grid, so `cols`/`rows` are the real grid extent for last-col/last-row detection — no extra plumbing. With `EdgeBleed::default()` (`enabled=false`) Stage 2 is a pure no-op, so the workspace is correct at the stage boundary; **bleed must not be exercised/asserted until Stage 3 lands.** (The instance unit tests construct their own `Term` via `feed`, so they control `term.size()` directly and are unaffected by the staging.)

**The two builders iterate in DIFFERENT row spaces — the top-row index differs per builder:**

- **Full builder** (`build_instances_into`): iterates `for r in 0..rows_rendered` where `rows_rendered = rows + pixel_extra` (`:547,561`) — **RENDER space**. When `view_offset_pixel > 0` (`pixel_extra = 1`, `y_translate = view_pixel - cell_h`, `:546-552`) an extra partial row is rendered at the top (`r==0`). The real content-top row is therefore `r == pixel_extra`. Pass **`top_row = pixel_extra`** here.
- **Dirty builder** (`build_dirty_instances_into`): iterates `for (r, row_damage) in damage.iter_rows()` (`:792`), which yields only **damaged** rows in **CONTENT-row space** `0..rows` (`damage.iter_rows` filters non-empty rows of a `self.rows`-sized vec, `damage.rs:162-173`; `Damage.rows` is sized to the content row count). The content-top row is therefore **`r == 0`**, NOT `pixel_extra`. Pass **`top_row = 0`** in the dirty builder.

> **Correctness consequence (resolved major):** passing `top_row = pixel_extra` in the dirty path would, when `pixel_extra==1`, grow the *second* content row and leave the real top row (`r==0`) un-bled. The dirty builder MUST receive `top_row = 0`. (See the dedicated dirty-builder unit test below pinning this at `pixel_extra==1` with a sparse damage set.)

Bottom edge = `r == rows - 1` in both builders (the real content extent, NOT `rows_rendered - 1` — see the fractional-scroll artifact note for the deliberate accepted imprecision during active scroll).

### Wiring into `build_instances_into` (bg quad, `:607-616`)

Compute `is_wide` from the same next-is-continuation check already used for `bg_w` (`:593-601`), pass `[bg_w, cell_h]` as the size, replace the push:

```rust
let (bg_pos, bg_size) =
    extend_edge_quad(pos, [bg_w, cell_h], c, r, cols, rows, pixel_extra, is_wide, bleed);
out.push(CellInstance { pos: bg_pos, size: bg_size, uv_min: [0.0,0.0], uv_max: [0.0,0.0],
                        fg, bg, flags: FLAG_NO_GLYPH, pad: [0;3] });
```

Only the **bg quad** grows. Glyph quad (`:638`), underline strip, and cursor keep natural geometry — bleed is background-only.

### Wiring into `build_dirty_instances_into` — CRITICAL overpaint case (`:836-845`)

The dirty path has **no `is_blank_for_render` gate**: it resolves `(fg,bg)` for every dirty non-continuation cell and unconditionally emits a bg "overpaint" quad (`:833-845`). Apply the identical `extend_edge_quad` here, **passing `top_row = 0`** (content-row space, per above). This is why decision #5 requires it: when an edge cell that previously bled reverts to default bg, it becomes dirty; the *grown* overpaint quad repaints the gutter with default bg, erasing stale bleed.

**Why the overpaint wipe is exact (two load-bearing facts, now grounded):**
1. A default-bg cell resolves `bg = theme.bg` via `resolve_bg(TColor::Default) => self.bg` (`instance.rs:271`), which is exactly the clear color `premultiplied_color(self.theme.bg)` (`lib.rs:2181`).
2. `fs_bg` outputs premultiplied `(rgb*a, a)` under REPLACE/One-Zero blend (`text.wgsl:140-142`, `:117`). So the grown overpaint quad **REPLACES** stale bleed pixels with the same value `LoadOp::Clear` would have written — correct for both opaque and alpha<1 (transparent) themes.

Forward `bleed` through the `damage.all` delegation to `build_instances_into` (`:749-758`).

> **Dirty-path / fractional-scroll invariant (corrected rationale):** an offset *change* forces a full clear (`lib.rs:1547`, `cur_view != last_view_offset`). At a **held** fractional offset (`pixel_extra==1`) where the offset is identical frame-to-frame, a cursor-blink / RGP-animation redraw CAN run the dirty path at `pixel_extra==1`. The dirty math is correct under this because (a) it uses content-row space with `top_row = 0`, and (b) edge rows are only re-emitted if individually damaged; the steady-state bleed from the prior full frame persists for undamaged rows. (The earlier "dirty path effectively only runs at pixel_extra==0" rationale was wrong and is dropped.)

### Wide / continuation cells

Continuation cells are skipped (`:577, :811`) and never grow — correct, their primary owns the quad. A width-2 primary in the last visual column sits at `c == cols-2` (continuation at `cols-1`); the `is_wide` OR in `extend_edge_quad` makes it bleed right.

### Fractional-scroll top AND bottom slivers (accepted v1 artifact, both edges)

During an active fractional scroll (`pixel_extra==1`, full-redraw frames), `y_translate = view_pixel - cell_h`:

- **Top:** the content-top row (`r==pixel_extra==1` in the full builder) sits at `y = view_pixel`; its top grow pulls the face to `view_pixel - pad_top`, landing (after the shader `+pad_top`) at `view_pixel` — a `view_pixel`-tall sliver of stale color in the top gutter.
- **Bottom:** the row we grow (`r == rows-1`) has its bottom at `(rows-1)*cell_h + view_pixel`, which is `cell_h - view_pixel` **short** of the content bottom (`rows*cell_h`). The genuinely bottom-most rendered row is `r == rows` (index `rows_rendered-1`, the partial sliver row), whose bottom overshoots to `rows*cell_h + view_pixel`. So a `(cell_h - view_pixel)`-tall band of base bg can show in the bottom gutter.

**Both** are cosmetic and transient; **steady state (`pixel_extra==0`) is exact** at every edge. **Accepted for v1**: we keep the simple `top_row = pixel_extra` / bottom = `rows-1` predicates rather than tracking `rows_rendered-1` for the bottom (which would bleed a partial row). The instance unit test `fractional_scroll_top_row_uses_pixel_extra` pins the full-builder top behavior at `pixel_extra==1`.

---

## Renderer plumbing (`crates/toastty-render/src/lib.rs`)

### Surface-size contract (explicit — do NOT confuse with content dims)

**`self.config.width` / `self.config.height` are the wgpu `SurfaceConfiguration` and MUST remain the FULL physical surface size at all times.** They are fed by `renderer.resize()` with the full physical window size (binary keeps this; `main.rs:1823`), and are used as: the blit `Extent3d` (`lib.rs:2353-2354`), the viewport tuple passed to image/rgp shaders (`:2263`), and the px→NDC divisor `viewport_and_atlas.xy` in the GlobalsUbo (`:2161-2163`). **Content dims are NEVER pushed into `resize()`.** They are derived only inside `content_dims()` by subtracting the stored physical pads. The shader `+content_origin` only lands correctly if the px→NDC divisor stays full-surface — that is what makes grown edge quads map past the content area into the gutter rather than being rescaled. (Pushing content dims into `resize()` would simultaneously break the px→NDC map, the bleed clear, and the blit — explicitly forbidden.)

### Render-side `ExtendBackground` enum (mirror `ScrollButtonCorner`, ~`lib.rs:327-337`)

`toastty-config` stays a leaf crate, so define a render-side copy (the binary bridges, same as `Theme`/`ScrollButtonCorner`) and re-export it publicly so `theme_bridge.rs` can map onto it:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtendBackground { Never, Always, AltScreen }

impl ExtendBackground {
    #[must_use]
    fn active(self, alt: bool) -> bool {
        matches!(self, ExtendBackground::Always)
            || (matches!(self, ExtendBackground::AltScreen) && alt) // term.is_alt_active() at term.rs:1136
    }
}
```

### New `Renderer` fields + init

Padding stored in **physical px** (binary pre-scales by `scale_factor`; renderer never sees scale). Add to the struct (`~204-325`) and init in `new()` (`~683-723`) to zero / `Never`:

```rust
    pad_top: u32, pad_right: u32, pad_bottom: u32, pad_left: u32,
    extend_background: ExtendBackground,
```

### Derived helpers (single source of truth, no stale cache)

`content_dims` subtracts the stored physical pads from the **full-surface** `config.width/height`:

```rust
    #[must_use]
    fn content_origin(&self) -> [f32; 2] { [self.pad_left as f32, self.pad_top as f32] } // (x=left, y=top)

    #[must_use]
    fn content_dims(&self, cell: (f32, f32)) -> (f32, f32) {
        // config.width/height are the FULL physical surface (see Surface-size contract).
        let cw = (self.config.width as f32 - (self.pad_left + self.pad_right) as f32).max(cell.0.max(1.0));
        let ch = (self.config.height as f32 - (self.pad_top + self.pad_bottom) as f32).max(cell.1.max(1.0));
        (cw, ch)
    }
```

Also expose a **public** `pub fn content_origin(&self) -> (f32, f32)` getter (physical px) for the mouse handler (see Mouse). This is the single source of truth shared by rendering and hit-testing.

### Public setters (force full clear on change)

```rust
    pub fn set_padding(&mut self, top: u32, right: u32, bottom: u32, left: u32) {
        if (self.pad_top, self.pad_right, self.pad_bottom, self.pad_left) != (top, right, bottom, left) {
            self.pad_top = top; self.pad_right = right; self.pad_bottom = bottom; self.pad_left = left;
            self.needs_full_clear = true;
        }
    }
    pub fn set_extend_background(&mut self, mode: ExtendBackground) {
        if self.extend_background != mode { self.extend_background = mode; self.needs_full_clear = true; }
    }
```

The `!= old` guards avoid spuriously forcing a clear on unchanged config re-push (reload path) and match the `set_theme`/`resize` setter contract.

> **Binary-contract (resolved API):** renderer exposes `set_padding(top, right, bottom, left: u32)` (four physical pads, t/r/b/l order) + `set_extend_background(mode)`; the binary computes physical pads and calls both. Origin is *derived* (`pad_left`, `pad_top`) inside the renderer — the binary does not pass origin separately (it reads it back via the public `content_origin()` getter for the mouse). Origin is thus defined in exactly one place.

### GlobalsUbo fill + cursor pre-offset (`lib.rs:2154-2169`)

```rust
        let origin = self.content_origin();              // [pad_left, pad_top]
        let cursor_rect = if cursor_visible {
            let r = crate::text::instance::cursor_pixel_rect(term, cell_size);
            // Pre-offset: fs_glyph compares raw in.clip.xy (post-origin) vs cursor_rect.
            [r[0]+origin[0], r[1]+origin[1], r[2]+origin[0], r[3]+origin[1]]
        } else { [0.0; 4] };
        let globals = GlobalsUbo {
            // viewport_and_atlas.xy MUST stay full-surface (the px->NDC divisor).
            viewport_and_atlas: [self.config.width as f32, self.config.height as f32,
                                 atlas_dims.0 as f32, atlas_dims.1 as f32],
            cursor_rect,
            cursor_color: self.theme.cursor,
            content_origin: [origin[0], origin[1], 0.0, 0.0],
        };
```

A hidden cursor stays `[0;4]` (degenerate, never matches; origin shift is irrelevant on a zero-area rect).

### Threading origin into image/rgp render

The `viewport` tuple stays full surface (`viewport = (config.width, config.height)`, `:2263`). **There is no `set_viewport` call anywhere in `crates/toastty-render/src` (verified empty grep); keep it that way** — the shader px→NDC divisor must remain the full surface so grown edge quads map past the content area into the gutter. Pass `origin` into each call site:

- `img_pipe.render(.., &insts, viewport, origin)` at `:2270`, `:2309`, `:2325`.
- `rgp_pipe.render(.., viewport, cell_size, origin)` at `:2295` (origin folded into the per-placement anchor, §RGP pipeline).

### Overlay / scroll-button col-row math → content dims (scrim is the exception)

Overlays flow through the same `content_origin` uniform (vertex shader adds origin), so their CPU positions stay content-local (start at 0,0); the change is swapping the full-surface divisor for content dims:

- `draw_banner` call site (`~1913-1924`): pass `content_dims(cell_size)` as `(w, h)`. Function body unchanged.
- `draw_scroll_button` call site (`~1888-1899`): pass content dims so the button anchors to the content corner.
- Debug-overlay width (`~1829`): use content width so it anchors to content top-right.
- IME preedit: anchors off `cursor_pixel_rect` (content-local) and flows through the uniform — leave content-local, do NOT add origin to its `pos`.

**`draw_close_dialog` — split the scrim from the box (resolved UX issue).** `draw_close_dialog` emits TWO things (`lib.rs:1169-1178`+): (a) a full-viewport scrim quad `pos:[0,0] size:[width,height]`, and (b) the centered dialog box. The **box** should be centered on content dims, but the **scrim** should dim the WHOLE window (typical modal) — if the scrim is routed through content dims it covers only `[pad_left,pad_top]..[+content_w,+content_h]`, leaving the gutter UNDIMMED (and, with `extend_background=Always`, showing bled edge colors at full brightness around a dimmed modal — looks broken). **Decision:** the scrim quad specifically must cover the full surface. Since it flows through the `content_origin` uniform (which adds `+origin`), emit the scrim in pre-origin space as `pos = [-pad_left, -pad_top]`, `size = [config.width, config.height]` so after the uniform it spans `[0,0]..[surface_w, surface_h]`. Compute the box layout from content dims as above. (Concretely: `draw_close_dialog` gains both full-surface dims — for the scrim — and content dims — for the box layout; or the scrim is emitted at the call site in full-surface pre-origin space and the box uses content dims.)

**`scroll_button_rect` (`~1338-1359`) is special** — it returns a **physical-space** rect hit-tested against raw mouse px, so bake the origin in here on the CPU (not via the shader):

```rust
        let (cdw, cdh) = self.content_dims(cell_size);
        let (left_col, top_row) =
            Self::scroll_button_cell_origin(cdw as u32, cdh as u32, cell_size, corner)?;
        let origin = self.content_origin();
        let x0 = left_col as f32 * cw + origin[0];
        let y0 = top_row  as f32 * ch + origin[1];
        Some([x0, y0, x0 + COLS*cw, y0 + ROWS*ch])
```

### `needs_full_clear` triggers + scratch staleness

- Padding / extend-mode change → the setters force it.
- **Alt-screen toggle when `mode == AltScreen`:** `enter_alt_screen`/`exit_alt_screen` both call `mark_all_dirty()` (`term.rs:2925, :2946`); `render_term` cascades `term.damage().all` into `needs_full_clear` (`lib.rs:2173-2175`). So crossing the `AltScreen` gate already forces a full clear (which re-emits every bg quad with/without bleed; the `LoadOp::Clear` repaints the gutter to base bg first) — **no new trigger needed.**
- **Scratch-staleness safety net (load-bearing for the gutter):** a full-clear frame renders direct-to-swapchain (`render_direct = needs_full_clear`, `lib.rs:2204`) and leaves `scratch_stale = true` (`:2393`); the next partial frame then **forces another full clear** (`:1747-1749`, `if !needs_full_clear && !damage.all && scratch_stale { needs_full_clear = true }`). So a setter that flips `needs_full_clear` for ONE frame is sufficient: the transition frame does a direct full clear (gutter painted by `LoadOp::Clear`), and the follow-up frame can't leak stale scratch pixels into the gutter. This is the mechanism that closes the padding-change staleness window.
- Ordinary partial-redraw frames force NO extra clear — the dirty builder's grown overpaint quad handles per-frame bleed staleness (decision #5).

### Forwarders → instance builders

The two private forwarders `build_term_instances_into` (`~56`) and `build_term_dirty_instances_into` (`~81`) gain an `EdgeBleed` param forwarded to `build_instances_into` / `build_dirty_instances_into`. At the `render_term` call sites (`~1784` full, `~1796` dirty) compute it once:

```rust
        let bleed = crate::text::instance::EdgeBleed {
            enabled: self.extend_background.active(term.is_alt_active()),
            pad: [self.pad_top as f32, self.pad_right as f32,    // [top, right, bottom, left]
                  self.pad_bottom as f32, self.pad_left as f32],
        };
```

The pad ordering here `[top, right, bottom, left]` matches `EdgeBleed::{TOP,RIGHT,BOTTOM,LEFT}` and `set_padding`'s arg order — keep distinct from `content_origin`'s `(x=left, y=top)`.

---

## Binary integration (DPI + reload) — `crates/toastty`

The binary bridges `toastty_config::WindowConfig` (logical px) → renderer (physical px). Padding is logical px scaled like `effective_font_size_px` (`geometry.rs:33`), which uses **`.round()`** — match that everywhere (see DPI consistency).

### `geometry.rs` — `content_rect_from_padding` (single scaling site)

```rust
#[must_use]
pub fn content_rect_from_padding(
    surface: (u32, u32),
    pad: (u16, u16, u16, u16), // top, right, bottom, left (matches PaddingConfig order)
    scale_factor: f64,
) -> (u32, u32, u32, u32) {     // (origin_x, origin_y, content_w, content_h) physical px
    let scale = |v: u16| {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let s = (f64::from(v) * scale_factor).round() as u32; s   // .round(), matching effective_font_size_px
    };
    let (sw, sh) = surface;
    let (pt, pr, pb, pl) = (scale(pad.0), scale(pad.1), scale(pad.2), scale(pad.3));
    let origin_x = pl.min(sw.saturating_sub(1));
    let origin_y = pt.min(sh.saturating_sub(1));
    let content_w = sw.saturating_sub(pl + pr).max(1);
    let content_h = sh.saturating_sub(pt + pb).max(1);
    (origin_x, origin_y, content_w, content_h)
}
```

Content clamps to `>=1px`; `grid_dims_from_pixels`' existing `.max(1)` floor (`geometry.rs:11-12`) then guarantees `>=1` cell (satisfies decision #4). Origin is pinned to `surface-1` so the lone clamped cell doesn't draw off-screen.

**To guarantee the renderer origin, the EdgeBleed pad, and the grid inset are provably identical**, derive the four physical pads from this same function (return them or expose a sibling `scaled_pads(pad, scale_factor) -> (u32,u32,u32,u32)` using the identical `.round()` closure) and feed both the renderer setter and `resync_grid` from it. Do NOT independently re-scale pads with a different rounding rule (Risk #6).

### `theme_bridge.rs` — config→render enum mapper (mirror `scroll_button_corner` at `:16`)

```rust
use toastty_config::ExtendBackground as CfgExtend;
use toastty_render::ExtendBackground as RExtend;
#[must_use]
pub fn extend_background(mode: CfgExtend) -> RExtend {
    match mode {
        CfgExtend::Never => RExtend::Never,
        CfgExtend::Always => RExtend::Always,
        CfgExtend::AltScreen => RExtend::AltScreen,
    }
}
```

### `main.rs` — `Toastty::push_padding_to_renderer`

Use **`.round()`** (matching `content_rect_from_padding` / `effective_font_size_px`), NOT a bare truncating cast — otherwise the renderer origin and the grid inset could disagree by 1px (Risk #6):

```rust
fn push_padding_to_renderer(&mut self) {
    let pad = &self.config.window.padding;
    // Same scale rule as content_rect_from_padding (.round()), so renderer origin,
    // EdgeBleed pad, and grid inset are byte-identical.
    let scale = |v: u16| (f64::from(v) * self.scale_factor).round() as u32; // physical px
    let (pt, pr, pb, pl) = (scale(pad.top), scale(pad.right), scale(pad.bottom), scale(pad.left));
    if let Some(r) = self.renderer.as_mut() {
        r.set_padding(pt, pr, pb, pl);                 // t,r,b,l physical px
        r.set_extend_background(extend_background(self.config.window.extend_background));
    }
}
```

(`set_padding`/`set_extend_background` already force `needs_full_clear` on change; no separate `invalidate_framebuffer` needed.)

### `resync_grid` (`main.rs:623-652`) — inset by padding

Replace the full-size grid math (`:631-632`) with content dims, and report **content** dims as the PTY winsize pixel fields:

```rust
    let pad = &self.config.window.padding;
    let (_ox, _oy, content_w, content_h) = content_rect_from_padding(
        self.physical_size, (pad.top, pad.right, pad.bottom, pad.left), self.scale_factor);
    let (cols, rows) = grid_dims_from_pixels(content_w, content_h, cell_w, cell_h);
    self.term.resize(rows, cols);
    // ... set_cell_pixel_size unchanged ...
    let pixel_width  = u16::try_from(content_w).unwrap_or(u16::MAX);   // was px_w (:641)
    let pixel_height = u16::try_from(content_h).unwrap_or(u16::MAX);   // was px_h (:642)
```

Keep the `u16::try_from(..).unwrap_or(u16::MAX)` clamp (content dims are `u32`).

> **Argument-order caution (verified):** `grid_dims_from_pixels` returns `(cols, rows)` (`geometry.rs:9`, bound as `let (cols, rows) =` at `main.rs:632`); `term.resize(rows, cols)` takes `(rows, cols)`; `term.size()` returns `(rows, cols)`; `pixel_to_cell`'s grid param destructures as `(rows, cols)` (`mouse.rs:87`). Preserve each site's order exactly.

### DECSET-2048 in-band resize report (resize handler, `main.rs:1850-1852`) — recompute content dims

**The resize-handler 2048 site does NOT reuse `resync_grid`'s content dims.** It computes `pixel_w = u16::try_from(width)...` / `pixel_h = u16::try_from(height)...` from the resize event's **full-surface** `width`/`height` locals (set at `~1806`), independently of `resync_grid`. Simply "substituting `px_w/px_h`" will NOT touch them. Recompute content dims here:

```rust
    let pad = &self.config.window.padding;
    let (_ox, _oy, content_w, content_h) = content_rect_from_padding(
        self.physical_size, (pad.top, pad.right, pad.bottom, pad.left), self.scale_factor);
    let pixel_w = u16::try_from(content_w).unwrap_or(u16::MAX);
    let pixel_h = u16::try_from(content_h).unwrap_or(u16::MAX);
    // ... encode_resize_report(rows, cols, pixel_h, pixel_w, ...) unchanged in arg order ...
```

(Note `encode_resize_report` takes `(rows, cols, pixel_h, pixel_w, …)` — preserve that order; `main.rs:1854-1858`.)

### Three call sites for `push_padding_to_renderer`

- **Startup `init_impl` (`~730`):** after `set_theme`/`set_scroll_button` (`~765-766`) and after `scale_factor` is captured (`~737`), call `push_padding_to_renderer()`, then call `resync_grid()` to compute the first-frame grid (replacing the inline grid at `~777-779` so there's one grid code path). Must run before the PTY spawn (`~798+`) which reads rows/cols.
- **Resize handler (`~1801-1862`):** `scale_factor` is set at `~1820` (before our call), font rebuilt at `~1822-1836`, `resync_grid()` at `~1844`. Add `push_padding_to_renderer()` right before `:1844` — unconditionally (the content clamp depends on surface size, not just scale), cheap.
- **Config reload `apply_config_to_runtime` (`~907`):** add `push_padding_to_renderer()` before `resync_grid()` (`~944`). It already calls `invalidate_framebuffer()` (`~934`). `width`/`height`/`vsync` are intentionally not live-applied (`~904-906`); padding/extend_background **are** safe to live-apply (render + grid only, no surface reconfigure).

`renderer.resize()` still takes the **full** surface size (`~1823`); only the grid + PTY winsize use content dims.

---

## Mouse (`crates/toastty/src/mouse.rs`)

`pixel_to_cell` (`:80`) maps raw physical-px winit coordinates (window top-left origin) to a 1-based `(col, row)`. The grid now starts at `origin = (pad_left, pad_top)`, so subtract origin first. The existing `.floor().max(0.0)+1` and `.min(cols/rows)` clamps already deliver the required "clamp padding clicks to the nearest edge cell" with **no new branches** — left/top padding → negative → collapses to cell 1; right/bottom padding → large quotient → clamps to last cell.

### Signature (resolved cross-area contract)

Add `origin: (f32, f32)` as the **2nd positional param** (physical px):

```rust
#[must_use]
pub fn pixel_to_cell(
    pixel: (f64, f64),
    origin: (f32, f32),
    cell_size: (f32, f32),
    grid: (u16, u16),
) -> (u16, u16) {
    let (cw, ch) = cell_size;
    if cw <= 0.0 || ch <= 0.0 { return (1, 1); }
    let local_x = pixel.0 - f64::from(origin.0);
    let local_y = pixel.1 - f64::from(origin.1);
    let col = ((local_x / f64::from(cw)).floor().max(0.0) as u32) + 1;
    let row = ((local_y / f64::from(ch)).floor().max(0.0) as u32) + 1;
    let (rows, cols) = grid;
    let col = col.min(u32::from(cols.max(1))) as u16;
    let row = row.min(u32::from(rows.max(1))) as u16;
    (col, row)
}
```

### Binary plumbing (two non-test call sites — verified exhaustive)

`origin` comes from the renderer (single source of truth), via the public `Renderer::content_origin() -> (f32, f32)`:

- `current_cell` (`main.rs:533-540`): read `(cell_size, origin)` from the renderer via `map_or`. **The no-renderer branch must supply a default origin `(0.0, 0.0)` alongside the existing `(1.0, 1.0)` cell-size default** (`main.rs:537`). Call `pixel_to_cell(self.mouse_pos, origin, cell_size, (rows, cols))` with `(rows, cols) = term.size()`.
- `hyperlink_under_cursor` (`main.rs:1442-1448`): same pattern.

`current_selection_pos` and the SGR `encode_mouse` path consume the already-mapped 1-based cell — verified no change needed. The signature change propagates to exactly these 2 non-test sites plus the 4 existing unit tests (`mouse.rs:251-273`).

---

## Tests & verification

### Coverage gate (shapes everything)

`make cover-gate` → `cargo llvm-cov --workspace --fail-under-lines 95 --ignore-filename-regex $(COVER_IGNORE)` where **`COVER_IGNORE := '/(crates/toastty/|toastty-pty/|toastty-io/|toastty-window/|toastty-render/src/(device|pipelines)/)'`** (verified `Makefile:29,38`).

- **`toastty-config` is fully in-gate** — exercise every `ExtendBackground` variant and `PaddingConfig` field.
- **`crates/toastty-render/src/text/instance.rs` is IN-gate** — confirmed: the regex excludes only render's `device`/`pipelines` dirs, which do NOT exist (`crates/toastty-render/src/` has `text/`, `image/`, `rgp/`, none excluded). Every grow-arm and the gate must be hit by unit tests.
- **`crates/toastty/src/geometry.rs` + `mouse.rs` are in the ignore-listed `crates/toastty/`** — coverage won't enforce them, but add the pure-function tests anyway (cheap; the modules already test).

### Config tests (`crates/toastty-config`)

In `window.rs` `mod tests` (mirror `confirm_close_*`): `padding_defaults_are_zero`, `padding_default_trait`, `window_defaults_include_padding_and_extend`, `padding_round_trips` (asymmetric), `partial_padding_subtable_fills_defaults`, `padding_unknown_key_rejected`, `extend_background_round_trips_each_variant`, `extend_background_serializes_to_kebab_case` (asserts `"alt-screen"`), `extend_background_each_variant_parses`, `extend_background_invalid_value_rejected`.

**Compile-forcing update:** `window_round_trip` (`:77-87`) uses an exhaustive struct literal and will fail to compile until the two new fields are added — update it (add `extend_background` + `padding`). `window_defaults`/`window_default_trait` need no literal change.

Fixture + integration: `full.toml` additions flow into `fully_populated_toml_parses` (`lib.rs:223-230`) and `tests/integration.rs::fully_populated_fixture_parses_via_load_from_path` automatically; add spot-checks importing `ExtendBackground`/`PaddingConfig`. `defaults_round_trip_through_toml` (`lib.rs:184-189`) covers the all-defaults `[padding]` round-trip for free.

### Instance edge-extension tests (`instance.rs` `mod tests`) — highest value, in-gate

Reuse helpers `feed`, `count_non_cursor`, `cursor_instance`, `Theme::default_dark()`. Assert on emitted `CellInstance.pos`/`.size` (grow does NOT add instances):

Full builder: `edge_bleed_disabled_is_noop` (regression guard), `left_edge_cell_grows_left`, `right_edge_cell_grows_right`, `top_edge_and_bottom_edge_grow_y`, `corner_cell_grows_both_axes` (proves no dedicated corner code), `interior_cell_not_grown`, `wide_primary_at_right_edge_grows`, `continuation_cell_emits_nothing_at_edge`, `fractional_scroll_top_row_uses_pixel_extra` (full builder: extra row `r==0` NOT grown; `r==1` content-top IS), `glyph_and_cursor_not_grown`.

Dirty builder: `extend_bg_dirty_grows_edge_quad`, **`dirty_overpaint_extends_into_padding`** (THE key regression: revert an edge cell to default bg, mark dirty, assert the overpaint quad still carries the grown `pos/size` — without the fix it'd be `[cw,ch]`), **`dirty_top_row_uses_content_space_zero`** (set `pixel_extra==1` via fractional offset with a SPARSE — non-`all` — damage set; assert the content-top row `r==0` still grows UP and `r==1` does NOT — pins the `top_row = 0` decision against `top_row = pixel_extra`), `extend_bg_dirty_all_falls_back_to_full` (smoke check `bleed` is threaded through the `damage.all` re-call at `:749`).

> **Test-harness decision (resolved):** the `build_instances` public convenience wrapper (`:476`) is called by ~50 existing tests with the current arg shape. **Keep the wrapper defaulting to `EdgeBleed::default()` (no signature change), and add the `EdgeBleed` param only to `build_instances_into` / `build_dirty_instances_into`.** Existing tests/benches compile unchanged; new bleed tests call the `*_into` variants directly with a populated `EdgeBleed`.

### Cursor pre-offset

The cursor offset lands at the **GlobalsUbo fill site** (`lib.rs:2154-2169`), not inside `cursor_pixel_rect`. So the offset test lives in `lib.rs` (ignore-listed for coverage) or as a snapshot; `cursor_pixel_rect` itself is unchanged. Document this so the test goes to the right place.

### Geometry tests (`geometry.rs`, mirror `simple_division`/`font_size_*`)

`content_dims_subtract_padding` (surface 800×600, pad {10,20,10,20} → content 760×580, origin (20,10)); `content_dims_clamp_to_one_cell_when_padding_huge` (no panic, grid ≥ (1,1)); `padding_scaled_by_scale_factor` (logical 8 @ scale 2.0 → 16, using `.round()`).

### Mouse tests (`mouse.rs`)

Update the 4 existing `pixel_to_cell` tests (`:251-273`) to pass `origin=(0.0,0.0)` (expectations unchanged). New: `pixel_to_cell_accounts_for_origin`, `click_in_left_top_padding_clamps_to_first_cell` (incl. exactly-on-origin → (1,1)), `click_in_right_bottom_padding_clamps_to_last_cell`, `zero_origin_matches_legacy_behavior`.

### Snapshots, benches, RGP matrix

- Golden trio (`text_snapshot_{hello,colors,cursor}.rs`) renders at zero padding/`Never`. The harness `tests/common/mod.rs` builds its own `GlobalsUbo` literal (`~186-195`) and calls `build_instances` (`~127`) + `cursor_pixel_rect` (`~193`) — **update the `GlobalsUbo` literal** to include `content_origin: [0.0;4]`, else it fails to compile. With padding=0 the goldens render byte-identically — they become a "padding=0 is a true no-op" guard.
- `benches/render_term.rs` builds `GlobalsUbo` (`~202, ~345`) — add `content_origin: [0.0;4]`. (Its `build_instances_into` calls at `~181, ~320` gain the `EdgeBleed` param — pass `EdgeBleed::default()`.)
- **Optional** `text_snapshot_padding.rs`: bg grid + non-zero padding + `extend_background=Always`, asserting bleed reaches the physical edge. Nice-to-have — the instance unit tests prove the math deterministically without a GPU.
- **RGP y-axis matrix/anchor test** (`rgp/` `mod tests`): lock the world-Y-up vs screen-down padding sign. Assert a placement at `row0/col0` with `pad_top`/`pad_left` lands at the expected NDC, comparing against the text-cell physical position (i.e. `center_px_x += pad_left`, `center_px_y -= pad_top`). Since the fold is in the per-placement anchor (not `ortho_screen`), the test can drive `render()`/the anchor computation directly or assert on the resulting MVP-projected center.

### Performance note (lock the no-extra-clear invariant)

`extend_edge_quad` is `#[inline]`, takes `Copy` args, returns a tuple — zero alloc; called once per emitted bg quad (instance count unchanged). Edge detection is O(1) per cell (four scalar comparisons), O(perimeter) extra amortized into the existing loop. GlobalsUbo grows 48→64 bytes, uploaded via the existing `bytes_of`/`write_buffer`. Setters force `needs_full_clear` only on actual change (`!= old`). Add a bench/assertion note that ordinary partial-redraw frames (`needs_full_clear==false`, `!damage.all`) still take `LoadOp::Load` and emit **no** extra clear with bleed enabled, locking the no-full-clear-every-frame invariant against regressions.

### Pre-existing failures — DO NOT chase

Per `preexisting-test-failures.md`: `toastty-config security::tests::defaults_are_off`, and the golden snapshot trio (`snapshot_hello_text`, `snapshot_colors_and_reverse`, `snapshot_cursor_at_non_trivial_position`) fail on clean `main` (GPU/headless SSIM). Do NOT use snapshot pass/fail as the acceptance signal — rely on the deterministic instance unit tests. When reading `make cover-gate`, separate "test failed" (pre-existing) from "coverage < 95%" (yours).

### Verify commands (in order)

```
cargo build -p toastty-config && cargo test -p toastty-config
cargo build -p toastty-render && cargo test -p toastty-render --lib   # instance.rs, no GPU
cargo build -p toastty        && cargo test -p toastty --lib          # geometry + mouse
make check        # cargo check --workspace --all-targets (catches benches + snapshot harness)
make test
make lint         # clippy -D warnings
make fmt          # repo expects clean fmt (commit fad13b1)
grep COVER_IGNORE Makefile   # confirm the regex still excludes only device|pipelines before relying on the gate
make cover-gate   # config + instance.rs must stay >= 95%
```

---

## Implementation ORDER (staged; dependency-correct)

**Stage 1 — Config types (leaf, no deps).** Land first so the binary can name the types.
Files: `crates/toastty-config/src/window.rs` (add `ExtendBackground` + `PaddingConfig`; add `extend_background` + `padding` LAST to `WindowConfig` + `defaults()`; update `window_round_trip`; add unit tests), `crates/toastty-config/src/lib.rs` (re-export at `:47`), `crates/toastty-config/tests/fixtures/full.toml` (add keys + `[window.padding]`), `crates/toastty-config/tests/integration.rs` (import + spot-checks).
Verify: `cargo test -p toastty-config`.

**Stage 2 — Render uniform contract (producers before consumers).** Bleed is a no-op here (`EdgeBleed::default()`); **do not exercise/assert bleed until Stage 3 lands** (edge detection keys off `term.size()`, which is full-surface until Stage 3's content-aware resize).
2a. **GPU UBO + shader layout** (the contract): `crates/toastty-render/src/text/pipeline.rs` (`GlobalsUbo` += `content_origin: [f32;4]`), `crates/toastty-render/shaders/text.wgsl` (struct field + vertex `+content_origin.x/.y`), `crates/toastty-render/src/image/pipeline.rs` (rename `tex_dims`→`content_origin`, add `content_origin` param to `render()`, change the `:279` write `tex_dims:[0,0]`→`content_origin`), `crates/toastty-render/shaders/image.wgsl` (rename field + update the `:20-22` doc comment + vertex `+content_origin`), `crates/toastty-render/src/rgp/pipeline.rs` (thread `content_origin` into `render()`; fold into `center_px_x += pad_left`, `center_px_y -= pad_top`; `ortho_screen`/`rgp.wgsl`/`matrix.rs` UNCHANGED).
2b. **Edge-bleed (`EdgeBleed` type + grow + dirty overpaint):** `crates/toastty-render/src/text/instance.rs` (`EdgeBleed`, `extend_edge_quad`, grow at `:607` with `top_row = pixel_extra` + at `:836` with `top_row = 0`, wide-cell fix, forward through `damage.all`; `build_instances` wrapper unchanged; add unit tests).
2c. **Renderer plumbing:** `crates/toastty-render/src/lib.rs` (render-side `ExtendBackground` + `active()` + re-export; `Renderer` fields + `new()` init; `content_origin()`/`content_dims()`; public `set_padding`/`set_extend_background` + public `content_origin()` getter; GlobalsUbo fill + cursor pre-offset at `:2154-2169`; origin into image/rgp render at `:2270/2295/2309/2325`; overlay/scroll-button call sites → content dims; **scrim full-surface vs box content dims** in `draw_close_dialog`; `scroll_button_rect` origin bake; `EdgeBleed` into the two forwarders + `render_term` call sites). Bench/snapshot harness compile fixes: `benches/render_term.rs`, `tests/common/mod.rs` (`GlobalsUbo` literal += `content_origin`; `EdgeBleed::default()`).
Verify: `cargo test -p toastty-render --lib` + `make check`.

**Stage 3 — Binary bridge (depends on Stages 1 & 2; this is what makes `term.size()` content-aware so bleed becomes correct).**
Files: `crates/toastty/src/geometry.rs` (`content_rect_from_padding` with `.round()` + tests), `crates/toastty/src/theme_bridge.rs` (`extend_background` mapper), `crates/toastty/src/mouse.rs` (`pixel_to_cell` origin param + tests), `crates/toastty/src/main.rs` (`push_padding_to_renderer` with `.round()`; call in `init_impl`/resize/`apply_config_to_runtime`; `resync_grid` content inset + content PTY winsize; **DECSET-2048 site `:1850-1852` content-dims recompute**; thread `content_origin()` into `current_cell` (+ default origin in the no-renderer `map_or`) + `hyperlink_under_cursor`).
Verify: `cargo test -p toastty --lib`.

**Stage 4 — Whole-workspace verification.**
`grep COVER_IGNORE Makefile && make check && make test && make lint && make fmt && make cover-gate`. Optionally add `text_snapshot_padding.rs` golden (`TOASTTY_UPDATE_SNAPSHOTS=1`). Confirm pre-existing failures are unchanged.

---

## Risks & open questions

### Risks

1. **Config field order:** `padding: PaddingConfig` MUST be the LAST field of `WindowConfig`. `toml 0.8` emits a nested struct as a `[padding]` sub-table after all scalar keys; declaring it earlier breaks `defaults_round_trip_through_toml` (`lib.rs:184`). Verified by probe.
2. **`window_round_trip` (`window.rs:77`)** uses an exhaustive struct literal — will fail to compile until the two fields are added (this is good; forces the update).
3. **REPLACE blend (One/Zero):** grown bg quads must never overlap a neighbor's cell rect — they only extend into the padding gutter / corner padding (occupied by nobody). The overpaint wipe is exact because default-bg resolves to `theme.bg` (`instance.rs:271`) = the clear color (`lib.rs:2181`) and `fs_bg` outputs premultiplied under REPLACE (`text.wgsl:140-142`).
4. **Wide-cell right edge:** a width-2 primary occupies `cols-2` (continuation at `cols-1`); the naive `c==cols-1` predicate misses it — the `is_wide` OR is required.
5. **Fractional-scroll slivers — BOTH edges:** during active scroll (`pixel_extra==1`) a `view_pixel`-tall stale sliver can show in the TOP gutter and a `(cell_h - view_pixel)`-tall base-bg band in the BOTTOM gutter (we grow `r==rows-1`, not the visually-lowest `r==rows`). Cosmetic/transient; steady state (`pixel_extra==0`) exact at every edge. Accepted v1.
6. **Padding↔origin must match exactly:** `EdgeBleed.pad`, the renderer's `content_origin`, and the grid inset must use the SAME `.round()` scaling. Derive all from one site (`content_rect_from_padding` / a shared `scaled_pads` helper); do not re-scale with a truncating cast.
7. **`config.width/height` stay full-surface:** `renderer.resize()` + the bg clear + the viewport tuple + the px→NDC divisor all use the full physical surface; only the grid + PTY winsize use content dims. Pushing content dims into `resize()` breaks all three — forbidden (see Surface-size contract).
8. **`u16` PTY winsize clamp:** `content_w/content_h` are `u32`; keep `u16::try_from(..).unwrap_or(u16::MAX)` at both `resync_grid` and the DECSET-2048 site (which needs its OWN content recompute, not the full-surface `width/height` locals).
9. **Scale-factor timing:** `push_padding_to_renderer` must run AFTER `scale_factor` is updated (init `~737`, resize `~1820` — both before the proposed call sites). Moving it earlier would scale by the stale 1.0 default.
10. **Re-pushing padding on every resize** (not just scale changes) is intentional — the content clamp depends on surface size. It writes one uniform + sets `needs_full_clear` (only on change); cheap.
11. **RGP y-axis sign** (world-Y-up vs screen-down padding): `center_px_x += pad_left`, `center_px_y -= pad_top`. Easy to invert — gate with the matrix/anchor unit test (Stage 2a).
12. **Two row spaces in the builders:** full builder is RENDER space (`top_row = pixel_extra`); dirty builder is CONTENT space via `iter_rows()` (`top_row = 0`). Passing `pixel_extra` to the dirty path mis-places the top bleed at `pixel_extra==1`.
13. **Two intentional orderings:** pads are `[top,right,bottom,left]`; origin is `(x=left, y=top)`. Distinct on purpose; do not "fix" into agreement.
14. **`draw_close_dialog` scrim:** must cover the FULL surface (emit at `pos=[-pad_left,-pad_top]`, full-surface size), while the box layout uses content dims. Routing the scrim through content dims leaves the gutter undimmed (looks broken with `Always`).
15. **Argument-order tripwire:** `grid_dims_from_pixels` → `(cols, rows)`; `term.size()` / `term.resize` / `pixel_to_cell` grid → `(rows, cols)`; `encode_resize_report(rows, cols, pixel_h, pixel_w, …)`. Preserve each site's order.

### Open questions

1. **CSI 14 t / PTY pixel reporting behavior change:** this plan reports **content** dims (not full surface) as PTY `pixel_width/pixel_height` and the DECSET-2048 in-band report — a behavior change from today's full-surface values. Confirm no app relies on full-surface pixel reporting. (Recommended: content dims.)
2. **`init_impl` first-frame grid:** this plan replaces the inline grid math (`~777-779`) with a `resync_grid()` call after `push_padding_to_renderer`. PTY at `~798+` reads rows/cols, so `resync_grid` must run first — it does under this ordering. Confirm nothing else in `init_impl` needs the inline grid earlier.
3. **Symmetric-padding shorthand** (`padding = 8` expanding to all sides) is NOT in the locked spec; not added.
4. **Optional `text_snapshot_padding.rs` golden:** the snapshot trio already fails on this headless machine; the deterministic instance unit tests fully cover the bleed math without a GPU. Confirm with the orchestrator whether a visual golden is required for acceptance.

---

## Review resolutions

- **[E2E correctness, major] `content_dims()` reads `config.width/height` without stating they must stay full-surface** — Resolved: added an explicit **"Surface-size contract"** subsection in Renderer plumbing pinning that `config.width/height` are the wgpu `SurfaceConfiguration` (full physical surface; blit `Extent3d` `lib.rs:2353-2354`, viewport `:2263`, px→NDC divisor `:2161-2163`), fed by `resize()` with full size, and that content dims are derived only inside `content_dims()`. Forbade pushing content dims into `resize()`. Also added Risk #7.
- **[E2E correctness, major] "term.size() reflects content grid — no extra plumbing" is a cross-stage hazard** — Resolved: §"Edge detection, content dims, and the two row spaces" now states bleed correctness depends on Stage 3's content-aware `term.resize`; Stage 2 is a pure no-op via `EdgeBleed::default()` (enabled=false), and bleed must not be exercised until Stage 3. Stage 2 header repeats this; instance tests build their own `Term` so are unaffected.
- **[E2E correctness, minor] Two pad orderings (t/r/b/l vs origin x=left/y=top)** — Resolved: added an explicit "Ordering note" by the `EdgeBleed` def and Risk #13 pinning both orderings as intentionally distinct.
- **[E2E correctness, minor] u32 pads vs f32 origin; confirm `pixel_to_cell` propagation + no-renderer default** — Resolved: Mouse §"Binary plumbing" now calls out the no-renderer `map_or` must supply default origin `(0.0,0.0)` alongside the `(1.0,1.0)` cell-size default; reconfirmed exactly 2 non-test call sites + 4 unit tests.
- **[E2E correctness, minor] image `render()` per-frame write `tex_dims:[0,0]` + stale WGSL doc comment** — Resolved: Image pipeline section + Stage 2a now explicitly call out changing `image/pipeline.rs:279` to `content_origin` and updating the `image.wgsl:20-22` doc comment.
- **[E2E correctness, nit] Verify cover-gate regex literally** — Resolved: grepped `Makefile` (`COVER_IGNORE := '/(crates/toastty/|toastty-pty/|toastty-io/|toastty-window/|toastty-render/src/(device|pipelines)/)'`, `:29`), confirmed `text/instance.rs` is in-gate; cited verbatim in Tests §coverage and added a `grep COVER_IGNORE Makefile` to verify commands + Stage 4.
- **[E2E correctness, nit] scratch_stale interaction omitted** — Resolved: §"needs_full_clear triggers + scratch staleness" now cites the `render_direct`/`scratch_stale` net (`lib.rs:2204, 2393, 1747-1749`) as the mechanism that closes the padding-change staleness window.
- **[E2E correctness, nit] re-export anchor exactness** — Resolved: §Re-export now quotes the current `lib.rs:47` line and the replacement.
- **[Damage correctness, major] Bottom-edge fractional-scroll sliver only top was flagged** — Resolved: §"Fractional-scroll top AND bottom slivers" now documents BOTH gutters (with exact band heights and the `r==rows-1` vs `r==rows` analysis), kept the simple predicates deliberately, and updated Risk #5 to both edges.
- **[Damage correctness, minor] "dirty path only runs at pixel_extra==0" rationale wrong** — Resolved: dropped that rationale; §"Dirty-path / fractional-scroll invariant" now states the real invariant (offset *changes* force full clear `lib.rs:1547`; held offsets can run the dirty path at `pixel_extra==1`, where math is correct via content-space `top_row=0` + per-row damage gating).
- **[Damage correctness, minor] Dirty builder loop misdescribed as `0..rows`** — Resolved: corrected to `damage.iter_rows()` (damaged rows only, content-row space `0..rows`, `damage.rs:162-173`); this drove the `top_row=0` fix below.
- **[Damage correctness, nit] Overpaint wipe load-bearing facts ungrounded** — Resolved: §"CRITICAL overpaint case" now cites default-bg == theme.bg == clear color (`instance.rs:271` / `lib.rs:2181`) and `fs_bg` premultiplied REPLACE output (`text.wgsl:140-142`).
- **[Damage correctness, nit] scratch_stale safety net not called out** — Resolved: same addition as the E2E scratch_stale nit; cited `lib.rs:2204/2393/1747`.
- **[GPU soundness, major] RGP origin-fold composition order ambiguous; translate option wrong** — Resolved: §RGP pipeline now picks ONE implementation — fold into the per-placement anchor (`center_px_x += pad_left`, `center_px_y -= pad_top`), leaving `ortho_screen`/`matrix.rs`/`rgp.wgsl` untouched. Wrote the exact terms + Y-sign derivation; rejected the MVP-translate and `ortho_screen_with_origin` options. Mandated the anchor/matrix unit test.
- **[GPU soundness, minor] Bottom-edge predicate during fractional scroll** — Resolved: same as the damage-correctness bottom-sliver finding; explicitly chose option (a) (keep `r==rows-1`, document the artifact at both edges) and noted the unit test pins full-builder behavior at `pixel_extra==1`.
- **[GPU soundness, minor] `draw_close_dialog` scrim covering only content area** — Resolved: §Overlays now splits scrim (full surface, emitted at `pos=[-pad_left,-pad_top]`) from the box layout (content dims); added Risk #14.
- **[GPU soundness, nit] Wrong shader paths (`shaders/*.wgsl`)** — Resolved: corrected all shader paths to `crates/toastty-render/shaders/{text,image,rgp}.wgsl` throughout (Summary, Rendering design, Stage 2a); noted the stale worktree copy must not be edited. Verified via `include_str!` grep.
- **[Config/DPI/perf, major] Dirty top-edge index must be content-space `0`, not `pixel_extra`** — Resolved: §"two row spaces" + Wiring-into-dirty now pass `top_row = 0` to the dirty path (full builder keeps `pixel_extra`); added the `dirty_top_row_uses_content_space_zero` test (sparse damage at `pixel_extra==1`) and Risk #12.
- **[Config/DPI/perf, minor] `push_padding_to_renderer` truncating cast vs `.round()`** — Resolved: switched to `.round() as u32` (matching `effective_font_size_px`/`content_rect_from_padding`); recommend deriving all four pads from one shared scaling site; updated Risk #6.
- **[Config/DPI/perf, minor] DECSET-2048 site uses full-surface `width/height` locals** — Resolved: §"DECSET-2048 in-band resize report" now recomputes content dims via `content_rect_from_padding` at `main.rs:1850-1852` (verified the locals are `pixel_w/pixel_h` from the event's full-surface `width/height`, independent of `resync_grid`); preserved `encode_resize_report(rows, cols, pixel_h, pixel_w, …)` arg order.
- **[Config/DPI/perf, minor] "do not call set_viewport" implies a call exists** — Resolved: reworded to "there is no `set_viewport` today (verified empty grep); keep the px→NDC divisor full-surface" in §"Threading origin into image/rgp render".
- **[Config/DPI/perf, nit] Hot-path perf clean; lock no-extra-clear invariant** — Resolved: added a "Performance note" subsection in Tests recording the O(1)/no-alloc analysis and recommending an assertion/bench note that partial frames take `LoadOp::Load` with no extra clear.
- **[Config/DPI/perf, nit] Fixture placement** — Resolved: §Fixture now says to insert `[window.padding]` after the `[window]` scalars and before `[scroll_button]`, noting placement is parse-tolerant and only struct field order is load-bearing.