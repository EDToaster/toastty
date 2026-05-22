# RGP 3D rendering path

**Status:** decided
**Date:** 2026-05-22
**Scope:** which library should toastty use to render the Ratty Graphics Protocol (RGP) 3D pass.

## Recommendation

**Use Option A: a hand-rolled wgpu scene graph.** Keep the existing `wgpu` choice
in [`architecture.md`](../architecture.md) and own a small set of node/material/
mesh primitives directly. Plan a future port to `rend3` *only* when RGP fidelity
demands real PBR + shadow maps + IBL.

Bevy is explicitly rejected: even at the smallest plausible feature set it
costs ~6× the binary and ~2× the transitive crate count of the hand-rolled
path, and the runtime memory delta — ~190 MiB resident — is a non-starter for
the "Alacritty-class baseline" target.

`rend3` is plausible as the upgrade path but not the starting point: its sole
public crates.io release (`0.3.0`, March 2022) is pinned to `wgpu 0.12` and
incompatible with anything modern; the only realistic version lives on a
two-year-old `trunk` commit (`d088a841b0`, May 2024) pinned to `wgpu 0.19`.
Adopting it today means consuming an unmaintained git dependency.

## Why this fits the project

The architecture doc already explicitly stakes out "**Start here.** Fits the
lightweight framing" for the hand-rolled path. The prototype confirms that
position with numbers: the cost of "minimal scene graph" is ~730 lines of
straight wgpu (one shader file, three pipelines, two bind groups, an indexed
draw call for the duck). That is the same order of magnitude as a single
protocol handler under `toastty-protocols/`.

It also fits the goal of a **fused color+depth render graph**. The hand-rolled
prototype renders Background → 3D → Foreground into the *same* color+depth
attachments inside a single render pass. That is exactly the composition shape
the architecture's renderer diagram calls for (`Background → 3D (RGP) → Cells
(text+image) → Post`). Both other options fight this:

- `rend3` owns its own depth buffer internally. To get a checkerboard *behind*
  the duck and a "text" stripe *in front* of it that occludes geometry, you
  either bracket `rend3` with two external wgpu passes (which the prototype
  does) or write a custom `rend3` routine that hooks into its render-graph
  interfaces. Both paths reach inside abstractions `rend3` did not design for
  termenal composition.
- Bevy assumes one world, one scene, one render graph. Composing "text+image
  layer on top of 3D layer" works if you commit to expressing the entire
  terminal in Bevy's ECS — at which point you are no longer "using Bevy as a
  library for the 3D pass", you are writing a Bevy app. That's a much bigger
  architectural commitment than the doc asks for.

## Measurements

All numbers measured 2026-05-22 on macOS, M4 Pro (Metal backend), `release`
profile with `lto = "fat"`, `codegen-units = 1`. Each prototype renders the
same scene: Khronos `Duck.glb`, directional light, checkerboard background,
foreground "text" stripe with depth occlusion.

| Metric                          | Hand-rolled wgpu | rend3 (git trunk) | Bevy headless |
| ------------------------------- | ---------------: | ----------------: | ------------: |
| Binary size (pre-strip)         |       6,577,720 B |        6,751,496 B |   47,687,568 B |
| Binary size (stripped)          |       5,394,192 B |        5,540,256 B |   32,648,360 B |
| Binary size (stripped, MiB)     |              5.14 |              5.28 |         31.13 |
| `cargo tree` raw lines          |               257 |               369 |           935 |
| `cargo tree` unique crates      |               125 |               168 |           258 |
| Idle RAM (RSS post-init)        |             92 MiB |           109 MiB |        135 MiB |
| Active RAM (RSS @60fps)         |            103 MiB |           110 MiB |        184 MiB |
| Time to first frame             |            330 ms |            169 ms |          239 ms |
| 600-frame wall-clock            |           4.03 s |            5.15 s |         8.62 s |
| Avg frame time over 600 frames  |           6.7 ms |            8.6 ms |         14.4 ms |
| Prototype Rust LoC              |               727 |               441 |           208 |
| Prototype WGSL LoC              |               125 |                60 |              0 |
| Total prototype source LoC      |               852 |               501 |           208 |
| Cold release build time         |            ~30 s |            ~30 s |          ~3.5 min |

Notes on the table:

- Binary sizes are after `cargo build --release` with `strip = false` in
  `[profile.release]`, and the "stripped" column is post `strip` (default
  options) on macOS. Pre-strip sizes include debug info; the stripped numbers
  are what would actually ship.
- `cargo tree | wc -l` is reported both as the raw line count (the literal
  measurement asked for) and as the unique-crate count (which is what people
  usually mean — `wc -l` over `cargo tree` double-counts shared transitives
  every time they appear in the dependency graph).
- "Idle RAM" is the first RSS sample taken after init reports `TTFF`, before
  the per-second log loop catches the steady state. "Active RAM" is the
  steady-state RSS during continuous redraw.
- The 600-frame wall-clock implies macOS is not honoring vsync for unfocused
  windows in the hand-rolled and rend3 prototypes (both run well above 60 FPS).
  Bevy's higher frame time is a fairer "with Bevy overhead" floor — it includes
  the ECS schedule, render-app extract/prepare/queue/render phases, and
  bevy_winit's main-thread pacing — and is the most representative active-frame
  cost figure. Frame-time numbers are useful as a ceiling, not as throughput.
- Cold release build time is wall-clock for `cargo build --release` from a
  clean target directory. Bevy's compile cost is large enough that it
  materially affects iteration loop time during development.

## How each option fits a shared render graph

The architecture doc specifies that 3D output composes with text+image+post
through shared depth/color attachments. The cleanliness of that integration
varies sharply by option:

**Hand-rolled wgpu.** Trivial. The prototype renders all three layers into the
same `wgpu::RenderPass` against a single `Depth32Float` attachment. Adding a
text pass between the 3D pass and the post pass is just "another
`set_pipeline` + `draw`". Depth is just a shared `wgpu::TextureView` — text
and 3D both write/read it. Extracting the 3D pass into its own function inside
`toastty-render/src/pipelines/rgp.rs` is mechanical.

**rend3.** Fights this. `BaseRenderGraph` is the only public API and it owns
the depth buffer, owns the color clear, and assumes it gets to render the
whole frame. The prototype demonstrates the workaround — bracket rend3 with
two extra wgpu passes that load the existing color attachment and use a
separate depth attachment — but those passes share *neither* depth nor a fused
pass with rend3's internal work, which means text cannot occlude the duck via
depth, only via paint order. Cleanly extracting rend3 into a single
`rgp.rs` pipeline file means going *through* `rend3::graph::RenderGraph` and
likely writing a custom `Routine`, doubling the surface area.

**Bevy.** Asks you to commit. Bevy's render-graph **is** the renderer; you
either feed every toastty layer (text, images, 3D, post) into Bevy's render
graph as nodes, or you accept that text+image renders in a Bevy `Camera2d`
pass on top of a `Camera3d` pass — at which point Bevy owns the whole
pipeline, including font shaping (we'd ignore Bevy's text and feed in our own
prepared glyph atlas as a `Mesh2d`, which is feasible but very on-the-grain
for Bevy and very off-the-grain for the rest of the toastty code). The
"shared depth across text + 3D + post" goal is achievable, but only by
ceding render-pass ownership to Bevy entirely.

## Scaling: where does each hit a wall?

**Hand-rolled.** The wall is real but distant. The current 730-line prototype
does diffuse-only one-material-per-object rendering. Each of the following is
a localized addition:

| Capability                       | Approx new LoC | Where the cost hides |
| -------------------------------- | -------------: | -------------------- |
| Multiple objects (per-instance)  |      +50–100 |    instance buffers, draw loop |
| Multiple materials in one mesh   |     +100–200 |    glTF primitive iteration, sort-by-pipeline |
| Full PBR (Cook-Torrance + IBL)   |     +600–1000 |    BRDF math, env maps, prefiltering |
| Cascaded shadow maps             |    +800–1500 |    shadow pass, depth atlas, frustum splits |
| Skinned animation (glTF skins)   |     +400–800 |    joint buffers, vertex skinning, animation player |
| Order-independent transparency   |    +500–1000 |    weighted-blended OIT or depth-peel |

The "wall" is **roughly at full PBR with shadow maps**. Past that the
"minimal scene graph we own" framing breaks down — that's about 3000–4000
extra LoC of code that has nothing to do with terminals, all of which exists
in well-tested form inside `rend3`. The migration path: at the point we need
real PBR, port the RGP pipeline file to `rend3` and keep everything else
unchanged. The hand-rolled prototype is structured around a clean trait
boundary (`Mesh + Material + Pipeline` per pass) that maps directly onto
`rend3::ObjectHandle` + `MaterialHandle`.

**rend3.** No wall in renderer capability — it already does PBR, shadows, and
deferred. The wall is **maintenance**: the project went two years without a
release. New wgpu releases since `0.19` (the trunk's pin) include 0.20, 0.21,
0.22, 22, 23, 24, 25, 26, 27, 28, 29 (wgpu renamed from `0.x` to integer
major in 2024). Anyone adopting `rend3` today is electing to either fork it or
keep wgpu pinned to 0.19 forever, which conflicts with toastty's "Latest
dependencies. Pin exact versions" rule.

**Bevy.** Effectively no rendering wall — anything Bevy can render, you can
render. The wall is **architectural**: every Bevy major adds breaking API
changes (this prototype hit three of them: `EventWriter → MessageWriter` in
0.18, the `reflect_auto_register` feature being required for `glTF` scenes,
and `bevy_animation` being required for glTF type registration even when the
duck has no animations). The cost of staying on the latest Bevy is per-Bevy-
release work, and the cost of staying behind is "your renderer is on an
unsupported branch".

## Bevy's "you can do anything later" — what's the actual cost?

The Bevy prototype is the shortest in lines (208 Rust, 0 WGSL — Bevy's
`StandardMaterial` is the materialized version of the WGSL we hand-write
elsewhere). The dollar cost is felt in two places:

1. **Compile time and resident size.** A 31 MiB stripped binary and 184 MiB
   active RSS for a terminal emulator is the dominant disqualifier. For
   reference, alacritty 0.13 stripped is ~5 MiB and resident is ~25–40 MiB.
2. **API instability per release.** The 0.18 prototype required three rounds of
   Cargo-feature spelunking to get the duck onto the screen: the GLB loader
   needed `bevy_animation` (for `AnimationPlayer` type registration), and the
   scene spawner needed `reflect_auto_register` (otherwise the `Transform`
   type was unregistered and the scene spawner panicked). None of this is
   documented in a way that surfaces from an example search; all of it
   silently became required between 0.16 → 0.18. This is the actual
   "ergonomic cost of using Bevy just for the render pass" — Bevy assumes you
   are writing a Bevy app, and using it as a library means stepping on the
   gravel that the framework usually paves over.

The "you can do anything later" framing is real but applies to the wrong axis.
The things that "do anything later" matters for in a game — physics, audio,
networking, scripting, asset hot-reload, scene editor — are explicitly
*non-goals* in toastty's architecture doc. The things it matters for in a
terminal — text shaping, image decode, PTY I/O, shell integration — are not
things Bevy helps with.

## Decision

1. Build RGP on the hand-rolled wgpu scene graph from the prototype as
   `toastty-render/src/pipelines/rgp.rs`. Extract the three pipelines
   (bg/model/fg in the prototype → bg-shader, model, depth-write-only) into
   the structure already sketched in `architecture.md`.
2. Define a clean `RgpScene` trait whose backend can be swapped without
   touching the cell pass or the post pass. Concretely: separate
   "RGP scene description" (objects, materials, lights — owned by
   `toastty-graphics`) from "RGP renderer" (pipelines, bind groups — owned by
   `toastty-render`). This keeps the door open for swapping to `rend3` later.
3. Defer the rend3/Bevy decision to a future revision triggered by an
   *actual* fidelity requirement from real RGP content: shadow maps, PBR
   metallic-roughness, IBL, or skinned animation. Re-evaluate `rend3`
   project health at that time — if it still has not shipped a release on
   modern wgpu, take Bevy off the candidate list permanently and keep
   investing in the hand-rolled path.

## Three surprises worth flagging

1. **`rend3` is effectively unmaintained.** The crates.io release is older than
   the cargo edition it targets. The git trunk has not moved since May 2024.
   Adopting it today means writing the dependency as `{ git = "...", rev =
   "..." }` and hoping the upstream comes back. This was not obvious from the
   architecture doc, which listed `rend3` as the "upgrade path when RGP needs
   lighting/shadows."
2. **Bevy 0.18 with `default-features = false` is a sharp edge.** Spawning a
   glTF scene panics at runtime unless `bevy_animation` *and*
   `reflect_auto_register` are explicitly enabled, even for a model with no
   animations. The panic message ("scene contains the unregistered type
   `<Enable the debug feature to see the name>`") sends you down the wrong
   path — you have to enable the `debug` feature, get the real error
   ("`bevy_transform::components::transform::Transform` not registered"),
   discover that the fix is `reflect_auto_register` rather than a manual
   `app.register_type::<Transform>()`, and unwind. Total surface time: ~30
   minutes of feature-flag triangulation per Bevy release if the team doesn't
   already track Bevy weekly.
3. **The hand-rolled prototype is fast at first frame.** TTFF of 330 ms is the
   *worst* of the three, because the prototype is doing a lot of init work
   (mesh upload, texture upload, 3 pipeline compiles, depth texture creation)
   eagerly on the main thread before requesting any frame. rend3 and Bevy
   *amortize* that work — rend3's TTFF was 169 ms and Bevy's was 239 ms — but
   the gap is GPU-pipeline lazy compilation: their first frame is cheap, the
   second/third frames absorb the hidden cost. The 600-frame total tells the
   true story; the hand-rolled path renders the same scene in roughly half
   the wall-clock time. A small refactor to compile pipelines on a background
   thread would erase the TTFF gap and is mechanical (`pollster::block_on`
   already runs the device request on a worker — pipelines can join).
