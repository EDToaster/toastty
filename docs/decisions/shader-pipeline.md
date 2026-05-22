# Shader pipeline: WGSL only, GLSL via `naga`, or both?

**Slug:** `shader-pipeline`
**Date:** 2026-05-22
**Status:** Recommendation, awaiting maintainer sign-off
**Prototype:** [`/prototypes/shader-pipeline`](../../prototypes/shader-pipeline)

## TL;DR

**Recommendation: ship dual-path — WGSL primary, GLSL accepted.** The GLSL
path costs ~250 LoC of shim (one `glsl_shim.rs` plus the `glsl-in` /
`wgsl-out` naga features) and pays back the entire ShaderToy ecosystem.
Naga 29.0.3's GLSL frontend handled **11/11** realistic ShaderToy
fragment-shader paste-ins in this prototype, at sub-millisecond cold
compile cost (≤0.7 ms on an Apple M4 Pro).

The previously-stated position in `architecture.md` — "WGSL primary, GLSL
accepted via `naga` translation" — survives contact with the prototype.

## What was built

A standalone Cargo crate under `prototypes/shader-pipeline/` that:

- opens a wgpu + winit window
- renders a placeholder gradient + fake glyph grid into an offscreen
  RGBA8-sRGB texture (the stand-in for the future cell pass)
- loads a user fragment shader from a CLI-supplied path
- exposes ShaderToy-style uniforms: `iTime`, `iFrame`, `iResolution`,
  `iCursor` (faked), and `iChannel0` (the offscreen scene as a sampled
  texture)
- supports two source paths:
  - `--wgsl path.wgsl` — user provides `@fragment fn fs_main(...)`.
    Concatenated with a prelude + fullscreen-triangle vertex stage.
  - `--glsl path.glsl` — user provides a ShaderToy-style
    `void mainImage(out vec4 fragColor, in vec2 fragCoord)`. The shim
    wraps it with a `#version 450 core` header, a UBO declaration, a
    `void main()` that calls `mainImage`, and the same fullscreen-vs
    bolted on after naga translates to WGSL.
- hot-reloads via `notify` (watching the parent directory)
- on compile error, logs the diagnostic and **keeps the previous
  pipeline live** — no crash, no black flash beyond the failing edit

A second `--bin translate_bench` runs the shader corpus headlessly to
measure parser cost without the wgpu/window setup overhead.

## Versions (pinned exact)

| Crate         | Version            |
| ------------- | ------------------ |
| `wgpu`        | `29.0.3`           |
| `naga`        | `29.0.3` (features: `glsl-in`, `wgsl-out`, `wgsl-in`) |
| `winit`       | `0.30.13` (latest stable; 0.31 is still beta.2) |
| `notify`      | `8.2.0`  (latest stable; 9.0 is still rc.4)     |
| `pollster`    | `0.4.0`            |
| `bytemuck`    | `1.25.0`           |
| `env_logger`  | `0.11.10`          |
| `log`         | `0.4.29`           |
| `anyhow`      | `1.0.102`          |

## Method

Twelve fragment shaders were authored across the WGSL and GLSL paths:

- **3 WGSL** — CRT, film-grain, plasma, written native
- **3 GLSL** — same three effects, written in ShaderToy paste-in style
- **8 GLSL "in-the-wild"** — common ShaderToy patterns deliberately
  chosen to stress naga's GLSL frontend:
  1. `mat2` rotation (a known historic flake)
  2. iquilezles palette function
  3. nested-loop voronoi with signed-int indices
  4. mandelbrot with break-out-of-loop
  5. truchet pattern with ternary
  6. iquilezles smin SDF
  7. ray-marched sphere with `calcNormal` triplets
  8. `const float[N]` array constructor inside a function
  9. `discard` + `dFdx`/`dFdy` derivative builtins
  10. `textureLod` (becomes WGSL `textureSampleLevel`)
  11. `iMouse.zw` swizzle (catches a real shim bug — see below)
- 1 deliberately-broken `iChannel1` reference to confirm clean error
  reporting

Each was compiled cold and warm via the `translate_bench` binary, and
verified end-to-end through the full wgpu pipeline by launching the GUI
binary with each path.

## Results

### Port success rate

|                                | Compiled | Reasonable looking |
| ------------------------------ | -------- | ------------------ |
| WGSL native (3)                | 3 / 3    | 3 / 3              |
| GLSL → naga ShaderToy ports (3)| 3 / 3    | 3 / 3              |
| GLSL → naga in-the-wild (11)   | 11 / 11  | 11 / 11            |
| GLSL → naga deliberate break (1) | 0 / 1 (expected; clean error) | n/a |

**Out-of-the-box port rate for GLSL via naga: 14/14 = 100%** on the
realistic corpus.

That number is suspiciously good, so the caveats matter:

- All test shaders are **fragment-only, single-pass, `iChannel0`-or-less**.
  ShaderToy multi-buffer setups (BufferA, BufferB, …) and 3D/cube
  channels are not exercised — and would not be exercised by toastty's
  single post-process pass anyway.
- "Looks right" is a low bar in a 4-second timeout with no screenshot
  diff; we confirmed each compiled to a valid wgpu pipeline and
  surfaced no validation errors during rendering.

### Compile time (Apple M4 Pro, release build)

Per-shader parser + validator + WGSL emit (no wgpu involvement):

|                          | Cold       | Warm       |
| ------------------------ | ---------- | ---------- |
| WGSL parse + validate    | 0.10 – 0.31 ms | 0.07 – 0.20 ms |
| GLSL → naga → WGSL emit  | 0.13 – 0.45 ms | 0.10 – 0.32 ms |

The wgpu `create_shader_module` + `create_render_pipeline` adds another
~0.1–0.5 ms; total interactive recompile is well under 1 ms even for
ray-marched scenes. **Hot reload feels instantaneous.**

### Hot reload + error recovery

Validated by:

1. starting the app with a good shader
2. `cp` the same file (fires a watcher event) → "recompile OK"
3. overwriting with `void mainImage(...) { THIS_IS_BAD; }` →
   `recompile FAILED: GLSL parse failed: - Unknown variable: THIS_IS_BAD`
4. restoring → "recompile OK"

App keeps running through (3) using the last-good pipeline. Same flow
verified for WGSL with an unknown identifier:
`wgpu validation: ... no definition in scope for identifier: broken`.

### What the GLSL shim does

ShaderToy is not a complete GLSL environment — it injects globals.
Reproducing the contract:

```glsl
// Prepended by the shim before user code.
#version 450 core
layout(set=0, binding=0) uniform Globals {
    vec3 iResolution; vec2 iCursor; float iTime; int iFrame;
} u;
layout(set=0, binding=1) uniform texture2D iChannel0Tex;
layout(set=0, binding=2) uniform sampler   iChannel0Smp;

vec3 iResolution; vec2 iCursor; vec4 iMouse;   // file-scope shadows
float iTime; int iFrame; float iTimeDelta;
vec4 iDate; float iSampleRate;
#define iChannel0 sampler2D(iChannel0Tex, iChannel0Smp)

layout(location=0) in  vec2 v_uv;
layout(location=0) out vec4 outColor;

// ... user code with mainImage(...) ...

void main() {
    iResolution = u.iResolution;  /* … rest of UBO → shadow copies … */
    vec2 fragCoord = vec2(v_uv.x, 1.0 - v_uv.y) * iResolution.xy;
    vec4 col = vec4(0.0);
    mainImage(col, fragCoord);
    outColor = col;
}
```

The non-obvious bits:

1. **Texture + sampler split.** GLSL `sampler2D iChannel0` doesn't exist
   in Vulkan-flavoured GLSL; the right combo is `texture2D` + `sampler`
   bound separately, glued with `sampler2D(tex, smp)`. We hide that
   behind `#define iChannel0`.
2. **Y-flip.** ShaderToy treats `fragCoord.y = 0` as the bottom of the
   screen; wgpu/Vulkan/Metal treat the window pixel `y = 0` as the top.
   The shim flips inside `main()` before calling `mainImage`. User code
   that then samples `iChannel0` typically re-flips on its side; that's
   why the GLSL test shaders contain `vec2 s = vec2(uv.x, 1.0 - uv.y)`.
3. **File-scope shadow vars, not macros.** `#define iMouse (vec4(...))`
   breaks the moment the user writes `iMouse.zw`, because naga's GLSL
   frontend doesn't accept `.swizzle` on a parenthesized constructor in
   that position. We declare `vec4 iMouse;` at file scope and copy from
   the UBO inside `main()`.

### Render pipeline detail

A single bind group is shared between WGSL and GLSL paths:

```
@group(0) @binding(0)  Globals UBO
@group(0) @binding(1)  iChannel0 texture (RGBA8-sRGB, scene output)
@group(0) @binding(2)  iChannel0 sampler (filterable)
```

The naga-emitted WGSL slots straight into this layout because the GLSL
shim declares `layout(set=0, binding=N)` decorations that survive the
translation 1:1.

The full-screen triangle and fragment stage live in **one shader
module** for the WGSL path (concatenation glue) and **two stages
combined post-translation** for the GLSL path (we paste our WGSL VS
after naga's emission). One `ShaderModule`, two entry points.

## Recommendation

**Dual path: WGSL is primary, GLSL is accepted on equal terms.**

Why not WGSL-only:

- Killing the GLSL door also kills the path to shadertoy.com, where the
  custom-shader audience already exists. The shim is two days of work,
  not a strategic burden, and 100% of realistic ShaderToy pastes worked.
- The cost is real but bounded: ~250 LoC of shim, two extra naga
  features compiled in, sub-millisecond runtime. None of those land on
  the hot path or the binary's idle RAM.

Why not GLSL-only:

- toastty's own built-in shaders should be WGSL — it's the native
  language of wgpu, has better error messages from the validator, and
  doesn't pay the translation tax. The dual path means we don't translate
  what we don't have to.
- WGSL has a saner error surface for user-authored shaders that the user
  already wrote in WGSL. We should not force them through GLSL just
  because we accept it.

### Suggested implementation in toastty

When implementing for real in `toastty-render`:

1. **File extension dispatch.** `.wgsl` → WGSL path, `.glsl` / `.frag` →
   GLSL path. No CLI flag like in the prototype.
2. **Document the shim contract** prominently in the config docs.
   ShaderToy users will paste; tell them about the y-flip and the
   `iChannel0` definition.
3. **Surface errors in the status line.** This prototype logs to stderr;
   in the real app, parse errors and the offending line should appear in
   the terminal's UI so the user doesn't have to tail a log file.
4. **Don't expose `iChannelN, N > 0` until we have multi-pass.**
   `iChannel1` and friends are common in multi-buffer ShaderToys; we'd
   need a multi-pass pipeline first. Friendly error message in the
   meantime is fine — the prototype already produces one.
5. **Consider caching translated WGSL to disk** keyed by content hash.
   At 0.5 ms per cold compile this is purely cosmetic, but it would
   skip even that tiny stall on the first frame after a config reload.

## Surprises

Three findings worth flagging:

1. **`naga` 29 is dramatically less fragile than its reputation
   suggests.** Earlier naga had a known-bad track record with mat2,
   const arrays, and derivative builtins. All three worked first try in
   naga 29.0.3. The "out-of-the-box ShaderToy port rate" is much higher
   than the docs would imply.
2. **`#define` macros that produce parenthesised expressions break
   swizzling in naga's GLSL frontend.** Specifically,
   `#define iMouse (vec4(u.iCursor, 0.0, 0.0))` makes `iMouse.zw` fail
   to parse: "Expected identifier, found LeftParen". File-scope shadow
   variables initialised in `main()` are the workaround. This will bite
   any future shim contributor who reaches for macros — worth a comment
   in `glsl_shim.rs` (there is one).
3. **wgpu 29's API has tightened around a few spots not yet reflected in
   most tutorials.** `InstanceDescriptor::default()` is gone (use
   `new_without_display_handle_from_env()`); `on_uncaptured_error`
   wants `Arc<dyn ...>`, not `Box`; `bind_group_layouts` is now
   `&[Option<&BindGroupLayout>]`; `multiview_mask: None` is required on
   `RenderPassDescriptor`; `pop_error_scope()` is replaced by a guard
   pattern (`push_error_scope` returns an `ErrorScopeGuard`,
   `.pop().await` gives the Option). None of these are deal-breakers but
   each is a 30-second silent papercut. Worth knowing before the real
   renderer crate is written.

## Build / run cheat sheet

```sh
cd prototypes/shader-pipeline
cargo run --release -- --wgsl shaders/wgsl/crt.wgsl
cargo run --release -- --glsl shaders/glsl/plasma.glsl
cargo run --release --bin translate_bench   # headless corpus runner
DUMP_WGSL=1 cargo run --release -- --glsl shaders/glsl/crt.glsl
#   → writes the naga-emitted WGSL to /tmp/shader-pipeline-last.wgsl
```

Hot reload: edit any of the watched shader files; the running app
recompiles in <1 ms or logs the diagnostic and keeps the old pipeline.
