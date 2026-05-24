# M12 — RGP (Ratty Graphics Protocol) inline 3D

**Goal.** First non-Ratty terminal to ship the Ratty Graphics Protocol —
inline 3D objects anchored to terminal cells, composed depth-first with text
and images. The ecosystem bet from `architecture.md`.

**Companion decision records.**
[`rgp-3d-path.md`](../decisions/rgp-3d-path.md) selects the renderer library
(hand-rolled wgpu). [`rgp-protocol.md`](../decisions/rgp-protocol.md) settles
the path-policy, scene-shape, and depth-composition questions.

## Scope

- Five RGP verbs: support query (`s`), register (`r`), place (`p`), update
  (`u`), delete (`d`).
- Both register sources: `payload=` (base64-encoded glTF inline in APC,
  optionally chunked via `more=1`/`more=0`) and `path=` (leaf-name lookup
  against an embedded asset bundle plus an optional user config directory —
  no arbitrary disk reads).
- Asset format: `.glb` only in v1 (advertise `fmt=glb`, omit `obj`).
- Depth-tested 3D pass between background and cells, sharing a
  `Depth32Float` attachment with the text and image pipelines so cell-layer
  text can occlude objects or sit underneath them based on the per-object
  `depth=` field.
- Animation: `animate=1` interpreted as a slow Y-axis spin (placeholder).
  glTF animation channels are out of scope.
- Lighting: hardcoded single sun direction + ambient term, modulated by
  the protocol's `brightness=` field. No PBR.

## Architecture (overview)

```
PTY bytes
   │
   ▼
toastty-parser (APC streaming scanner) ──── start / chunk / end ───▶ Term
                                                                       │
                                                            ┌──────────┴──────────┐
                                                            ▼                     ▼
                                          (peek prefix on apc_end)
                                            "G…"           "ratty;g;…"
                                            ▼                     ▼
                                     KittyHandler ─▶ Term  RgpHandler ─▶ RgpScene
                                                                       (in toastty-graphics)
                                                                              │
                                                                              ▼
                                                              toastty-render reads &RgpScene
                                                              (pipeline + GPU cache)
```

`RgpScene` is a concrete struct with `&self` accessors only — no trait, no
Bevy escape hatch (see decision §2). The renderer sync logic mirrors the
M11 image-revision pattern: on `rgp_revision` advance, diff cached GPU
resources against the scene and upload/evict deltas.

## Phasing

M12 lands as five PRs, sized to be reviewable independently. Each later
phase depends only on the phase before.

### M12a — protocol plumbing (no rendering)

- New crate module `toastty-graphics::rgp` with `parser`, `header`,
  `scene`, `handler`, `reply` files.
- `RgpHandler` + `RgpSink` (mirror of `KittyHandler`/`KittySink`).
- Chunked-payload reassembly per object id, capped at 64 MiB.
- `apc_end` demux in `toastty-term`: `G…` → Kitty (unchanged),
  `ratty;g;…` → new RGP handler.
- Support-query reply (`s` verb) returns capabilities (`v=1;fmt=glb;path=1;
  payload=1;chunk=1;anim=1;depth=1;color=1;brightness=1;transform=1;
  update=1`).
- Tests: parser per-verb, scene mutations, chunk reassembly, APC demux.
- No GPU work touched.

### M12b — asset loading

- `gltf` crate dependency (pinned exact version per workspace rule).
- Embedded asset bundle: `include_bytes!`-baked `.glb` files in
  `crates/toastty-graphics/assets/`. Leaf-name lookup.
- Optional config-dir resolver: `[rgp] asset_dir` in `toastty-config`;
  canonicalizes against the configured root to defeat symlink escape.
- `r` verb wired end-to-end: payload (base64 decode + glTF parse) and
  path (leaf-name resolve + glTF parse) both produce `CpuAsset` in the
  scene.
- Eviction on `d` verb.
- Tests: path resolver rejects separators/parent components; loader
  decodes the bundled demo asset; chunked payload register reassembles
  across `more=1`/`more=0` packets.

### M12c — placement state & animation tick

- `p` verb creates an `RgpPlacement` entry.
- `u` verb does field-by-field merge over the existing placement (any
  field absent in the update is preserved).
- `d` with `id=` drops one placement; without `id` wipes all RGP state.
- `RgpScene::animation_deadline(now)` returns a ~16 ms tick when any
  placement has `animate=1`; `tick_animations(now)` advances phases.
- `Renderer::next_redraw_deadline` combines this with the existing
  cursor-blink deadline. Animation forces non-skipped frames the same
  way the blink path already does.

### M12d — renderer integration

- `scratch_depth_texture` + `scratch_depth_view` alongside the existing
  scratch color FB. Recreated on resize. Format `Depth32Float`.
- Text and image pipelines: depth attachment added with
  `depth_compare: LessEqual`, `depth_write_enabled: true`. Vertex shaders
  output a fixed `z = 0.5` (NDC).
- `toastty-render::rgp::pipeline`: per-draw uniform (model matrix, color
  tint, brightness, texture-present flag), single WGSL shader doing
  lambertian + ambient + optional base-color texture sample.
- GPU mesh/texture cache keyed by asset id; `scene_sync` diffs against
  `rgp_revision` and uploads deltas.
- Draw order inside the render pass: RGP → image-below → text → image-
  above. Depth test handles intra-frame composition; paint order handles
  the image-vs-text z-ordering inherited from M11.
- Camera: orthographic, world units = terminal cells, looking down -Z.
- Snapshot tests: render bundled demo asset at known placements, golden
  PNG diff. Two variants for depth interaction (`depth=-5` vs `depth=+5`).
- **Risk:** highest of the five phases. Adding depth to the text pipeline
  means every existing snapshot needs re-baking. Co-ordinate the
  snapshot refresh with the pipeline change in one PR to avoid a noisy
  diff against intermediate states.

### M12e — demo + protocol docs

- `scripts/demo-m12.sh` — emits the wire bytes for register + place +
  update + delete against the bundled demo asset. Mirrors
  `scripts/m11-demo.sh`.
- `docs/protocols.md` gets an RGP section documenting our advertised
  capabilities and the depth-mapping convention.
- Update `architecture.md`'s decisions table and milestone list.
- Add RGP capability marker to `terminfo/toastty.terminfo` if a
  precedent exists.

## Dependencies on earlier milestones

- **M5** (streaming APC scanner) — already shipped; RGP uses
  `Perform::apc_start`/`apc_chunk`/`apc_end` with the buffered shape.
- **M11a** (image registry + revision-bump sync) — supplies the design
  pattern that RGP follows for "renderer pulls from `Term` on revision
  advance and forces a full clear that frame."
- **M9** (damage tracking) — RGP frames bump `rgp_revision`, which
  cascades into a full clear (same shortcut as M11a). Per-placement
  damage tracking is deferred.

## Out of scope

- `.obj` format. `tobj` is small but punt to v1.1.
- `path=` access outside the embedded bundle + configured asset dir.
- glTF animation channels (skins, morph targets, keyframes).
- PBR materials, shadow maps, IBL, normal mapping. Escape hatch is to
  implement these in hand-rolled wgpu — *not* to introduce Bevy
  (`rgp-3d-path.md` and `rgp-protocol.md` both pin this).
- Whole-terminal 3D surface transform. The depth conventions in
  `rgp-protocol.md` preserve the option; the transform itself is post-
  M13 work.

## If M12d slips

Each of these is a 1–2-file revert that keeps the protocol surface
intact (apps still see a conformant terminal — just one with a less
capable renderer):

1. Drop animation tick (M12c logic stays parsed; always-on `animate=0`).
2. Drop `base_color_texture` support — solid-color materials only.
3. Drop depth interaction with text (RGP always on top; cell pass keeps
   `depth_stencil_attachment: None`).

The protocol parsing and scene state (M12a–c) ship even if the renderer
slips.
