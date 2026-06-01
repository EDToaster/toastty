# TGP — Toastty Graphics Protocol (Design v2)

- **Status:** Design v2 — all ambiguities resolved (see `tgp-decisions.md`, `tgp-reconciliation.md`); ready for implementation planning.
- **Date:** 2026-05-31
- **Supersedes:** `2026-05-29-tgp-design.md` (v1 draft).
- **Binding decisions:** clean break from RGP (no adapter); per-app namespace token; placeholder-cells-authoritative inline viewports; sub-captures-viewport-cells input routing. Full rationale in `tgp-reconciliation.md`.

> This is a clean-break protocol. The legacy RGP (`ratty;g;`) implementation remains as independent, untouched code; TGP neither bridges to nor inherits from it.

---


## 1. Summary

TGP is a modern, retained-mode 3D graphics protocol for the toastty terminal. It is a deliberate
**clean break** from RGP: a brand-new `tgp;` escape-sequence namespace with a compact binary wire format,
a real **scene graph**, multiple **cell-anchored viewports**, true **GPU instancing**, **registerable
materials & lights**, and — the category-defining feature no terminal graphics protocol has — **interactive
3D** (clicking/hovering objects routed back to the application).

TGP does not extend, adapt, or bridge to RGP. There is no RGP adapter, no shared semantics, and no
compatibility carve-out. The existing RGP (`ratty;g;`) implementation remains in the tree as untouched,
independent legacy on its own code path; TGP neither reuses nor inherits from it (see §4).

Every TGP session is scoped to a **per-app token**. The app proposes a short token at handshake; the
terminal echoes (or assigns) one, and from then on every frame the app sends — and every reply, event, or
error the terminal emits — carries that token in its text header. The token **namespaces all addressing**
(node / asset / material / clip / viewport ids live under the token that created them) and **tags every
emitted frame** so a reader sharing the PTY can demultiplex. This is the foundation for safe-by-default
multiplexing: a stray `curl | cat` line cannot reach into another app's scene, and stray event bytes are
identifiable rather than indistinguishable garbage.

### 1.1 Why a new protocol

RGP today (per grounding):

- **Flat object model.** `RgpScene { assets, placements, revision, asset_revision, … }` _(scene.rs:71–87)_ —
  no node hierarchy, no parent/child transforms; placements are independent `u32`-keyed entries.
- **"Place-many" reuse but no GPU instancing.** Each placement is its own `draw_indexed(…, 0..1)` with its
  own uniform buffer _(pipeline.rs:287)_ — N placements = N draws.
- **Single, non-isolated viewport.** One surface, one ortho camera; cell anchors mapped to pixels
  _(pipeline.rs:196–212)_; one shared `Depth32Float` buffer interleaving text/image/3D by z-order
  _(lib.rs:1539–1553)_.
- **Hardcoded lighting.** `rgp.wgsl` = Lambertian + ambient, fixed sun dir/color; UVs parsed but unused;
  no PBR, no textures, no shadows. Animation = hardcoded spin at 1.0 rad/s _(scene.rs:27)_.
- **Limited assets.** GLB only, first primitive of first mesh only; OBJ rejected at load; `baseColorFactor`
  only. `path=` reads **any** absolute/CWD-relative path _(path_resolver.rs:45–77)_ — a real untrusted-input risk.
- **No interactivity, no accessibility, no structured errors** (malformed input is silently dropped,
  _term.rs:3417_). No per-app isolation: a single global scene with no token, so any output stream can mutate
  any object. Does not survive SSH well (not fully GPU-resident).

TGP fixes all of the above and adds the differentiators below. It does so on a fresh code path rather than
by retrofitting RGP, which keeps the legacy implementation stable while TGP is free to choose its own model.

### 1.2 What sets TGP apart

1. **Interactive 3D** — hit-testing + event routing back to the app. *Nobody* in the field does this.
2. **Retained scene graph** with parent/child transforms, animation, skinning — vs RGP's flat list.
3. **True cheap instancing** — one mesh, thousands of tinted copies in a single draw.
4. **Multiple terminal-native viewports** that interleave with text, and reflow on scroll/resize.
5. **Safe by default** — no file access at all (assets are inline), structured capability negotiation,
   per-token addressing isolation, app-owned fallback.

---

## 2. Design principles

These are load-bearing. Every later decision is justified by reference to them.

1. **App-authoritative; the terminal stays simple.** The app owns the scene and drives it. The app
   issues transforms by default. The terminal renders and reports — nothing more.
2. **Conveniences are opt-in.** Anything where the terminal takes control — grab-to-explore camera,
   built-in animation playback, fancy materials — happens **only** when the app explicitly registers or
   requests it. No surprise behavior.
3. **Safe by default.** Bytes arrive from anywhere a terminal prints (`curl | cat`, a log line, a web
   page). TGP must be safe to receive from untrusted output: no file access, bounded resources, hardened
   parsers, structured errors instead of crashes, and **per-token addressing isolation** so one stream
   cannot mutate another's objects.
4. **Fallback is the app's job.** The terminal answers a reliable capability query; the app decides
   whether to send TGP or degrade to sixel/ASCII/text. The terminal never auto-degrades.
5. **Compact on the wire.** Binary encoding, diff-style updates, instancing — designed to survive SSH/PTY
   bandwidth.
6. **Per-app isolation over a shared channel.** A PTY has one writeback channel and one input stream shared
   by every process on it, so the terminal cannot physically deliver bytes to just one process. TGP does not
   pretend otherwise: it isolates **addressing** (token-namespaced ids), **gates emission** (only for live
   handshaked tokens, only while focused), and **tags every emitted frame** with its token so the reader can
   demultiplex. Routing is the reader's job; isolation and tagging are the terminal's.
7. **Own the design; don't inherit.** TGP is a clean break. It is free to choose its own scene model, wire
   format, defaults, and constants without matching any RGP behavior byte-for-byte. RGP stays as independent
   legacy on its own code path.

---

## 3. Goals / non-goals

### 3.1 Goals (v1 scope)

- Clean-break `tgp;` namespace + compact binary framing, fully independent of RGP.
- Per-app **token**: proposed in the query, echoed/assigned in the reply, carried in every frame's text
  header; namespaces all ids and tags every emitted reply/event/error.
- Capability negotiation (always-answered, per-feature flags, versioned).
- Retained scene graph: typed nodes, parent transforms, instancing, per-node visibility + alt-text.
- Multiple viewports: inline (flows with text) and pinned (fixed), app's choice; offscreen depth-aware
  compositing; terminal-driven reflow on resize/scroll. Inline position is anchored by **authoritative
  placeholder cells**; any `{line,col}` is only a hint.
- One default material (zero config); registerable PBR/material + lights (opt-in).
- Interactivity: opt-in terminal-side explore controller; opt-in picking + click/hover event reports; an
  active sub captures the pointer within its viewport cell rect (suppressing SGR mouse reporting there); a
  raw escape-hatch mode forwarding pointer events to the app.
- Assets inline only (glTF/GLB bytes in the protocol); **no file loading**.
- Structured error replies; resource caps; bounds-checked asset parsing.

### 3.2 Non-goals (explicitly deferred — see §16)

- Any RGP adapter, bridge, or compatibility layer (the clean break is permanent; see §4).
- Per-process physical delivery of events (impossible over a single shared PTY — TGP provides addressing
  isolation, emission gating, and per-frame token tagging instead).
- Sandboxed file/path loading (kept out entirely "for now").
- Image-based lighting (IBL), real-time shadows, post-processing stacks.
- HDR / wide-gamut surface signaling.
- A declarative client library (the wire is imperative-patch; a declarative layer can sit on top later).
- Full skeletal animation polish (skinning is in the model; production-grade rig support is staged).
- Text rendered as a 3D scene node ("labels in 3D space") — possible later via a `text` node kind.

---

## 4. (removed)

The original "Relationship to RGP" section has been deleted. TGP is a clean break: there is no RGP adapter,
no demultiplexing of `ratty;g;` into the TGP scene model, no shared scene model or renderer fork, and no
compatibility carve-out for RGP semantics (including its permissive `path=` loading). The existing RGP
implementation remains untouched as independent legacy on its own code path; TGP does not bridge, reuse, or
inherit from it, and the rest of this document does not reference RGP semantics as a basis for TGP behavior.

---

## 5. Architecture overview

```
┌──────────────────────────────────────────────────────────────────────┐
│ toastty terminal                                                       │
│                                                                        │
│  PTY bytes ─► escape parser ─► [tgp framer]                            │
│                                     │                                  │
│                                     ▼                                  │
│                              TGP op decoder                            │
│                              (token demux + addressing isolation)      │
│                                     │                                  │
│                                     ▼                                  │
│                                   ┌───────────────────┐                │
│                                   │   Scene model     │  (retained)    │
│                                   │  per-token ids    │                │
│                                   │  assets/materials │                │
│                                   │  nodes (graph)    │                │
│                                   │  viewports        │                │
│                                   │  dirty-tracking   │                │
│                                   └─────────┬─────────┘                │
│                                             ▼                          │
│  input (mouse/keys) ─► interaction router ─► renderer (wgpu)           │
│        ▲                      │              ├─ per-viewport offscreen  │
│        │                      │              │   color+depth targets    │
│        │                      │              ├─ pick pass (node-id)     │
│        │                      ▼              └─ depth-aware composite    │
│        └────  tagged (tok=) event reports ──► PTY (stdin to app)       │
└──────────────────────────────────────────────────────────────────────┘
```

There is exactly **one** ingress path. The escape parser hands `tgp;` frames to a single TGP framer/op
decoder — no second framer, no adapter. The decoder reads the **token** from each frame's text header,
enforces that the frame may only address objects created under that token (a mismatched or absent token that
references another token's ids is rejected with a structured error), and applies the ops to the retained
scene model. All ids in the model are **namespaced per token**; what was previously "one global namespace per
session" is now "one namespace per token."

The scene model is the single source of truth on the CPU: node graph with world matrices, viewport pixel
rects, decoded instance buffers, asset/material tables, and dirty flags are all computed and queryable
CPU-side; the GPU only consumes them. The renderer draws each viewport to its own offscreen color+depth
target and composites them into the framebuffer with a depth test against the text plane.

On the way back out, the interaction router resolves pointer events to nodes (with a CPU-resolvable pick
path, GPU acceleration optional) and emits replies, acks, events, and errors on the **single shared PTY
writeback channel**. The terminal cannot deliver these to one specific process; instead each emitted frame
is **tagged with its `tok=`**, emission is **gated** to live handshaked tokens (and only while the terminal
is focused and the owning subscription is alive), and demultiplexing is the reader's responsibility. When the
foreground process group changes or the input side closes, the terminal cancels that token's
subscriptions/explore/animation and stops emitting, so event bytes never leak into a shell prompt as garbage.

Crates (current layout, _grounding_): the existing RGP protocol/model lives in
`crates/toastty-graphics/src/rgp/*` (`scene.rs`, `operation.rs`, `handler.rs`, `parser.rs`) and rendering in
`crates/toastty-render/src/rgp/*` (`pipeline.rs`) + `crates/toastty-render/shaders/rgp.wgsl`. TGP adds its
own sibling `tgp` modules with their own scene model, framer/decoder, and render pipeline. It does not fork
or generalize the RGP modules; the RGP path is left in place, untouched and independent.

## 6. The scene model

A single retained scene per terminal session, plus a set of viewports that look into it. The
scene is **namespaced per app token** (§8.4): every node, asset, material, clip, and viewport id is
scoped to the token that created it, so two cooperating apps — or an app and a hostile `curl | cat`
line sharing the same PTY — cannot see or mutate each other's objects.

### 6.1 Data structures (conceptual)

```rust
struct Scene {
    assets:     Map<AssetId,    Asset>,      // meshes, textures, skins — uploaded once
    materials:  Map<MaterialId, Material>,   // default material is implicit id 0
    nodes:      Map<NodeId,     Node>,       // the graph
    roots:      Set<NodeId>,                 // parent-less nodes
    viewports:  Map<ViewportId, Viewport>,
    clips:      Map<ClipId,     AnimationClip>,
    reasm:      Map<AssetId,    InFlightChunks>, // open chunk buffers, NOT yet assets (§6.4, §8.7)

    asset_revision:  u64,   // monotonic, saturating; wrap unreachable in practice
                            // bumps on asset/material/skin commit changes → re-upload GPU buffers
    scene_revision:  u64,   // monotonic, saturating; wrap unreachable in practice
                            // bumps on transform/visibility/instance/material-ref changes
    dirty:           DirtySet, // per-node + per-viewport dirty flags (see §6.5)
}

struct Node {
    id:        NodeId,        // app-assigned, token-scoped string (see §8.4)
    parent:    Option<NodeId>,
    trs:       Trs,           // local transform (translate / rotate quat / scale)
    kind:      NodeKind,      // immutable for the lifetime of the id (see §6.2)
    visible:   bool,          // cheap show/hide without delete+re-add
    alt:       Option<String>,// accessibility caption (see §13)
}

enum NodeKind {
    Group,                                       // transform-only container
    Mesh   { asset: AssetId, material: MaterialId, tint: Rgba },
    Instanced { asset: AssetId, material: MaterialId,
                instances: InstanceBuffer },     // mat4-only v1; no node-level tint (see §6.4)
    Light  { light: Light },                     // see §11.3
    Camera { camera: Camera },                   // see §10
    // future: Text { … }  (labels in 3D space) — deferred
}
```

**Per-field clear defaults under sparse merge.** `node.upsert` is a sparse merge driven by the CBOR
op map (full semantics in §8.6). For each optional field the three-way distinction is: key absent →
preserve; key present and CBOR null → clear to the default below; key present with a value → set.

| Field | Clear default (CBOR null) | Notes |
|---|---|---|
| `parent` | `None` (node re-roots into `roots`) | Children and animation phase retained |
| `alt` | `None` | — |
| `tint` (Mesh) | opaque white `[255,255,255,255]` | Identity in the color chain (§6.4) |
| `trs` | not individually nullable | Replaced wholesale when present |

A CBOR null on a non-nullable field (`id`, or the kind-selector key) is rejected with
`x code=null_not_allowed` naming the field; the whole txn is rejected atomically (§8.6).

### 6.2 Transforms & hierarchy

- Each node has a **local** TRS. **World transform = product of the parent chain** (root → … → node).
- "Move the parent, children follow" — the headline reason for the graph. A robot arm is a chain of
  `Group`/`Mesh` nodes; rotating the shoulder cascades to the hand with **one** patch op, not N.
- World transforms are computed lazily on the **dirty subtree** before each render (see §6.5), never the
  whole tree unless the whole tree changed.
- **The committed scene is always acyclic.** At txn commit — after applying all ops to a staged copy
  and resolving references (§8.6) — the terminal walks every node's parent chain with an iterative,
  visited-set traversal (no recursion, so the validator itself cannot overflow). A self-parent
  (`A.parent = A`) or any cycle (`A → B → … → A`) rejects the **whole** txn with `x code=cycle`,
  `detail=<id-in-cycle>`, citing the op that introduced or last touched a node on the cycle. A parent
  chain that would exceed `max_node_depth` (256, advertised in caps), even when acyclic, rejects with
  `x code=depth_exceeded`, `detail=<id>`. The scene is left unchanged. The lazy render-time
  world-matrix walk carries the same depth cap as defense-in-depth, but commit-time validation
  guarantees committed state never reaches it.

### 6.3 Why a graph (recap of the decision)

Use-case analysis identified three dynamism classes: (1) **inspect** (rigid model + camera orbit), (2)
**bulk data-viz** (many independent moving instances), (3) **articulated/animated** (correlated hierarchy).
Transform propagation pays off only for class 3. TGP commits to the full graph **now** because (a)
articulated/skeletal animation is a day-one goal, and (b) glTF's animation + skinning are *defined* over a
node tree — flattening on import is lossy for exactly those assets. Classes 1 & 2 pay almost nothing: a
flat scene has trivial 1-long parent chains, and bulk data uses instancing (§6.4), not propagation.

### 6.4 Instancing

One registered mesh, drawn many times in **one** GPU draw call — the path for point clouds, live
charts, and molecule atom/bond sets, none of which should pay one draw per copy.

**Canonical instance layout (v1).** An `Instanced` node carries a single pinned buffer layout; the
earlier `Trs | mat4` ambiguity collapses to **mat4 only**. The `node.instances` op carries:

- `count: u32` — the number of instances.
- `xforms` — a CBOR byte string of exactly `count * 64` bytes: per instance, 16 little-endian IEEE-754
  `f32` in **column-major** order (a mat4 model matrix). This matches the renderer's `@location`
  instance-step vertex attributes with **zero CPU re-pack** and no per-record `Trs → mat4` math on the
  hot path.
- `tints` — **optional** CBOR byte string of exactly `count * 4` bytes: per instance, R,G,B,A as `u8`,
  sRGB-encoded (matching the `color=srgb` capability). If absent, every instance defaults to opaque
  white.

**Hardened validation (§15.5).** `xforms.len()` must equal `count * 64` and, when present,
`tints.len()` must equal `count * 4`, else `x code=bad_layout` with `detail=xforms_len | tints_len |
count_mismatch`. `count` must be `≤ max_instances_per_buffer`, else `x code=cap_exceeded`,
`detail=max_instances`. Any NaN/inf matrix element is rejected (never clamped) with `x
code=parse_error`, `detail=instance_nan`. An over-cap or malformed `node.instances` txn is rejected
atomically and the node **retains its last committed instance buffer** — no flicker, no partial
upload.

**Rendering.** The renderer uploads the buffer to an instance vertex buffer and issues
`draw_indexed(indices, 0..count)` with per-instance attributes; the record index `0..count` is the
`instance_index` used for picking (§12.4).

**Per-instance vs node-level tint.** A `Mesh` node carries one node-level tint; an `Instanced` node
carries **per-instance** tints (above) and **must not** set a node-level tint — an upsert or
`node.instances` that sets a node-level tint on an `Instanced` node is rejected with `x
code=kind_conflict`, `detail=instanced_node_tint`. Per-instance tint multiplies exactly like a Mesh
tint in the color chain (§6.1, §11.2): `final_linear = baseColor_linear * tint_linear * brightness`,
with the sRGB tint decoded to linear before the multiply and alpha multiplied straight through — it is
**not** an emissive add and **not** a baseColor replace.

**Single-instance re-tint (the §12.6 click-highlight path).** To recolor one instance without
re-sending `xforms` or the whole buffer, send `node.instance_set { id, index: u32, tint:[r,g,b,a] }`.
It writes exactly one `tints` record and sets the node's `instance_dirty` flag (§6.5), avoiding a full
re-upload. `index ≥ count` is rejected with `x code=bad_index`, buffer unchanged. This is distinct
from `node.instances`, which replaces the whole buffer.

- **Static instances:** uploaded once; subsequent renders reuse the GPU buffer until `instance_dirty`
  is set again.
- **Dynamic/bulk instances** (class 2 data-viz): re-upload the buffer per frame via `node.instances`
  carrying only the changed buffer — the point-cloud / live-chart path. This sets `instance_dirty` and
  bumps `scene_revision`; it **never** bumps `asset_revision` (§6.5), so the mesh is not needlessly
  re-uploaded each frame.
- **Per-instance material** is deferred (§16): in v1 all instances of a node share the node's single
  material, distinguished only by per-instance tint. Full per-instance material handles (material
  arrays / bindless) are out of scope.

### 6.5 Dirty-tracking (the render gate)

Three independent notions of "changed", so we never do more GPU work than necessary. The `u64`
revisions are coarse monotonic change tokens (incremented with `saturating_add`, so wrap is
unreachable even in principle) and the asset-vs-scene split signal — they are **not** the per-frame
gate. The renderer's recompute/re-upload decision instead consumes explicit **per-node and
per-viewport dirty flags**, cleared after consumption, eliminating any dependence on numeric revision
equality.

| Change | Bumps | Effect |
|---|---|---|
| Asset/material/skin upload committed (`more=0`) | `asset_revision` | Re-upload GPU buffers/textures |
| Node transform / visibility / tint | `scene_revision` + per-node transform/visibility dirty flag | Recompute dirty subtree world matrices; re-render |
| Instance buffer (`node.instances` / `node.instance_set`) | `scene_revision` + per-node `instance_dirty` flag | Targeted instance-buffer re-upload for that node only; grow the buffer in place if `count` exceeds capacity; **never** `asset_revision` |
| Viewport anchor/size/camera/settings | per-viewport dirty flag | Recompute pixel rect; re-render (or just re-blit the cached layer if only moved — §10.4) |

`instance_dirty` is a dedicated per-node flag in `DirtySet`, orthogonal to that node's
transform/visibility flag, and is cleared by the renderer after the instance vertex buffer is
uploaded. Keeping instance changes off `asset_revision` is what lets a per-frame point cloud avoid
re-uploading every mesh in the scene each frame; mesh uploads stay gated solely by `asset_revision`.

Terminal-side motion — explore damping, animation playback (§12, §14) — drives this same machinery:
it sets the relevant per-node dirty flags and re-renders only the affected viewport(s). It does
**not** bump global `scene_revision` every frame (preserving the no-re-upload optimization) and does
not re-emit unrelated cells.

The result is fine granularity: a 10k-node scene with one moving node recomputes exactly one subtree,
and a live point cloud re-uploads exactly one instance buffer.

## 7. Capability negotiation & handshake

The single thing the terminal *must* do well, because the app's entire fallback strategy depends on it
(principle 4). The historically flaky part of terminal protocols is "I asked and heard nothing — is it
unsupported, or just slow?" TGP solves this explicitly: every well-formed query is **always answered**, the
answer is **ordered ahead of a reply every terminal already sends**, and the completed handshake doubles as
the session's opt-in for structured error reporting.

### 7.1 Query / reply

```
app → term:   ESC _ tgp;q;v=2;vmin=1;tok=app-7 ESC \    # "do you speak TGP? I support v1..v2; my token is app-7"
term → app:   ESC _ tgp;r;v=2;vmin=1;tok=app-7;caps_gen=1;
                 feat=geom,graph,instance,material,pbr,light,pick,event,explore,anim,bintrailer;
                 enc=b64,bin;color=srgb;
                 max_verts_per_asset=4000000;max_indices_per_asset=8000000;
                 max_texels_per_texture=67108864;max_tex_dim=8192;
                 max_instances_per_buffer=1000000;max_asset_bytes=67108864;
                 max_nodes_session=1000000;max_node_depth=256;max_viewports_session=64;
                 max_vram_mb_session=512;max_msg_mb=64;max_reasm_mb=256;max_pending_bytes_session=268435456;
                 max_inline_cols=256;max_inline_rows=256;
                 max_viewport_px_w=8192;max_viewport_px_h=8192;
                 max_lights_per_viewport=16;anim_speed_max=16;camera_report_hz=30;
                 max_id_bytes=64;msaa=1,4;click_deadzone_px=3;
                 max_backchannel_bytes=4096 ESC \
```

#### Always-answered (normative)

The no-hang guarantee is realized by an app-side pairing convention plus two terminal-side obligations:

- The terminal **MUST** emit a `tgp;r` for **every** well-formed `tgp;q`, regardless of the requested
  version — it **MUST NOT** silently drop a recognized query. ("Well-formed" means the frame parses as a
  `tgp;q`; a query with a malformed or absent `v=` is still answered, carrying the supported range, and
  **MAY** additionally draw a `tgp;x;code=parse_error` — see version rules below.)
- When a `tgp;r` and a primary Device Attributes (DA1, `ESC [ c`) reply are both pending on the shared
  writeback channel within one parse batch, the terminal **MUST** enqueue the `tgp;r` **strictly before**
  the DA1 reply.
- The recommended detection handshake is for the app to send `ESC _ tgp;q;… ESC \` **before** `ESC [ c`.
  Writebacks are FIFO in parse order, so combined with the ordering obligation above the `tgp;r` is
  guaranteed to reach the app before the DA1 reply.

The canonical app inference follows directly: **"DA1 reply observed with no preceding `tgp;r` ⇒ TGP is
absent"** — the app concludes "no TGP" with certainty and never hangs. A secondary app-side timeout
(recommended default 250 ms) is documented as belt-and-suspenders only; it is **not** a terminal obligation
(the terminal cannot enforce that an app paired its queries). The TGP reply is independent of any other
graphics-capability reply the terminal may send; neither implies the other, and an app **MUST NOT** infer
TGP presence from an unrelated protocol's reply.

#### Versioning & three-way detection (normative)

`tgp;r` **always** carries both `vmin=` (lowest supported version) and `v=` (the negotiated version), so the
*presence of any `tgp;r`* unambiguously means "TGP exists" and the app computes compatibility locally.

- `v=` is **required** in `tgp;q`; omission is a `parse_error`. Because the writer has not yet established a
  session, an omitted/malformed `v=` is still answered with a `tgp;r` carrying the supported range and **no
  negotiated session**, never a silent drop.
- The negotiated version is `min(app_max, term_max)`. If `app_max < term_min` (the app is too old), the
  terminal still replies, with `v=` set to `term_min` and `vmin=` advertising the true minimum, so the app
  can **knowingly** refuse rather than face silence.
- The app's decision is therefore three-way: `no tgp;r before DA1` ⇒ **absent** (fall back to
  sixel/ASCII/text); `tgp;r present and app's range overlaps [vmin..v]` ⇒ **usable**; `tgp;r present but the
  ranges do not overlap` ⇒ **present but incompatible** (prompt to upgrade, or take a different fallback
  path). These are three distinct fallback paths, which is why presence must never be conflated with
  usability.

#### Per-feature flags (`feat=`)

`feat=` is a **frozen, closed vocabulary for v1**, so an app can branch its fallback on exact token strings
and degrade *partially* (e.g. draw geometry but skip `pick` if absent). The v1 set is:

```
geom  graph  instance  material  pbr  light  pick  event  explore  anim  bintrailer
```

- **Implication rules** (the terminal MUST honor when advertising; the app MAY rely on): `pbr ⇒ material`,
  `pick ⇒ event`, `explore ⇒ event`, `instance ⇒ graph`, and `anim ⇒ graph`. A terminal **MUST NOT** advertise a token
  without also advertising every token it implies. Missing flag = feature absent.
- **Forward-compat.** Apps **MUST ignore** unknown `feat` tokens and **MUST NOT** infer any dependency not
  listed here (parity with the field-skipping rule in §7.2). The vocabulary is versioned with `v=`: new
  tokens are introduced only by bumping the negotiated version, so an app that negotiated `v=1` never sees a
  `v=2`-only token.
- There is **no `binframe` token** — binary framing is advertised solely through `enc=` (below).
- `bintrailer` advertises support for the opt-in per-frame binary integrity trailer (§8.2); it is
  independent of `enc=`.

#### Encoding (`enc=`)

`enc=` is the **single source of truth** for payload encoding (§8.2–8.3), listed in the terminal's
recommended order:

- `enc=b64` is the **safe default** and is listed first; raw binary frames are reserved for transports the
  terminal believes can carry them. A terminal behind a passthrough that cannot carry raw bytes advertises
  `enc=b64` only.
- An app **MUST NOT** send `enc=bin` unless the reply advertised `bin`; doing so is a `parse_error`.
- Because a live multiplexer can still mangle raw bytes after the reply was generated, an app **SHOULD**
  verify the transport before streaming bulk binary: send a tiny `enc=bin` probe (a minimal no-op patch) and
  expect a `tgp;a` ack for that `txn`. Absence of the ack within the app's timeout means the live transport
  mangled the frame — the app downgrades to `enc=b64` for the session. (Acks are available to any
  handshaked session for exactly this probe and for txn-commit confirmation; see §8.5.) A binary frame that
  arrives but fails to decode draws a `tgp;x;code=parse_error`, never silent corruption.

The terminal never auto-detects multiplexers and never auto-degrades; the app owns the bin-vs-b64 choice
(principle 4).

#### Color

`color=srgb`: wire colors are sRGB-encoded. The renderer works in linear light internally and sRGB-encodes
into each viewport's offscreen target before compositing, so the composite against the (sRGB) text plane
matches (§11).

#### Limits (advertised = enforced)

The reply advertises the terminal's hard caps so an app can pre-trim instead of being rejected. **The reply
and the enforced cap list (§15.2) are the same closed set**, and the terminal **MUST NOT** enforce a cap it
did not advertise. Each cap name carries an explicit **scope suffix** so the app can budget a
multi-viewport, multi-instanced scene unambiguously:

- **Per-asset / per-buffer:** `max_verts_per_asset`, `max_indices_per_asset`, `max_texels_per_texture`,
  `max_tex_dim`, `max_instances_per_buffer`, `max_asset_bytes` (per-id pending reassembly bytes).
- **Per-message:** `max_msg_mb` (the **decoded** payload size of one frame; it applies to both encodings —
  for `enc=b64` the encoded in-APC length is bounded at `ceil(max_msg_mb*4/3)`).
- **Per-session:** `max_vram_mb_session`, `max_nodes_session`, `max_node_depth`, `max_viewports_session`,
  `max_reasm_mb`, `max_pending_bytes_session`, `max_lights_per_viewport`, `anim_speed_max`,
  `camera_report_hz`, `max_backchannel_bytes`.
- **Inline / viewport sizing:** `max_inline_cols`, `max_inline_rows` (placeholder-cell limits for inline
  viewports), `max_viewport_px_w`, `max_viewport_px_h` (per-viewport pixel-rect limits).
- **Rendering / input:** `msaa` (supported MSAA sample counts), `click_deadzone_px` (explore
  click-vs-drag threshold), `max_id_bytes` (node-id byte length).

Caps are advisory hints for pre-trimming; the authoritative enforcement is the structured `tgp;x` error
(§15.3), which cites the breached cap by its exact advertised field name in `detail=`. Exceeding a cap is
never a crash or unbounded allocation.

#### Per-app token (`tok=`)

A session is identified by an app-chosen **token** (`tok=`, ≤ 16 bytes, printable, no control/ESC bytes).
The app proposes it in `tgp;q`; the terminal accepts and echoes it in `tgp;r` (assigning one if the app
omits it). The token scopes the session for the rest of its life:

- **Addressing isolation (hard-enforced).** Every node, asset, material, clip, and viewport id is namespaced
  under the token that created it. A frame bearing token T may only address or mutate objects created under
  T; a frame whose token differs from (or is absent for) an object it tries to reference draws a
  `tgp;x;code=denied`. This is the confused-deputy fix — a stray `curl | cat` line cannot overwrite your
  `cam`. There is **one namespace per token**, not one global namespace per session.
- **Filtered, tagged emission.** Terminal → app frames (`tgp;e` events, `tgp;x` errors, `tgp;a` acks) are
  emitted on the single shared PTY writeback channel and are **tagged with `tok=`** so the reading app can
  demux them. The terminal does not (and physically cannot, over one PTY) guarantee which process reads
  them; correct demux by the tag is the reader's responsibility. In practice only the foreground app reads
  stdin.
- **Emission gating.** The terminal emits a token's events/errors/acks only while that token has live state
  and the terminal is focused; when the foreground process group changes or the input side of the PTY
  closes, the terminal cancels that token's subscriptions, explore, and playback and stops emitting — so
  stray `tgp;e` bytes never leak into a shell prompt as garbage.

#### Handshake = error-reporting opt-in

A completed `tgp;q`/`tgp;r` handshake **is** the opt-in for structured error reporting; there is **no
separate `errors=` flag in v1**. Once a token has a negotiated version, all structured `tgp;x` errors for
that token's frames are emitted (routed only to the owning token, per the tag above), including the token's
*first* malformed patch. Writers that never handshake — dumb readers, `curl | cat`, log lines — stay silent:
their malformed input is still processed safely (caps enforced, scene protected) but draws no reply, exactly
the silent-drop default for non-participants. (A frame whose `tgp;` header itself fails to parse, from a
token that *has* handshaked, draws a `parse_error` rather than vanishing, so detection/typo failures are
diagnosable.) Event reporting (`tgp;e`) is a **separate** opt-in via per-viewport `sub` and `feat=event`
(§12); it is independent of error arming.

### 7.2 Re-query, idempotency & runtime-variable caps

- **Idempotent and always answered.** `tgp;q` is valid at any point mid-session and is always answered with
  a fresh `tgp;r` reflecting current caps. A repeat with the **same** `v=` is a pure re-answer (no state
  change beyond a possibly-advanced `caps_gen`); a repeat with a **differing** `v=` re-negotiates the
  session version under the same rules as the initial handshake.
- **Not mid-binary.** A `tgp;q` **MUST NOT** be sent while a binary frame for the session is mid-read: the
  binary-read state consumes exactly `len` raw bytes outside escape-scanning, so query bytes that land
  inside an active payload are *by definition* payload and are consumed as such. The terminal does not scan
  for control frames inside a binary payload; not interleaving is the app's responsibility. (Only patch
  `p` and chunked `asset.add` frames ever enter binary-read mode; all control frames — `q`/`r`/`e`/`x`/`a`/
  `vp`/`sub`/`anim` — are always parseable otherwise.)
- **Runtime-variable caps.** Advertised caps can change at runtime (window resize alters viable inline cells
  and viewport pixels; GPU device loss/restore or a headless/SSH transition alters the renderable budget).
  The reply carries a monotonically increasing `caps_gen=` so the app can detect staleness and ordering. On
  a **material** capability transition, the terminal pushes an **unsolicited** `tgp;r` (carrying the full
  current cap set and a bumped `caps_gen`) to every handshaked token; tokenless writers get nothing. The
  `tgp;x;code=cap_exceeded` error remains the hard backstop: an app that pre-trimmed to a now-stale cap
  still gets a structured error rather than a silent drop or crash.

### 7.3 Forward-compat within a version

A negotiated `v=` gates whole feature sets (via the frozen `feat=` vocabulary, §7.1). Within a version,
**unknown ops and unknown fields inside a patch are skipped, not fatal** — version gating handles feature-set
changes, while field-skipping absorbs minor additive evolution. This keeps a newer app's harmless extra
fields from breaking an older terminal, and vice versa.

## 8. The wire protocol

### 8.1 Goals

Compact (binary, not text key=value or JSON), binary-safe through the PTY, diff-friendly, and able to carry large asset blobs inline. Two further goals shape every detail below:

- **Resync against a hostile or buggy sender.** Bytes arrive from anywhere a terminal prints. Framing must stay byte-counted and self-delimiting so that a lie about content (a wrong length, a truncated body, trailing garbage) can produce a *structured error* but can never desync the stream or leak into the text grid (principle 3).
- **Per-app isolation over a single channel.** A PTY exposes one writeback FIFO; the terminal cannot deliver bytes selectively to one of several processes sharing it. TGP therefore isolates apps by *addressing* and *tagged emission*, not by impossible per-process routing. Every frame carries a namespace **token** in its text header; ids are scoped under that token; replies are tagged with it so a reader can demultiplex (§8.4).

### 8.2 Framing

Two frame shapes, chosen per message:

**(a) Control frames** — small, must survive every transport (tmux, ssh, screen). Text APC, human-debuggable:

```
ESC _ tgp ; <type> ; tok=<token> ; <k=v ; …> ESC \
```

Used for: capability query/reply (`q`/`r`), event reports (`e`), error replies (`x`), acks (`a`), viewport ops (`vp`), subscriptions (`sub`), animation control (`anim`), and any op small enough to not need binary. Values that need bytes use base64. Every frame, control or binary, carries `tok=` in this text header (§8.4).

**(b) Binary frames** — bulk scene data (patches with geometry/instance/texture payloads). A **text header announces an exact byte length, then that many raw bytes follow**, read outside escape-scanning:

```
ESC _ tgp ; p ; tok=A ; txn=42 ; enc=bin ; len=20512 ESC \  <20512 raw bytes of CBOR>
```

The header APC is terminated by ST as normal. The parser then reads exactly `len` raw bytes with escape-scanning **disabled**, and returns to Ground after the Nth byte. **No terminator follows the raw payload** — a length prefix is self-delimiting, so a mandatory trailer would be redundant. A single ST token (`ESC \`, `BEL`, or `0x9C`) immediately after the raw bytes is tolerated and consumed as a no-op sync marker (a harmless hedge for senders and for human-pasted frames); a second consecutive ST is treated as ordinary input. This optional-ST tolerance is bounded to one token so a flood of `0x9C` cannot be absorbed.

**Why length-prefixed raw bytes:** true compactness (no base64 33% tax) and zero ambiguity. On seeing a `tgp` binary header the parser switches to "consume exactly `len` bytes" mode, bypassing the C0/ESC scanning that would otherwise corrupt binary data. Raw payload bytes that happen to be `0x1B 0x5C` (ESC `\`) or any other control sequence are captured verbatim; the byte count, not escape scanning, ends the frame.

**Parser back-channel (normative).** The pre-scanner stays dumb and holds no `tgp`/CBOR/`len` knowledge. The APC-end hook returns a typed signal:

```
fn apc_end(&mut self) -> ApcEnd
enum ApcEnd { Done, BinaryFollows { len: u64, enc: BinEnc } }   // BinEnc = Bin only
```

The terminal/Perform layer parses the `tgp; … ; enc=bin; len=N` text header during the APC and returns `BinaryFollows{len,enc}`; on that signal the pre-scanner transitions from Ground into the binary-read state for exactly N bytes, then returns to Ground. A non-`tgp` APC, a `tgp` control frame (`q`/`e`/…), and any `enc=b64` frame all return `Done` — `enc=b64` stays inside the APC and never sets `BinaryFollows` (§8.3). _(grounding: today the APC-end hook returns unit; this typed return is the required signature change to the APC pre-scanner, parser.rs:3–130 / term.rs demux.)_

**Cross-read lifecycle (normative).** `advance()` is non-blocking and per-read, so a byte-counted frame *will* straddle reads. The binary-read state lives in the pre-scanner and persists across `advance()` calls: a `remaining: u64` counter plus a cap-bounded accumulation buffer. Each call appends `min(remaining, available)` bytes and decrements `remaining`; when it reaches 0 the buffer is handed to the decoder (§8.3) and the parser returns to Ground. If the input slice empties first, the state is retained for the next call. On stream close (EOF) with `remaining > 0`, the partial buffer is discarded and, for a handshaked token, an `x;code=parse_error;detail=truncated` is emitted citing the header `txn` captured at frame start. The accumulation buffer never exceeds the cap (§8.7) regardless of how reads are split.

**Header field order (normative).** `enc=` MUST appear before `len=` in the header. A `len=` seen before any `enc=` in the same header is rejected as `x;code=parse_error;detail=field_order` (handled control-frame style — **no** binary-read state is entered). This removes the only path into the wrong read mode: the parser always knows its mode from `enc=` before it acts on `len`.

**Integrity trailer (opt-in).** For `enc=bin` only, an app that negotiated the `bintrailer` capability (§7) may set `trl=1` in the header. An 8-byte trailer then follows the raw payload: the 4-byte magic `TGPB` (`0x54 0x47 0x50 0x42`) plus a 4-byte little-endian echo of `len`. The terminal verifies it; a mismatched or absent trailer → `x;code=parse_error;detail=trailer_corrupt`, scene unchanged, return to Ground. The trailer bytes are **not** counted in `len`. Without `trl=1` no trailer is read. The magic + length-echo detects the two realistic multiplexer corruptions (ST-strip shifting bytes; mid-frame truncation) and surfaces them as structured errors instead of a silent desync. The terminal does **not** detect multiplexers; the app chooses bin-vs-b64 and whether to pay for the trailer (principle 4).

**Robustness fallback.** If a transport can't carry raw bytes (some tmux passthrough configs), the app negotiates `enc=b64` (§7) and sends the payload base64-encoded inside an ordinary ST-terminated APC. The terminal accepts both encodings; the app picks based on the `enc=` ordering in the capability reply (b64 is listed first as the recommended default) and on whether it can confirm a clean PTY.

### 8.3 Payload encoding

The v1 semantic encoding is **CBOR** (compact, self-describing, schema-less, easy to evolve) for the op structure, with large binary sub-blobs (vertex buffers, instance buffers, texture bytes, GLB) carried as CBOR byte strings. The framing (§8.2) is encoding-agnostic, so the chosen semantic encoding can change without a framing break (§16).

`txn`, `enc`, and `len` are **text-header fields, parsed before the CBOR body** — so the frame's identity and correlator are always known even when the body fails to decode (§8.4, G4).

**`len` vs payload (normative).** `len` governs **framing exclusively**. The parser consumes exactly `len` bytes (subject to the cap rules of §8.7) and hands that buffer to the CBOR decoder. The decoded item MUST consume the **entire** buffer. Three outcomes:

1. The buffer decodes to one complete CBOR item that consumes the whole buffer → apply the patch.
2. The CBOR self-terminates before the end of the buffer (trailing bytes) → reject: `x;code=parse_error;detail=trailing_bytes`, scene unchanged.
3. The CBOR is truncated / needs more than `len` bytes → reject: `x;code=parse_error;detail=truncated`, scene unchanged.

In all three cases the parser has already consumed exactly N bytes, so the stream stays synced regardless of any internal mismatch; an under-length payload can never leak into the text grid, because framing consumes N rather than reading "until CBOR ends."

**Two `len` semantics, mode-gated by `enc`.** Only `enc=bin` engages the raw-byte-count binary-read state. For `enc=b64`, `len` denotes the base64 **character** count *inside* the APC; the payload is scanned and ST-terminated as a normal APC (no binary-read state), and `len` is optional (ST delimits) but validated against the actual char count when present (mismatch → `x;code=parse_error`). When `enc=` is absent the default is **b64**. After a b64 APC closes, the payload is base64-decoded (a decode error → `x;code=parse_error;detail=base64`) and then fed to the **same** CBOR decode and error path as `enc=bin`. Both encodings therefore share one decoded-size cap and one CBOR error contract; a payload pre-trimmed to `max_msg_mb` for `bin` is equally valid as `b64`.

**Decode-error surfacing (normative).** The decode site returns a `Result` that MUST NOT be discarded; the terminal routes a decode error to the reply channel, emitting `x;code=parse_error` with the header `txn` (and an `op` index when the failure is op-localized, omitted when whole-frame). Emission is gated on the token having completed the capability handshake (§7) — a dumb reader that never handshakes sees nothing. Replies route only to the owning namespace token (§8.4).

### 8.4 Identity & addressing

- **Node ids: app-assigned strings.** UTF-8, **hard cap 64 bytes** (advertised as `max_id_bytes`). The byte sequence MUST be valid UTF-8 and contain no C0 controls (`0x00`–`0x1F`), no DEL (`0x7F`), no C1 range (`0x80`–`0x9F`), and no ESC (`0x1B`) — nothing that could re-enter the app's stdin as a control sequence when an id is echoed in an event report (§12.5). A violation → `x;code=bad_param;detail=charset|too_long`, op rejected, txn atomically rejected. The control-safe charset means ids ride into event reports verbatim with no escaping, and a defense-in-depth check rejects any forbidden byte before a reply is queued. Ids are human-friendly (`"wheel_fl"`) and double as the handles in event reports and in patch addressing; the terminal treats them opaquely.
- **Asset / material / clip ids: numeric handles** (`u32`). Compact for the hot upload path; assigned by the app.
- **Namespace tokens (per app).** Every frame carries `tok=<≤16 bytes>` in its text header. The token is established at handshake: the app proposes `tok=` in `tgp;q`; the terminal accepts and echoes it in `tgp;r` (or assigns one). The token lives in the text header so it survives a binary-payload cap or abort.
- **Addressing is scoped per token, not one global namespace.** Node / asset / clip / viewport / material ids are namespaced under the creating token. Two apps may both use `"cam"` without collision. A frame bearing token T may address or mutate only objects created under T; a frame with a different or absent token that tries to upsert, remove, or reference T's ids → `x;code=denied;detail=token`. This is the confused-deputy fix: a stray `curl | cat` line cannot overwrite your `cam`. A viewport references a camera **node** and an optional subtree **root** node by id, both resolved within the owning token's scope.
- **Collision rule.** `upsert` on an existing node id mutates it. `add` on an existing **committed** asset id is an error (`x;code=dup_id`); assets are immutable once committed — replace via remove + add, which bumps `asset_revision`. An in-flight (`more=1`) reassembly buffer is *not* a committed asset, so reusing its id across its own chunks is not a collision (§8.7).

### 8.5 Message taxonomy

| Type | Frame | Meaning |
|---|---|---|
| `q` / `r` | control | capability query / reply (§7) |
| `p` | control or binary | **patch**: a transactional list of ops (§8.6) |
| `vp` | control or binary | viewport create/update/destroy (§10) |
| `sub` | control | subscribe/unsubscribe to input events for a viewport/nodes (§12) |
| `anim` | control | animation playback control (opt-in) (§14) |
| `e` | control | **event report** terminal → app (§12.5) |
| `x` | control | **error reply** terminal → app (§15.3) |
| `a` | control | **ack** for a committed correlator (§8.6) |

All types carry `tok=` (§8.4). Binary-capable types (`p`, `vp`) use the binary frame when they carry bulk blobs and the control frame otherwise.

### 8.6 The patch (the core mutation message)

```
# Conceptually (CBOR on the wire); shown as readable pseudo-JSON.
# tok / txn / enc / len ride in the TEXT header; the body below is the CBOR payload.
{ txn: 42, ops: [
    { do: "asset.add",     id: 7, fmt: "glb",  data: <bytes> },
    { do: "material.add",  id: 3, model: "pbr", base: [..], metallic: 0.9, rough: 0.3,
                           tex_base: 8 /* asset id of a texture */ },
    { do: "node.upsert",   id: "car",      trs: {...} },
    { do: "node.upsert",   id: "wheel_fl", parent: "car", mesh: 7, material: 3 },
    { do: "node.instances",id: "atoms",    mesh: 5, count: 1200,
                           xforms: <bytes>, tints: <bytes> },
    { do: "node.instance_set", id: "atoms", index: 6, tint: [255,0,0,255] },
    { do: "node.visible",  id: "ghost",    visible: false },
    { do: "node.remove",   id: "old" },
] }
```

**Semantics:**

- **Atomic / transactional.** All ops in a patch apply against a staged copy and commit together or not at all. Success bumps `scene_revision` **once** (no mid-frame tearing — the "transactional frame" for free) and `asset_revision` once if assets/materials changed. Any op error rejects the **whole** txn and emits an `x` citing the offending op index (§15.3); the scene is unchanged.
- **Ordering & acks.** Patches apply in receive order. `txn` is an app-chosen correlation id echoed in acks and errors. An `a` (ack) confirms a committed `txn` and is available to any handshaked token (not gated on a subscription); it is used for txn-commit confirmation and for the `enc=bin` transport probe (§7).
- **Reference resolution (order-independent within a txn).** All references — `parent`, `mesh`/`asset`, `material`, `tex` — resolve against the txn's **final staged state**, so ops need not be topologically sorted: a child op may precede its parent op, and a `node.upsert{mesh:7}` may precede the `asset.add{id:7}` in the same txn. Asset/material refs require a fully **committed** (`more=0`) target; an in-flight chunk buffer does **not** satisfy a ref. Any unresolved reference rejects the whole txn: `x;code=bad_ref;detail=<kind>:<id>`, citing the first offending op index.
- **Idempotent addressing (sparse merge).** `node.upsert` creates or mutates over the CBOR op map with a three-way per-field rule decided by the CBOR value: **key absent = preserve**; **key present and CBOR null = clear to default**; **key present with a value = set**. Clear defaults: `parent` → root (the node joins the roots set, retaining its children and animation phase — the only non-destructive detach), `alt` → none, `tint` → opaque white `[255,255,255,255]`. `trs`, when present, is replaced wholesale (its sub-fields are not individually nullable). A CBOR null on a non-nullable field (`id`, or the kind selector when present) → `x;code=null_not_allowed;detail=<field>`.
- **Kind is immutable.** A node's kind (`Group | Mesh | Instanced | Light | Camera`) is fixed at creation. On first upsert, zero kind selectors ⇒ `Group`; exactly one of `{mesh, instances, camera, light}` ⇒ that kind; two or more in one op → `x;code=kind_conflict;detail=multiple_selectors`. A later upsert whose selector implies a *different* kind than the existing node → `x;code=kind_conflict` (change kind via remove + re-add). A matching selector mutates kind-specific data in place (e.g. `mesh:9` on an existing `Mesh` swaps its asset/material).
- **Acyclic, depth-bounded.** At commit the terminal walks every node's parent chain (iterative, visited-set). A self-parent or any cycle → `x;code=cycle;detail=<id>`; a chain exceeding `max_node_depth` (256, advertised) → `x;code=depth_exceeded`. The whole txn is rejected; the committed scene is always acyclic, so the render-time world-matrix walk needs no per-frame cycle bookkeeping (it carries the same depth cap only as defense-in-depth).
- **`node.remove` cascade.** `node.remove` deletes the node **and its whole descendant subtree** by default, also erasing the removed ids from the roots set, the dirty set, and any pick/event subscription targeting them. The optional `reparent:"root"` flag instead detaches the node's direct children to the scene root (their local TRS retained; world transform not baked in v1) and removes only the named node. Removing a node referenced by a viewport's `camera`/`root` is permitted: that viewport silently falls back to its default (§10), with an optional diagnostic `x`/event to the owning token. Removing a missing id is a no-op (no error, no revision bump).
- **Instanced buffers (canonical layout).** A `node.instances` op carries `count:u32` and two CBOR byte strings: `xforms` = `count × 64` bytes (each record 16 little-endian IEEE-754 f32, column-major mat4) and the optional `tints` = `count × 4` bytes (RGBA8, sRGB; absent ⇒ all opaque white). Lengths must match `count` exactly or `x;code=bad_layout;detail=xforms_len|tints_len|count_mismatch`; `count` over cap → `x;code=cap_exceeded;detail=max_instances`; NaN/inf in any matrix element → `x;code=parse_error;detail=instance_nan` (reject, do not clamp). An `Instanced` node carries per-instance tints and **must not** set a node-level tint (→ `x;code=kind_conflict;detail=instanced_node_tint`). Per-instance material handles are deferred (§16). `instance_index` for picking (§12.4) is the record index `0..count`.
- **Single-instance re-tint.** `node.instance_set{id, index, tint}` writes **one** tints record and sets the node's instance-dirty flag without re-sending `xforms` or the whole buffer — the cheap path for the click-to-highlight flow (§12.6). `index >= count` → `x;code=bad_index`.
- **Revision accounting.** `node.instances` and `node.instance_set` bump `scene_revision` and set a dedicated per-node **instance-dirty** flag, and **never** bump `asset_revision` — so a per-frame point cloud re-uploads only that node's instance buffer, never all meshes. Transform/visibility/tint changes bump `scene_revision` + the node's transform-dirty flag. Asset/material commits bump `asset_revision`. The renderer gates on these per-node and per-viewport dirty flags, cleared after consumption — not on numeric revision equality. Revisions are `u64`, incremented with saturating add (wrap unreachable in practice).
- **Bulk fields are byte blobs.** `xforms`/`tints`/vertex data ride as CBOR byte strings in the same frame.

### 8.7 Chunking

A single asset larger than one frame is uploaded across multiple binary frames keyed by `id`, with a `more=1|0` flag, generalizing the older `more=1|0` reassembly model _(grounding: handler.rs:26–31, 189–203)_.

**Keying & isolation.** Reassembly is keyed by `(namespace token, asset id)` — token-scoped so two apps cannot collide or corrupt each other's buffers (§8.4), and a tokenless or foreign writer can neither address nor abort another token's pending upload.

**Interleaving (reject-new-keep-prior).** Within one token, independent asset ids may interleave their chunk streams freely. A single id may **not** interleave: a second open `more=1` stream for an id that already has an in-flight buffer, or an `fmt` change mid-stream, is rejected — `x;code=chunk_conflict` (`detail=chunk_interleave | chunk_fmt_mismatch`) — and the **prior in-flight buffer is retained**; the offending new stream is dropped. (Retaining the prior buffer stops an interleaved hostile stream from wiping a legitimate upload.) A chunk targeting an id that is already **committed** → `x;code=dup_id;detail=<id>` (a committed asset cannot be re-opened; replace via remove + add).

**In-flight ≠ committed.** An in-flight reassembly buffer is not a registered asset: the duplicate-id immutability check and `mesh:`/`asset:` ref resolution see only **committed** assets, so the first chunk does not self-error and a ref to an id still mid-upload yields `bad_ref` (§8.6) until commit.

**Atomicity.** Chunk frames are **pure pre-parse buffering** — a `more=1` frame bumps nothing, applies nothing, and carries no txn effect. Only the final `more=0` frame completes reassembly; the assembled buffer is then CBOR-decoded as one patch and applied **atomically**, bumping `asset_revision` once (and `scene_revision` once if the patch also mutates nodes). Any failure on any chunk (cap overflow, conflict, fmt mismatch, or final-frame decode error) discards the whole buffer and leaves the scene unchanged — no partial asset is ever installed. The txn correlator lives on the final frame's header and is cited in the resulting ack or `x`.

**Caps.** Per-frame size is bounded by `max_msg_mb` and the accumulating reassembly total by `max_reasm_mb` (≥ `max_msg_mb`), both advertised (§7). On overflow the in-flight buffer is dropped → `x;code=cap_exceeded;detail=max_msg|max_reasm`. For an over-cap `enc=bin` header (`len` > cap) the terminal still **consumes and discards exactly `len` bytes** — without buffering them — to keep the stream synced, then emits `x;code=cap_exceeded`. A second absolute hard ceiling (`BIN_HARD_CEIL`, an internal compile-time constant ≥ `max_reasm_mb`) caps even the discard loop: `len > BIN_HARD_CEIL` → `x;code=cap_exceeded;detail=unrecoverable`, and the terminal stops consuming at the header boundary (returns to Ground without draining) rather than chasing a fabricated multi-gigabyte count. The accumulation buffer never exceeds the cap regardless of how reads are split. An in-flight buffer left open (no `more=0`) is reclaimable under a reassembly cap/timeout and never partially registers.

## 9. (reserved)

## 10. Viewports & compositing

A **viewport** is a window into the scene, anchored to terminal cells. Many can exist at once, each owned by the token that created it (§8.4); a viewport's ids, camera, and root all resolve within that token's namespace.

### 10.1 Viewport object

```rust
struct Viewport {
    id:        ViewportId,
    anchor:    Anchor,           // Inline | Pinned  (see §10.2)
    cells:     CellRect,         // position + size in cells (col,row,cols,rows); u16 fields
    camera:    Option<NodeId>,   // None ⇒ implicit auto-framing default camera (§10.5)
    root:      Option<NodeId>,   // subtree to render; None ⇒ whole scene
    z:         i32,              // order among overlapping viewports
    clear:     Option<Rgba>,     // Some ⇒ occlude cells; None ⇒ transparent over text (§10.4)
    render:    RenderOpts,       // msaa, tone-map operator, theme-tint (see §11)
    explore:   Option<ExploreOpts>,   // opt-in camera controller (§12.3)
    screen_affinity: ScreenAffinity,  // which screen buffer this viewport lives on (§10.6)
    clip_to_scroll_region: bool,      // clip to the DECSTBM region + terminal bounds (default true, §10.4)
}

enum Anchor {
    // {line,col} is an INITIAL placement hint only; after creation the placeholder
    // CELLS are authoritative for position. The hint is not re-read for scroll or reflow.
    Inline { line: ScrollbackLine, col: u16 },
    Pinned { col: u16, row: u16 },   // fixed screen region; text scrolls under/beside
}

enum ScreenAffinity { Primary, Alt, Both }  // default = buffer active at create time
```

### 10.2 Inline vs pinned (app chooses per viewport)

- **Inline** — occupies real cells in the text grid, like an image embedded in a notebook/chat log. It scrolls with the surrounding text and scrolls off-screen (state retained, not rendered while off-screen). It is realized via a **Unicode-placeholder-style cell binding** (kitty-style Unicode placeholders) so the region flows naturally through line-based apps (vim, less, tmux) that only understand cells.
  - **Placeholder cells are authoritative.** The `Inline { line, col }` fields seed only the *initial* placement; once the viewport exists the terminal derives its position, size, occupancy, and lifetime entirely from where its placeholder cells currently land in the grid. If a TUI repaints and moves the placeholder glyphs, the viewport follows the cells; the stored `{line, col}` is never reconciled back or re-read.
  - **Cell caps and over-cap rejection.** The capability reply (§7) advertises `max_inline_cols` and `max_inline_rows`. A `tgp;vp;anchor=inline` whose `CellRect` exceeds either cap is **rejected** — it is not created or partially created — and the terminal replies `tgp;x;code=cap_exceeded;detail=max_inline_cols` (or `max_inline_rows`) to the owning token. The terminal never silently clamps, truncates, or partially binds an inline region. `CellRect` stays `u16` in the wire and model; the cap is a separately advertised limit, not a type change.
  - **Occupancy follows `clear`.** With `clear=Some(rgba)` the viewport **occludes** its placeholder cells (any underlying glyphs are not shown while it is live). With `clear=None` it **composites over** whatever cell content occupies those cells, using premultiplied-alpha over-blend, so un-drawn pixels reveal the underlying text exactly (§10.4).
  - **Detach on overwrite.** Writing a new, non-placeholder glyph into a cell that currently holds a viewport placeholder reverts that cell to ordinary text and removes it from the viewport's live placeholder set — the app's text wins. Re-emitting the identical placeholder glyph is not an overwrite and does not detach. If detaching drops the viewport below its required cell count, the binding is broken and the viewport is auto-destroyed (see lifetime below, `reason=detached`).
  - **Lifetime is bounded by scrollback depth.** When every placeholder cell of an inline viewport has been evicted from the bounded scrollback ring, the terminal destroys the viewport: it frees that viewport's offscreen color+depth and pick targets and removes it from the scene. Shared scene nodes, assets, and the referenced camera are never touched (a viewport owns only its compositing resources). Destruction keys on placeholder-cell liveness, never on the stored `{line, col}` hint. If the owning token has an active lifecycle subscription on the viewport, the terminal emits exactly one `tgp;e;vp=<id>;ev=destroyed;reason=evicted` (routed and tagged to that token only); otherwise eviction is silent. No `tgp;x` is emitted — eviction is normal teardown, not an error. Detach-driven and erase-driven destruction (§10.7) use the same path with `reason=detached`.

- **Pinned** — fixed to a screen region; text scrolls underneath/around it. For dashboards, monitors, HUDs, inspectors. Pinned viewports are not cell-bound: they persist across scroll, erase (§10.7), and (with `screen_affinity = Both`) across alt-screen switches (§10.6). Their size is bounded by the pixel caps of §10.3, not by the inline cell caps.

### 10.3 Cell → pixel mapping & reflow

- Cell rects map to pixel rects using the current cell metrics. The mapping is computed CPU-side and is queryable independent of the GPU.
- **Allocation is always clamped to the on-screen intersection** of a viewport's pixel rect with the terminal surface — off-screen area is never allocated. A zero-size cell rect (`cols=0` or `rows=0`) creates a legal but **not-rendered** viewport (no offscreen target) until it later gains size via a `vp` update or reflow. A fully off-screen rect allocates nothing and is not composited; its state is retained. Because allocation is bounded by the screen, an arbitrarily large cell rect can never demand an arbitrarily large allocation: it allocates at most the screen-sized intersection.
- **Per-viewport pixel cap.** The terminal advertises `max_viewport_px_w` / `max_viewport_px_h`. A viewport whose on-screen clamped pixel size still exceeds the cap is **rejected** with `tgp;x;code=cap_exceeded;detail=max_viewport_px` (to the owning token) and is not created or updated. Together with the on-screen clamp this neutralizes the oversized-viewport VRAM exhaustion vector (§15.1).
- **Terminal-driven reflow (deliberate choice).** On `SIGWINCH`, font-size change, or resize-driven rewrap that moves or splits the anchor line, the terminal recomputes each inline viewport's pixel rect from the **current landed positions of its placeholder cells** — the cells are authoritative; the stored `{line, col}` is only the initial hint and is not re-read. The app does **not** re-issue placements: viewports reflow like text. If the app wants to react (e.g. swap LOD), it can subscribe to a `resize` event (§12), but it is not required to.
  - If rewrap splits a viewport's placeholder cells across a wrap boundary, the offscreen layer is rendered once at the viewport's logical (`cols × rows`) size and composited into the smallest axis-aligned grid rect that bounds all live placeholder cells, clipped (not re-rendered) to the on-screen intersection.
  - **Scroll-region scrolling.** A DECSTBM-bounded scroll moves an inline viewport's placeholder cells exactly as it moves text. The terminal recomputes the viewport's cell rect from the new cell positions and sets only the per-viewport dirty flag, so the cached layer is re-composited at the new rect without a re-render (§10.4). When placeholder cells straddle a scroll-region margin, only the cells inside the region scroll, which can split the placeholder run; `clip_to_scroll_region` (default true) clips the cached layer to the active region so nothing bleeds across the margin. Inline viewports are permitted inside DECSTBM regions and are never silently downgraded to pinned.
  - Reflow and scroll set the per-viewport dirty flag (a re-composite, not a re-render, unless content/camera/size also changed — §6.5).

### 10.4 Compositing — "stickers with depth"

Each viewport renders to its **own offscreen target carrying color + depth** ("a sticker that knows how far back each dot is"), then composites into the main framebuffer with a **depth test against the shared text/scene depth**.

- **Text ↔ 3D interleaving is preserved.** The text plane sits at a known constant depth. A solid object can be partly in front of text (near side covers it) and partly behind (far side hidden) — **exact for opaque geometry**.
- **Transparent fill.** Offscreen color targets use **premultiplied alpha**, and the offscreen depth buffer clears to FAR. Un-drawn pixels have alpha 0. For `clear=None` the composite is a straight "over" blend of the premultiplied viewport color onto the underlying cell/text content, with the depth test gating **only drawn fragments**: a drawn opaque fragment depth-tests against the text plane so geometry in front of text covers it and geometry behind text is hidden, while an alpha-0 pixel contributes nothing and the underlying text shows through exactly (so a transparent inline viewport never punches a hole over the text it overlays). For `clear=Some(rgba)` the color target is cleared to that opaque color, so the viewport occludes its cells (§10.2).
- **Smooth scroll/resize.** Because each viewport is its own layer and the text plane sits at a constant depth, a **pure screen translation** of a viewport (scroll or reflow reposition) preserves the depth comparison between the cached viewport depth and the text plane, so the cached color+depth layer is **re-composited at the new offset rather than re-rendered**. Re-render is forced by any content/camera/size change, which alters viewport-local depth relative to the text plane (driven by the dirty flags, §6.5). This translation-only invariant — exact only while text stays at a constant depth — is what makes cache-recomposite correct.
- **Transparency caveat (honest).** A semi-transparent fragment (glass, smoke, anti-aliased edges) overlapping text must pick a single depth for the test, so translucent-over-text is **approximate**, not pixel-perfect. Opaque geometry is exact. This is the standard trade every renderer makes.
- **Z-order.** Overlapping viewports composite in a total, deterministic order: by `z` ascending (higher `z` on top), with ties broken deterministically by viewport identity — across tokens the tiebreak is `(z, owning-token order, ViewportId)`, keeping the order total and stable. Inline and pinned anchors share **one** z-space: pinned does not implicitly sit above inline; apps control all stacking via `z`. The compositor sorts every frame; order never depends on map iteration.
- **Clipping.** `clip_to_scroll_region = true` clips a viewport's composite to the intersection of the current DECSTBM vertical scroll region and the terminal's own bounds; `= false` clips only to the terminal bounds. In all cases allocation and composite are clamped to the on-screen surface (§10.3). This is honest, terminal-local clipping: a multiplexer pane is invisible to the inner terminal (it sees only one PTY and its DECSTBM region), so `clip_to_scroll_region` cannot clip to a tmux/screen pane boundary. Cross-multiplexer-pane clipping requires multiplexer cooperation and is out of scope (§16).

### 10.5 One scene, many cameras (decision)

TGP uses **one shared scene** rendered by **N viewports**, each via its own `Camera` node — not N independent scenes. This shares asset/material uploads (cheap) and matches "register once, view many ways." A viewport may set `root` to a subtree for isolation when desired; an absent or unresolved `root` defaults to the whole scene.

- **Camera is optional, with an implicit default.** `Viewport.camera` may be omitted. When it is `None`, or names a node that does not (yet) resolve to a `Camera` node — because it has not been created, or was removed — the viewport renders with an **implicit auto-framing default camera** that fits the root subtree's bounding sphere at a default field of view. This mirrors the implicit default material (`MaterialId 0`) and default lighting, so the simplest scene (one mesh, one viewport, no `Camera` node) renders zero-config. A missing camera is **never** a viewport or transaction error: a viewport created before its camera node (a normal ordering) simply renders with the default until the named camera appears, at which point it silently adopts the real camera. `node.remove` of a node used as a viewport camera or root succeeds and silently falls that viewport back to the default. Diagnostics, when wanted, are ordinary `tgp;x` replies to the owning token (e.g. `code=bad_ref` on entering the missing state); there is no separate viewport diagnostic channel, and rendering never stops.
- **Explore needs a concrete camera.** The explore controller (§12.3) mutates a real `Camera` node's transform, so it needs persistent state to write. If `explore` is enabled on a viewport with no resolvable camera, the terminal materializes a **terminal-owned** `Camera` node bound to that viewport (addressable only by the owning token, for sync-back) to hold orbit/zoom/pan state. Without explore, the default camera stays per-frame scratch.

### 10.6 Screen-buffer affinity

Every viewport carries a `screen_affinity` defaulting to the buffer active at create time. On alt-screen enter (DECSET 1049/1047/47) the terminal hides every viewport whose affinity excludes the now-active buffer and renders those whose affinity includes it; on exit the prior set is restored exactly. Hiding retains all viewport and scene state — there is no GPU teardown; only render gating changes, so an inline viewport hidden by an alt-screen switch is not scrollback-evicted while hidden. Inline viewports are implicitly bound to their create-time buffer (Primary or Alt). A pinned viewport may set `Both` to persist a HUD overlay across a full-screen TUI. Affinity is a property of the token-owned viewport, so an alt-screen switch only gates the owning app's viewports; scene nodes and assets are buffer-independent and are never affected by a switch.

### 10.7 Erase & reset semantics

ED (CSI J) and EL (CSI K) erase underlying cells exactly like text. Because placeholder cells are authoritative, erasing all of an inline viewport's placeholder cells destroys it and frees its offscreen layer (the same teardown path and opt-in `tgp;e;ev=destroyed` as scrollback eviction, §10.2); a partial erase that leaves some placeholder cells leaves the viewport alive with a cell rect recomputed over the surviving cells. In particular, `CSI 2J` erases the on-screen placeholder cells of inline viewports and therefore destroys them. Pinned viewports are not cell-bound and **persist** across all ED/EL (including `2J`/`3J`/`2K`) — a HUD correctly composites over freshly-cleared cells. `CSI 2J`/`3J` also clear images (existing terminal behavior) but never free TGP scene nodes or assets and never remove pinned viewports; apps must clear scene state deliberately.

A full hardware reset (RIS, `ESC c`) is the guaranteed VRAM-recovery path: it tears down the entire TGP scene — assets, materials, nodes, viewports of every token — frees all GPU buffers, and invalidates all session tokens (see §15). A soft reset (DECSTR) leaves the TGP scene untouched.

## 11. Rendering pipeline

### 11.1 Per-viewport flow

Every viewport renders its subtree into **one** linear-light offscreen color target (all nodes, default-material and registered-material alike), runs a fixed ordered set of stages, then composites into the framebuffer. There is no per-pixel material-class branch and no split into separate default/PBR targets.

```
for each visible viewport (dirty or first-draw):
    recompute dirty world matrices for its root subtree
    render subtree → offscreen {color, depth} target (linear-light)
        - one forward lighting loop shades default-material and PBR nodes alike
        - instanced draw for Instanced nodes (one draw, 0..N) using the node's single material/pipeline
    (if pick subscribed) render subtree → pick target  (pick pass: always 1x, never tone-mapped, never resolved)  [§12.4]
    apply the per-viewport stages, in this exact order:
        theme-tint (default-material diffuse input, pre-lighting, default-material nodes only)
          → lighting (linear)
          → tone-map (render.tone_map, default none; whole viewport, all nodes, in linear)
          → sRGB-encode into the offscreen color target
          → MSAA resolve (color target only)
then:
    composite all viewport layers into the framebuffer by z, depth-tested vs the text plane
```

The ordered stage list is canonical and total. Theme-tint is a material-**input** stage applied per-node before lighting and **only** to default-material nodes (never to registered materials); it is therefore inside the linear render and naturally precedes tone-map, with no post-resolve color grade. Tone-map runs once at viewport scope over the combined linear result. The pick pass is outside this chain entirely: it is a dedicated single-sample, non-tone-mapped, non-resolved pass (§11.4, §12.4).

### 11.2 Materials

- **Default material (implicit `MaterialId 0`).** A clean **matte lit** look defined as a **linear-light** shader: Lambertian diffuse + constant ambient + a soft hemispherical term, with `base × per-node tint × brightness` as the surface input. Wire colors arriving sRGB-encoded are linearized on input (see "Color space" below); the material is lit and written entirely in linear. Zero config, legible when tiny. Its constants are pinned:

  ```
  key light dir (view space)   = normalize((-0.3, -1.0, -0.4))
  key color                    = linear(1.0, 1.0, 1.0)
  key intensity                = 1.0
  ambient (no registered lights, see §11.3) = linear(0.12, 0.12, 0.12)
  hemisphere sky               = linear(0.30, 0.34, 0.40)
  hemisphere ground            = linear(0.16, 0.14, 0.12)
  hemisphere weight            = 0.25  (along view-up)
  brightness                   = scalar gain, clamped to [0.0, 8.0]
  ```

  with

  ```
  final_linear = clamp(base_linear × tint_linear × brightness, 0, large)
                 × (ambient + hemi + key × max(N·L, 0))
  ```

  The hemispherical term is **always** part of the default material — there is no gated or no-hemisphere mode. `brightness` is a continuous gain, not a resource: values outside `[0, 8]` are clamped silently (never an error).

- **Registered materials (opt-in).** `material.add` with `model: "pbr"` → a metal-roughness core: baseColor, metallic, roughness, normal, emissive, occlusion; optional texture maps. A separate PBR pipeline; nodes opt in by referencing the material id. PBR is linear-light and shares the same forward lighting loop as the default material.

- **Textures (v1).** Two sources only: PNG byte assets (`asset.add fmt=png`) and PNG images embedded in GLB, both decoded by **one** hardened PNG decoder. Embedded JPEG, standalone JPEG, raw RGBA, and KTX2/Basis are deferred. A texture exceeding `max_tex_dim` on either axis (checked from the header before full decode) or any non-PNG byte asset is rejected with `tgp;x;code=parse_error` (`detail=tex_dim` or `detail=tex_format`), rejecting the whole txn atomically (§8.6). Decode is bounded by the decode/parse-time cap (§15.2) to guard against decompression bombs.

- **Theme-tint (optional `render.theme_tint`, default false).** Multiplies the terminal's active **default-foreground** color into the default material's **diffuse base** only, as a pre-lighting material-input stage (§11.1 ordering). It applies **only** to default-material (`MaterialId 0`) nodes whose per-node tint is the neutral default (white, `1,1,1,1`) — a node with an explicit non-white tint is left untouched, so a deliberately red sphere is never recolored by a blue theme. It never touches PBR nodes or the baseColor of registered materials. The palette source is the active default foreground color, read once per frame and linearized; it is not one of the 16 ANSI indices and not the background. The multiply is bounded:

  ```
  effective_diffuse_linear = base_linear × lerp(white, fg_linear, strength)
  ```

  with `strength` fixed at `1.0` in v1 (the flag is boolean). Off by default, so 3D never silently adopts the theme unless the app asks.

- **Color space.** All wire colors — per-node tint, per-instance tint, default-material base, PBR baseColor, the theme-tint foreground source, and the viewport clear color — are sRGB-encoded RGBA on the wire. PBR scalar parameters (metallic, roughness, occlusion strength, emissive intensity) and all TRS/geometry values are linear/numeric and are **not** color-managed. The terminal linearizes every color field once on input; the entire viewport then renders in linear; the per-viewport tone-map runs in linear; and the result is **explicitly sRGB-encoded in-shader** into the offscreen color target. The offscreen target is a linear-data UNORM format with an explicit sRGB-encode stage (not an `*_SRGB` auto-encoding format), so tone-map math stays well-defined in linear and the depth-tested composite sees sRGB-encoded color exactly like the text plane. `color=srgb` in the capability reply (§7.1) means precisely this; there is no linear-color negotiation in v1.

### 11.3 Lighting

- **Implicit default lighting.** With zero registered lights, the default material is lit by the built-in key + ambient + hemisphere (§11.2 constants) so objects look good out of the box.
- **Default lighting is replaced, not augmented, per viewport.** The built-in key+ambient+hemisphere is an implicit default that is fully disabled **per viewport** the moment that viewport's visible subtree contains at least one `Light` node. From then on the default material is lit **only** by registered lights — default-material nodes respond to registered lights through the **same forward lighting loop** as PBR. The switch is evaluated per frame from the visible-subtree light set, so two viewports sharing one scene can differ: a viewport whose `root` subtree excludes the light keeps its built-in key on. An opt-in viewport flag `render.keep_default_light` (default false) keeps the built-in key+ambient+hemisphere on **additively** alongside registered lights, for apps that want a guaranteed fill.
- **Registered lights as nodes (opt-in).** `Light` nodes (KHR_lights_punctual style: directional / point / spot, with color + intensity). Because lights are nodes they inherit transforms and animate (a headlight parented to a car). Forward rendering.
- **Per-viewport light cap.** `max_lights_per_viewport` is advertised in the capability reply (§7.1). The effective light set per viewport per frame is computed from the visible subtree. If it exceeds `max_lights_per_viewport`, the renderer **deterministically** selects the `max_lights_per_viewport` highest-intensity lights (by the `light.intensity` scalar; ties broken by ascending node-id lexicographic order) and ignores the rest for that frame — never crashing, never producing nondeterministic shading. Going over cap also queues a **one-time-per-viewport** `tgp;x;code=cap_exceeded;detail=max_lights_per_viewport`, routed only to the owning token and only if that app handshaked (§7, §15.3). The over-cap patch is **not** rejected: lights are dynamic and animated, and rejecting a transform patch because an animated light drifted into view would violate the app-authoritative principle. The latch clears (and may re-fire once) when the visible light count drops back under cap.
- **Deferred:** shadows, IBL/environment maps, area lights.

### 11.4 Anti-aliasing, tone-map, and the pick target

Tiny per-cell viewports alias badly, so per-viewport MSAA (resolved before composite) matters more here than in full-screen rendering.

- **MSAA.** `render.msaa` is per-viewport; default **4x**. The GPU-supported sample counts are advertised as `msaa=` in the capability reply (`msaa=1,4` baseline; `2,8` only where the adapter reports support). A requested count not in the supported set is silently clamped **down** to the nearest supported count `≤` the request (never up, never an error — MSAA is a quality knob, not a resource the app must size). The effective clamped count is reported back via the viewport ack (`a;vp=<id>;msaa=<n>`) when the app subscribed to acks.
- **Tone-map.** `render.tone_map` (none / Reinhard / ACES, default **none**) is applied **uniformly to the whole linear offscreen color target**, after lighting and before sRGB-encode, regardless of the mix of default-material and PBR nodes in the viewport. It is a single viewport-scoped stage, never per-node. With the default operator `none` (a clamp-only passthrough), a viewport's default-material colors are unchanged; choosing Reinhard/ACES tone-maps all nodes identically. Mixing PBR into a viewport therefore never silently alters default-material color unless the app explicitly selects an operator.
- **Pick target.** The pick target is **always single-sample (1x)**, is **never** MSAA-resolved, and is **never** tone-mapped, regardless of the viewport's `render.msaa` or `render.tone_map`. Pick rendering uses a dedicated 1x pass with nearest/point semantics; node-id + instance-index colors are written and read back unmodified. The whole-viewport tone-map and the MSAA resolve apply only to the color target. Averaging id colors would yield nonexistent ids and mis-route events, so this single-sample invariant is mandatory; the CPU-resolvable pick result (§12.4) is authoritative and the GPU color-ID pass, when present, must agree with it.
- **VRAM accounting.** MSAA color + depth memory counts against `max_vram_mb_session`. A viewport's VRAM charge is `pixels × samples × (color_bytes + depth_bytes)` for the multisampled target, **plus** the 1x resolve target, **plus** the always-1x pick target (when picking is active). Both the MSAA multiplier and `max_lights_per_viewport` storage are therefore reflected in the advertised cap, keeping it honest (§15.2).

## 12. Interactivity (the headline)

Off by default (principle 1: the app drives). The app opts into two independent capabilities per
viewport: **explore** (terminal-side camera control) and **events** (clicks/hover routed to the app),
plus a third low-level escape hatch, **raw** pointer forwarding. Every interactive frame and every report
carries the app's namespace token `tok=` (§8.4); the terminal hard-isolates addressing by token and tags
each emitted report so a reader can demux the single shared back-channel, but it makes no attempt to
deliver bytes selectively to one of several processes sharing the PTY — physical delivery isolation is
impossible over one PTY (§8.4, §12.5).

### 12.1 Model

```
                 ┌─────────── app subscribes (sub) per viewport ───────────┐
 user input ──► interaction router                                          │
   (mouse)        │  if explore enabled  → terminal updates camera node     │  (opt-in)
                  │                         (takes control of camera only)   │
                  │  if events subscribed→ pick → emit `e` report to app ────┘
                  │  if raw mode         → forward raw pointer events to app
```

Input never arrives through the scene-mutation path (`Parser::advance`): pointer and focus events enter
through the terminal's input API, are routed here, and are resolved entirely on the CPU (§12.4) so the
whole interaction surface is testable without a GPU — inject synthetic pointer/focus events, then assert
the camera node's transform, the queued `tgp;e`/`tgp;x`/`tgp;a` report bytes, and the hover/subscription
state.

#### 12.1.1 Input hit-testing across viewports

When viewports overlap, the same `z` that orders compositing (§10.4) orders input. Hit-testing walks
candidate viewports **top-down by `z`** (highest first), considering **only** viewports that have an
active `sub` (events or raw) and whose **live placeholder-cell rect** (the authoritative rect, §10.2;
the stored `{line,col}` is only a hint) contains the cursor. For each candidate, the router resolves the
viewport's pick buffer at the cursor:

- If the pick reports a node hit, that viewport **owns** the event and the walk stops.
- If the top candidate's pick pixel is **empty** (no node) **and** its `clear` is transparent (`None`),
  the walk **falls through** to the next lower subscribed viewport — so a click on an object visible
  *through* a transparent layer lands on that object, not on the empty layer above it.
- If a candidate's pick pixel is empty but its `clear` is **opaque**, it owns the event anyway (it
  visually occludes that space, so it consumes the empty region rather than passing the click down).

Non-subscribed viewports never participate in input hit-testing (they still composite visually). The
owning viewport's id is reported in the `vp=` field, and the report is tagged with that viewport's owning
token.

### 12.2 Opt-in, app-in-control (recap of decision)

By default the app issues all transforms and receives no input. The app explicitly turns on the pieces it
wants; only then does the terminal take that piece of control. Three capabilities, requested per viewport:

1. **Explore** — terminal-driven orbit/zoom/pan (§12.3).
2. **Events** — terminal reports semantic hits (`click`/`hover`/`enter`/`leave`/…) on subscribed nodes
   (§12.5).
3. **Raw** — the terminal forwards the raw pointer stream for the viewport and the app interprets it
   itself. Raw is *pointer-forwarding only*: reflow (§10.3) and offscreen compositing (§10.4) remain
   terminal-owned exactly as for non-raw viewports, and the app drives the camera itself via
   `node.upsert` on the camera node. Raw never suspends layout or compositing.

#### 12.2.1 Gesture arbitration

The three capabilities resolve by a fixed precedence with mutual exclusion, so every gesture has exactly
one owner:

- **Raw is mutually exclusive with explore and events.** A `sub` that requests `raw` together with
  `explore` or `events` is rejected with `tgp;x;code=invalid_sub` **before any state change** (no
  subscription is stored). Because raw and explore are mutually exclusive, explore is implicitly disabled
  on a raw viewport.
- **When raw is active**, it consumes the entire pointer stream within the viewport's cell rect and
  suppresses both explore and events there.
- **When explore and events coexist**, gestures are partitioned deterministically:
  - A **press → release with no intervening motion** (within a small dead-zone) is a **click**
    (and `down`/`up` if subscribed).
  - **Any motion** between press and release is an **explore gesture** (orbit/pan per `ExploreOpts`);
    a motion-bearing gesture does **not** also emit a click.
  - `hover`/`enter`/`leave` **always** fire, regardless of explore.
  - Drag and wheel are consumed by explore when it owns that axis (§12.3, §12.5.5); a drag is forwarded
    as a `drag` event only when explore does not own it (orbit/pan disabled for that drag).
  - The dead-zone is advertised as the `click_deadzone_px` capability (default 3).

### 12.3 Explore controller (opt-in)

```rust
struct ExploreOpts {
    orbit:   bool,         // drag to rotate
    zoom:    bool,         // scroll/pinch to zoom
    pan:     bool,         // drag to pan
    lock_axes: AxisMask,   // e.g. lock to Y for turntable-only
    zoom_min: f32, zoom_max: f32,
    auto_spin: Option<{ axis: Vec3, speed: f32 }>,
    damping: f32,
    initial: Option<CameraPose>,
}
```

- **Explore moves the _camera node_, not the model.** The terminal mutates the viewport's `Camera` node
  transform, never the model's nodes. The app's model transforms stay app-owned (principle 1). This is why
  "the terminal handles tumbling" doesn't violate "the app issues transforms": they touch *different*
  nodes.
- **A concrete camera is required.** Explore mutates a real `Camera` node, which is where orbit/zoom state
  persists. If `explore` is enabled on a viewport with no resolvable camera, the terminal materializes a
  terminal-owned camera node (auto-framing initial pose; addressable only by the owning token for
  sync-back) so the gesture has stable state to write.
- **One controller per camera.** A viewport's camera is driven by at most one terminal-side controller.
  Enabling `explore` on a viewport blocks terminal-side animation playback (§14) of that camera node — a
  conflicting `anim;play` on it is rejected with `tgp;x;code=camera_busy`, and vice-versa.
- **Damping and the clock.** Damping, auto-spin, and report throttling advance on a single injectable
  monotonic clock; tests step it explicitly. Damping decays angular velocity to rest deterministically;
  it can never grow unbounded.
- **Sync-back (optional).** If the app subscribes to `ev=camera`, the terminal reports the camera node's
  resolved local TRS so the app can persist it (§12.3.1).
- **Auto-spin** is an explore option, not a default.

#### 12.3.1 Camera sync-back

`ev=camera` carries the camera node's resolved **local TRS** in the exact schema the app would send in a
`node.upsert` (`node=` camera id, `t=` translate, `r=` rotation quat, `s=` scale), so the app can write it
back **round-trip-identically**. Cadence is **never per render frame**: the terminal emits on gesture
settle (drag-end, or explore coming to rest after damping) and, while a continuous gesture is in progress,
at a throttled steady-state cap (advertised as `camera_report_hz`, default 30) measured against the
injectable clock. Consecutive in-flight camera reports for one viewport coalesce to the latest (§12.5.4).
`ev=camera` is emitted only if the app subscribed to it for that viewport.

#### 12.3.2 Wheel ownership

Within a viewport's live cell rect, the wheel resolves in a fixed order:

1. If **explore zoom** is enabled for the viewport, explore consumes the wheel (zooms the camera node);
   no wheel event is reported and the surrounding text does not scroll.
2. Else, if the viewport has an active `sub` for `ev=wheel`, the terminal reports `tgp;e;ev=wheel` and the
   surrounding text does not scroll.
3. Else, the wheel passes through to the surrounding text/pager scroll — the default behavior for an
   inline viewport embedded in a scrollable pager.

Enabling explore zoom **and** subscribing `ev=wheel` on the same viewport is a conflict: the later of the
two is rejected with `tgp;x;code=invalid_sub` (they are mutually exclusive).

### 12.4 Picking (implementation)

Picking resolves a pointer position to `(node, instance, world-point)` against a **CPU-side last-pick
buffer** that is refreshed each time the viewport's pick pass renders — there is no synchronous GPU stall
on input. The CPU path (ray-cast against node world AABBs / instance transforms, or a CPU-rasterized pick
buffer) is **authoritative and always present**, so picks resolve deterministically without a GPU; the
optional GPU color-ID pass (each pixel encoding `node-id` plus `instance-index` for `Instanced` nodes) is
an acceleration that must agree with the CPU result. The world-space hit point is computed from the same
CPU buffer's stored depth/position.

- The pick buffer (and its GPU pass) exists **only** for viewports with an active sub (no cost otherwise).
- Resolution is **asynchronous** against the most recent buffer for that viewport. Each emitted report
  carries `rev=`, the `scene_revision` the buffer was rendered against, so the app can detect staleness.
  If the resolved node no longer exists in the scene at emit time, the report is dropped for
  `click`/`hover`/`enter`; for `leave` the prior node id is still cited (§12.5.2).
- v1 reports node id + instance index + world-space hit point. UV/barycentric reporting needs interpolated
  attributes in the pick pass and is **deferred** (heavier).

### 12.5 Event report wire format (terminal → app)

Mode-gated: only emitted for viewports/nodes the app subscribed to (`sub`), and only while the owning
token has a live subscription **and** the terminal is focused — so a non-interactive app or a dumb reader
never sees them. When the foreground process group changes or the PTY input side closes, the terminal
cancels that token's subscriptions, explore, and animation and stops emitting, preventing report bytes
from leaking into a shell prompt as garbage. Each report is a control frame on stdin, tagged with the
owning token:

```
ESC _ tgp;e;tok=AP1;vp=2;ev=click;node=O1;inst=5;btn=1;x=0.31;y=0.62;wx=1.4;wy=0.0;wz=-2.1;rev=42 ESC \
ESC _ tgp;e;tok=AP1;vp=2;ev=enter;node=bond_3;rev=42 ESC \
ESC _ tgp;e;tok=AP1;vp=2;ev=leave;node=bond_3;rev=43 ESC \
ESC _ tgp;e;tok=AP1;vp=2;ev=camera;node=cam;t=…;r=…;s=… ESC \
ESC _ tgp;e;tok=AP1;vp=2;ev=resize;cols=20;rows=12 ESC \
ESC _ tgp;e;tok=AP1;vp=2;ev=destroyed;reason=evicted ESC \
```

- `ev` ∈ `click | dblclick | down | up | hover | enter | leave | drag | wheel | camera | resize |
  destroyed`.
- `node` is the picked node id (string), `inst` the instance index when applicable; `x,y` viewport-local
  normalized coords; `wx,wy,wz` the world-space hit point; `rev` the `scene_revision` the pick was
  resolved against.
- **Subscription granularity.** A `sub` selects a viewport and either "all nodes" (`nodes=*`) or a list of
  node ids, and which `ev` types to receive — so an app gets only the events it asked for.
- **Disambiguation.** Reports are TGP-namespaced APC, distinct from SGR mouse. Within a subscribed
  viewport's cell rect, an active sub **captures** the pointer and **suppresses** app-enabled SGR mouse
  reporting there; the click is reported only as `tgp;e`. Outside every subscribed viewport's rect, SGR
  reporting is untouched and no `tgp;e` is emitted. Capture is keyed to the viewport's **live
  placeholder-cell rect** (§10.2), so it follows scroll and reflow rather than a stale stored position.
  A per-sub opt-in flag `both=1` (default `0`) makes the terminal emit **both** SGR and `tgp;e` within the
  rect, leaving dedup to the app, for hybrid TUIs.

#### 12.5.1 Subscription lifecycle

Subscriptions are owned by the namespace token and bound to a viewport id:

- **Viewport destroy** drops all of that viewport's subs and frees its pick target and pick buffer. (A
  viewport is destroyed explicitly, or implicitly when its inline placeholder cells are evicted from
  scrollback or overwritten with text; a subscribed owner receives one `ev=destroyed` with `reason=`.)
- **`node.remove`** silently drops the removed id from any explicit node-id sub list for that viewport
  (no error, no event); if the removed node was hovered, a `leave` is synthesized first (§12.5.2).
- **`nodes=*`** is a viewport-wide filter that matches any current node, and so naturally covers
  re-created ids.
- **Re-creating** a previously-removed specific node id via `node.upsert` does **not** revive an explicit
  sub for that id; the app must re-subscribe.
- A `sub` naming an id that does not currently exist is accepted (filters resolve at event time) but
  matches nothing until that id exists — it is not an error.
- All sub mutations and the events they produce route only to the owning token; a sub under one token can
  never match or report on another token's nodes.

#### 12.5.2 Hover state machine

Hover state is held **per viewport** as a single current target — `(node, inst)` or `None`. It is
recomputed every render from that viewport's refreshed pick buffer **and** on every pointer-move input,
whichever happens — so a stationary cursor still tracks geometry that moves under it via explore,
animation, patch, or reflow. On a change:

- If the newly-resolved target differs from the held target, the terminal emits `leave` for the prior
  `(vp,node,inst)` **then** `enter` for the new one.
- `hover` fires on pointer-move while a target is held.
- A held target that is **removed**, **hidden** (`node.visible=false`), or **moves out** from under the
  cursor (for any reason — transform, animation, explore, reflow, scroll-driven rect motion, or crossing
  a viewport boundary) synthesizes a `leave` citing the prior id (even if just removed); the held target
  then becomes the new target or `None`.
- Hover is independent per viewport; crossing a boundary fires `leave` against the old viewport then
  `enter` against the new one.

#### 12.5.3 Focus and pointer loss

On focus-out or pointer-leave while a gesture is active in a subscribed or exploring viewport, the
terminal **ends** the active gesture:

- Explore stops dragging immediately and lets damping decay to rest on the injectable clock — no infinite
  spin.
- For any viewport with an active sub, the terminal synthesizes `ev=up` (for the in-flight button, if
  `down`/`up`/`drag` is subscribed) **then** `ev=leave` for the held hover target, so app-side drag and
  highlight state can close.

A subsequent focus-in / pointer-enter does **not** auto-resume the drag; it only re-derives hover from the
current pointer (firing `enter` if over a target). Synthesized `up`/`leave` are token-routed to the owning
app.

#### 12.5.4 Back-channel ordering & coalescing

All terminal → app traffic — `tgp;e` events, `tgp;a` acks, `tgp;x` errors, and DA1 — shares one **ordered
FIFO** back-channel. The channel never reorders, and is advertised as ordered so apps correlate `txn` acks
reliably (§8.6, error/ack correlation echoes the offending frame's `txn`/`vp`/`asset` correlator plus
`tok=`).

- **Coalescing.** Consecutive still-pending events of the same `(viewport, ev-type)` where
  `ev ∈ {hover, drag, camera, wheel}` may be coalesced to the latest before flush. `click`, `dblclick`,
  `down`, `up`, `enter`, `leave`, `resize`, `destroyed`, and **all** acks/errors/DA1 are **never**
  coalesced and **never** reordered.
- **Backpressure.** If the back-channel exceeds its advertised cap (`max_backchannel_bytes`), the terminal
  drops the **oldest coalescable** entries first (hover/drag/camera/wheel); it never drops or delays an
  ack, an error, or a non-coalescable event.
- Token scoping is orthogonal: each token's reports are tagged for that token's app, and order within a
  token's stream is FIFO.

### 12.6 Worked interaction loop

1. App handshakes (`tgp;q` → `tgp;r`), establishing its token, then
   `sub tok=AP1 vp=2 nodes=* ev=click,hover`.
2. User clicks an atom. The router hit-tests top-down by `z` among subscribed viewports (§12.1.1), resolves
   the cursor against viewport 2's CPU pick buffer (§12.4) → `node=O1, inst=5`, world point from the
   buffer's depth.
3. Terminal → app:
   `ESC _ tgp;e;tok=AP1;vp=2;ev=click;node=O1;inst=5;wx=1.4;wy=0;wz=-0.4;rev=42 ESC \`.
4. App reacts (its choice): prints "Oxygen, 16.00 u", or sends a `patch` tinting `O1` highlighted, or
   opens a menu. The terminal did **not** decide what a click means — it only reported it.

If the user instead **drags**, the gesture exceeds the dead-zone and is owned by explore (§12.2.1): the
terminal mutates viewport 2's camera node and, if the app subscribed `ev=camera`, reports the settled TRS
on drag-end so the app can persist it (§12.3.1) — with no per-frame round-trip.

## 13. Accessibility

Visual fallback is the app's job (principle 4), but assistive-technology (AT) exposure needs terminal
help — the terminal is the only party that owns the AT integration and the cell grid a screen reader walks.
This is the one place TGP asks the terminal to add value beyond rendering, and it is a cheap lead: no
existing terminal graphics protocol carries structured alt-text.

### 13.1 Alt-text in the scene

- Every node may carry an `alt` string, set on `node.upsert` and updated like any other sparse field. The
  scene itself, and each viewport, may carry an `alt` caption too. A `Mesh` for a molecule carries
  `alt: "caffeine molecule, C8H10N4O2"`; a viewport carries `alt: "interactive 3D inspector"`.
- `alt` is app-authored data that travels **in the scene** under the creating token's namespace, but is
  surfaced by the terminal. It is bounded like every other string field (UTF-8, ≤ 64 bytes, no control or
  ESC bytes — these strings can re-enter the host's input path through AT tooling). Over-cap or malformed
  `alt` is rejected with the whole txn (`x code=bad_param`); the scene is unchanged.
- `alt` is purely descriptive. It never affects rendering, hit-testing, dirty-tracking, or revisions, and
  setting it alone bumps neither `scene_revision` nor `asset_revision`.

### 13.2 Terminal surfacing

- The terminal exposes a viewport's `alt` (and, when an AT cursor lands inside a viewport's cell rect, the
  `alt` of the node under that cell, resolved by the same CPU pick path used for events — §12.4) to the
  platform accessibility layer and as a cell-level fallback caption when a viewport cannot be presented
  visually (no GPU, a headless/SSH session that negotiated text-only, or a reader walking scrollback).
- An inline viewport's authoritative placeholder cells (§10) carry the viewport `alt` as their accessible
  name, so the region announces itself in line-based readers exactly where it sits in the text flow. A
  pinned viewport announces against its fixed region.
- The terminal never fabricates descriptions. A node or viewport with no `alt` is exposed as an unlabeled
  graphic; the app, not the terminal, decides how much to describe.

### 13.3 Relationship to fallback

Visual degradation stays app-driven: the terminal answers the capability query (§7) and the app chooses to
send TGP or fall back to sixel/ASCII/text. AT surfacing is the complement — even when the app does send TGP
and the terminal renders it, the `alt` channel lets the scene remain legible to a non-visual reader without
the app shipping a second, text-only rendering.

---

## 14. Animation

Authored motion travels with the asset; how it plays is the app's choice. By default the app drives every
transform (principle 1). Terminal-side playback is an opt-in convenience that, like the explore controller,
takes control of a well-defined set of nodes only when the app asks.

### 14.1 Clips

- **Authored clips travel with the asset.** glTF animation channels — TRS keyframes with STEP, LINEAR, and
  CUBICSPLINE interpolation — are imported when their owning GLB is added and stored as `AnimationClip`s.
- **Clips are auto-registered on `asset.add`.** There is no separate `clip.add` op. When an `asset.add`
  commits a GLB carrying animations, the terminal registers one clip per glTF animation and returns the
  glTF-animation-index → `ClipId` mapping in the commit `a` ack for that asset (acks are available to any
  handshaked token — §8). Clip ids are numeric handles (`u32`) in the creating token's namespace, alongside
  asset and material ids; an app that needs deterministic clip ids reads them from the ack rather than
  guessing.
- **`anim;play` against an unknown clip id is an error** (`x code=unknown_clip`), never a silent no-op.

### 14.2 Channel resolution (`node=` is the subtree root)

A glTF clip animates many nodes — a whole skeleton or articulated rig — but a playback names a single
`node=`. **`node=` is the subtree root the clip drives.** The clip's channel targets resolve **relative to
`node=`** (by relative id/index within the imported hierarchy), addressing descendants of `node=`. This is
what lets one imported rig clip play the robot arm's shoulder/elbow/wrist/hand from `node="base"`, and lets
the same clip be played on multiple instantiated copies of a rig by pointing each playback at a different
subtree root. A channel whose relative target resolves to no node under `node=` is skipped (it animates
nothing); a playback whose channels resolve to **no** existing targets at all is an error
(`x code=no_targets`).

### 14.3 Playback verbs and the playback state machine

```
ESC _ tgp;anim;tok=…;play;clip=<u32>;node=<id>;loop=0|1;speed=<f32> ESC \
ESC _ tgp;anim;tok=…;pause;clip=<u32>;node=<id> ESC \
ESC _ tgp;anim;tok=…;seek;clip=<u32>;node=<id>;t=<seconds> ESC \
ESC _ tgp;anim;tok=…;stop;clip=<u32>;node=<id> ESC \
```

- **Handle = `(clip, node)`.** A playback is identified by the clip id and its subtree-root node; distinct
  `(clip, node)` pairs are independent playbacks that can run concurrently. `pause`/`seek`/`stop` address an
  existing `(clip, node)`.
- **State machine — Stopped / Playing / Paused.** `play` on a Stopped pair starts it from `t=0`; `play` on
  an already-Playing or Paused pair **restarts** from `t=0`. `pause` holds the current pose (Playing →
  Paused); `play` resumes a Paused pair from `t=0` (restart semantics — use `seek` then `play` to resume at
  a point, or `seek` while Paused to scrub). `seek` sets absolute clip time `t` and re-evaluates the pose
  without changing Playing/Paused state. `stop` ends the playback (→ Stopped).
- **`stop` releases ownership and holds the last pose.** On `stop`, the terminal releases its ownership of
  the affected nodes' TRS and leaves them **at their last-evaluated pose** — there is no snap to bind pose.
  The app may then drive those nodes again with patches. `pause` likewise holds the current pose but retains
  ownership.
- `pause`/`seek`/`stop` against a `(clip, node)` that is not currently a live playback is `x code=not_playing`.

### 14.4 Loop, speed, and end-of-clip

- `loop` is a boolean (`0`/`1`). `speed` is a float clamped to `(0, anim_speed_max]`; the cap is advertised
  in the capability reply (§7) and an over-cap or non-positive `speed` is `x code=bad_param`. Reverse,
  freeze, and integer loop counts are deferred (§16).
- **A non-looping clip holds its last frame at end** (it does not snap to bind pose) and emits an optional
  `anim_end` event to the owning token if subscribed (§12). The playback transitions to Stopped and releases
  TRS ownership, leaving the nodes posed at the final frame — so an app awaiting a one-shot finish can
  re-tint, swap, or hand back to its own transforms with the node in a known state. A looping clip wraps and
  never emits `anim_end`.

### 14.5 Ownership and conflicts

Playback takes exclusive control of the TRS channels it animates, mirroring how explore takes the camera —
two terminal-side controllers never fight over one transform.

- **Playback vs app patches.** While a `(clip, node)` is Playing or Paused, the clip exclusively owns the
  animated TRS channels of the nodes it drives. A `node.upsert` that writes a TRS field of a node owned by an
  active playback is rejected (`x code=node_busy`, whole txn rejected, scene unchanged). To regain control
  the app sends `stop` first (which holds the last pose), then patches. Upserts of non-TRS fields on a played
  node (visibility, tint, material ref, alt) are unaffected.
- **Playback vs explore on a camera.** A viewport's camera is driven by at most one terminal-side
  controller. If a viewport has `explore` enabled, an `anim;play` targeting that viewport's camera node is
  rejected (`x code=camera_busy`); conversely, enabling `explore` on a viewport stops any playback already
  driving its camera node. Playback may freely target ordinary model nodes; the camera is the only contended
  node, and it has a single owner.
- **Removing a played node auto-stops playback.** `node.remove` of a node bound to an active playback (or any
  of its driven descendants) succeeds and **auto-stops** that playback — releasing ownership and emitting an
  optional event/ack — rather than failing. A later same-id `node.upsert` starts fresh: there is no
  auto-rebind, since string ids are reused intentionally and the new node is a new identity for animation
  purposes.

### 14.6 Ordering relative to patches

All TGP frames — patches and `anim` control frames alike — are processed in strict PTY receive order, so a
prior patch's `node.upsert` commits before a following `anim;play` observes the scene. The robot-arm pattern
(upsert the rig, then `play` it in the same write) works because the upsert is committed first. An
`anim;play` whose `node=` is not a committed node at the time it is processed is `x code=unknown_node`; it is
not queued for a future node.

### 14.7 Clock and scheduling

- **Single injectable deterministic clock.** All terminal-side time — clip playback, `seek`, explore
  damping, camera-report throttling, and inactivity/partial-frame timeouts — is driven from one injectable
  monotonic clock. The production clock is wall-time; tests inject a controllable clock and advance it
  explicitly, so node world-matrix assertions over a playing clip are deterministic and reproducible. `seek`
  is the same operation a test uses to set absolute clip time.
- **Playback drives the dirty machinery, not global revisions.** Each tick, playback evaluates the clip,
  writes the affected nodes' local TRS, and sets those nodes' **per-node dirty flags**; the terminal then
  recomputes only the dirty subtree's world matrices and **re-renders only the affected viewport(s)** through
  the existing per-node / per-viewport dirty + generation-token machinery (§6.5, §10). Playback does **not**
  bump global `scene_revision` every frame — preserving the no-re-upload optimization — and does **not**
  re-emit unrelated text cells. `asset_revision` is never touched by playback.

### 14.8 Skinning

Skinning — joints, skin matrices, and in-shader vertex skinning — is present in the scene model because
glTF skinning is defined over the node tree (§6.3), so a skinned asset can arrive day one via the inline-GLB
path. v1 renderer behavior is defined and safe:

- A skinned GLB is **accepted** and parsed with the same bounds checks as any asset; the terminal renders
  its **bind pose** and ignores joint weights. It never crashes or silently misrenders into garbage.
- The terminal does **not** advertise `feat=skin` in v1. An app detects the missing flag and chooses its
  fallback per principle 4 (e.g. send pre-baked per-frame TRS patches, or accept the static bind pose).
- Production skinning — driving joints from clip channels through the skinning shader path — lands with
  `feat=skin` in a later phase (§16, §17); the flag is the contract, and it is only set when the shader path
  is real.

## 15. Security & robustness

### 15.1 Threat model

TGP bytes arrive on the PTY from **untrusted sources** (a `curl`, a `cat`'d file, a log line, a web
page rendered in a pager). The protocol must be safe to *receive*, not just to *send* (principle 3). The
hardening that follows is organized around three concrete threats and the mitigations that close each:

- **Resource exhaustion.** A malicious or accidental frame declares enormous geometry, textures, or
  instance counts, or drip-feeds an endless binary frame, to OOM or wedge the terminal. Mitigated by the
  caps regime (§15.2), bounded parser states, and check-before-allocate decode (§15.5).
- **Confused-deputy / cross-app corruption.** Bytes from one writer (a stray log line, a hostile
  `curl | cat`) overwrite or hijack scene objects another app created — e.g. moving your inspector's
  camera, deleting a node, or wiping an in-flight upload. Mitigated by **per-token addressing isolation**
  (§15.2): every object is namespaced under the token that created it, and a frame may only address its
  own token's objects.
- **Parser-level crash / corruption.** Malformed framing, truncated binary, or pathological asset blobs
  crash the terminal or corrupt the byte stream so later valid frames mis-parse. Mitigated by hardened,
  bounds-checked parsing (§15.5), framing that re-syncs after any rejection (§15.2), and a single fuzz
  oracle over the whole ingress seam (§15.5).

A single PTY exposes one writeback channel shared by every process on it; the terminal cannot physically
deliver bytes to one of several co-resident processes. Isolation is therefore enforced where it *is*
achievable — at addressing and at emission gating/tagging — not by impossible per-process delivery. See
§12 for the event side of the token model.

### 15.2 Resource caps & addressing isolation

**Caps.** Every cap the terminal enforces is advertised in the capability reply (§7.1), and the terminal
**MUST NOT** enforce a cap it did not advertise — so an app can always pre-trim instead of being silently
rejected (principle 4). The advertised set and the enforced set are the **same closed, scope-suffixed
vocabulary**; the same field name appears in the reply, in enforcement, and in the `detail=` of the
error that fires when it is breached:

- **Per-asset / per-buffer:** `max_verts_per_asset`, `max_indices_per_asset`, `max_texels_per_texture`,
  `max_tex_dim`, `max_instances_per_buffer`, `max_asset_bytes`.
- **Per-message:** `max_msg_mb` — the **decoded** payload size, applied identically under both encodings
  (a base64 frame's encoded length is bounded at `ceil(max_msg_mb × 4/3)`).
- **Per-session:** `max_vram_mb_session`, `max_nodes_session`, `max_node_depth`, `max_viewports_session`,
  `max_reasm_mb` (in-flight reassembly + partial-binary budget), `max_pending_bytes_session`,
  `max_lights_per_viewport`, `anim_speed_max`, `camera_report_hz`, `max_backchannel_bytes`.
- **Inline / viewport sizing:** `max_inline_cols`, `max_inline_rows`, `max_viewport_px_w`, `max_viewport_px_h`.
- **Rendering / input:** `msaa`, `click_deadzone_px`, `max_id_bytes`.

**Caps are enforced before allocation, and over-cap drains then errors.** No cap is checked only after
the fact; each is checked at the point where memory would be committed, and a breach rejects the
offending unit atomically with a structured error (§15.3) — never a crash, never an unbounded or partial
allocation. The recurring shape is **drain-then-error**: when a cap fires on a length-prefixed binary
frame, the terminal still consumes (and discards) exactly the declared bytes so stream framing stays
aligned and the next `tgp;` header re-syncs cleanly, then emits the error. The specific loci:

- **Message size.** A binary header (`tgp;p;tok=…;txn=…;enc=bin|b64;len=N`) is validated against
  `max_msg_mb` at **text-header parse time, before entering consume-N mode**. On over-cap the terminal
  still enters consume-N, drains exactly `len` bytes, returns to Ground, and emits
  `code=msg_too_large` citing `txn` (correlation lives in the text header, so it survives a payload that
  was never decoded — §15.3).
- **Truncation / stall.** A binary frame in consume-N state is aborted — buffered bytes discarded,
  budget freed, `code=truncated` emitted — on either a deterministic inactivity bound (measured in
  PTY-read events against the injectable clock, not wall time) or any terminal reset (RIS/DECSTR). The
  consume-N byte counter itself guarantees the state exits after exactly `len` bytes, so a too-large
  `len` (already rejected pre-decode) can never wedge the parser.
- **Chunk reassembly.** Cumulative pending size is checked **on every chunk before its bytes are
  accepted**, against a per-session reassembly budget (`max_reasm_mb`) shared with in-flight binary
  frames. A breach rejects the chunk, **immediately frees all buffered bytes for that id**, decrements
  the session budget, and emits `code=cap_exceeded`. No memory is reserved ahead of arrival. A second
  in-flight stream opened for an id that already has one, or a format change mid-stream, yields
  `code=chunk_conflict` and **retains the prior buffer** (an interleaved hostile stream cannot wipe a
  legitimate upload). An in-flight reassembly buffer is not a registered asset: immutability and `mesh:`
  reference resolution check only **committed** assets, and `more=0` commits and bumps `asset_revision`
  once.
- **Expanded-output (decompression bombs).** A small wire payload that expands to huge geometry or
  texels is the classic terminal-crash vector, so the guard is on **expanded output checked incrementally
  during decode**, with allocation gated on the declared-size check rather than discovered after OOM
  (§15.5). A wall-clock watchdog MAY exist as a non-normative backstop only; no normative behavior
  depends on it.
- **VRAM.** VRAM is accounted **two-phase, CPU-side**, as the single source of truth (the GPU is never
  queried for residency). A txn's net VRAM delta is computed from its declared/expanded asset, texture,
  and instance-buffer footprint (shared assets counted once, freed bytes from same-txn removals netted
  out) and validated against `max_vram_mb_session` **before any upload**. Over-cap rejects the **whole
  txn** with `code=vram_exhausted` and commits nothing; upload and the `asset_revision`/`scene_revision`
  bump happen only after the txn validates.
- **Instance churn.** A `node.instances` op over `max_instances_per_buffer` (or over VRAM) rejects the
  txn atomically and **retains the node's last committed instance buffer** — never cleared, never
  flickered to empty. Multiple `node.instances` for the same node within one render frame coalesce to the
  latest committed buffer (one upload per node per frame), bounding churn from a spammy stream without a
  hard rate cap in v1.

**Addressing isolation (the confused-deputy fix).** There is **one namespace per token**, not one global
namespace per session. Every node, asset, material, clip, and viewport id is scoped under the token that
created it (the token is established at handshake and carried in the text header of every frame, §7/§12).
A frame bearing token *T* may only address, mutate, or reference objects created under *T*; a frame whose
token differs from — or is absent for — the object it tries to `upsert`, `remove`, or reference is
rejected with `code=denied`, the whole txn unchanged. This is what prevents a stray `curl | cat` line
from overwriting your `cam` node or deleting your scene. A handshake that proposes a malformed or
unacceptable token is itself rejected with `code=bad_token`.

### 15.3 Structured errors (vs RGP's silent drop)

Where RGP silently drops malformed input, TGP replies with a structured error. **Error reporting is the
handshake itself:** a completed `tgp;q` / `tgp;r` exchange enables `x` (errors) and `a` (acks) for that
token for the rest of the session — there is no separate `errors=` flag in v1. Sending a well-formed
query proves the writer speaks TGP and reads replies, so it is not a dumb reader to be spammed; a writer
that never handshakes (a raw `tgp;p` from untrusted output) is still processed **safely** (caps enforced,
scene protected) but receives **no** `x`/`a`/`e` — it is dropped silently like any dumb reader. One
consequence is preserved deliberately: a `tgp;`-prefixed frame that fails to parse, **from a token that
has handshaked**, emits `code=parse_error` rather than vanishing, so a garbled or typo'd TGP frame is
diagnosable instead of mysteriously inert.

```
ESC _ tgp;x;tok=A1;txn=42;op=3;code=cap_exceeded;detail=max_verts_per_asset ESC \
ESC _ tgp;x;tok=A1;txn=43;code=parse_error;detail=accessor_oob ESC \
ESC _ tgp;x;tok=A1;txn=51;code=msg_too_large;detail=len=70000000,cap=67108864 ESC \
```

- **Correlation lives in the text header.** Every `x`/`a` carries `tok=` plus whatever correlator the
  offending frame carried — `txn` for a patch (with `op=` index when an op index is knowable), the `vp`
  id for a viewport frame, the `asset` id for a chunked upload — and a frame with no correlator yields an
  `x` with `code` only. Because `tok`/`txn`/`enc`/`len` are in the text header, the error is answerable
  even when a cap fired before any payload was decoded; `op=` is omitted exactly when no op index was
  ever parsed (e.g. a pre-decode `msg_too_large`).
- **Per-txn, atomic.** A patch error rejects the whole txn (scene unchanged) and cites the offending op
  index; no partial application, no mid-frame tearing.
- **Token-scoped, gated.** Errors, acks, and events are emitted on the single shared FIFO **tagged with
  `tok=`** so the reader can demux; the terminal does not (and physically cannot) guarantee which process
  reads them. Emission is gated: a token's `x`/`e`/`a` flow only while it has a live handshake, and the
  terminal stops emitting for a token when the foreground process group changes or the PTY input side
  closes — so TGP bytes never leak into a shell prompt as garbage.

**The error `code` is a closed, versioned enum.** Apps branch on `code` only; `detail=` is free-form,
advisory, and **non-load-bearing** (apps MUST NOT parse it for control flow). New codes may be added only
with a version bump; an unknown `code` is treated as a generic fatal-txn error. The v1 set is:

```
parse_error   msg_too_large   truncated      cap_exceeded   vram_exhausted
bad_ref       dup_id          unknown_op     unknown_node   unknown_clip
kind_conflict cycle           depth_exceeded bad_index      bad_layout
bad_param     chunk_conflict  unsupported    denied         node_busy
camera_busy   not_playing     no_targets     invalid_sub    bad_token
null_not_allowed
```

The enum is the union of every reject path in the protocol, so the taxonomy and the actual reject sites
are one closed set: `msg_too_large`/`truncated`/`cap_exceeded`/`vram_exhausted`/`chunk_conflict` from the
caps loci (§15.2); `parse_error`/`bad_index`/`bad_layout`/`bad_param`/`null_not_allowed` from decode and
op validation; `bad_ref`/`dup_id`/`unknown_op`/`unknown_node`/`unknown_clip`/`kind_conflict`/`cycle`/
`depth_exceeded` from scene/patch semantics; `denied`/`bad_token` from token isolation (§15.2);
`node_busy`/`camera_busy`/`not_playing`/`no_targets`/`unsupported` from controller and feature gating;
and `invalid_sub` from subscriptions.

### 15.4 No file access

Native TGP has **no `path=`** and no file, temp, or shared-memory loading of any kind — assets are always
inline in the protocol bytes. This removes the entire file-access attack surface by construction. TGP is a
clean break: there is no RGP adapter and TGP neither bridges to, reuses, nor inherits the legacy
`ratty;g;` path, which remains independent with its own semantics and its own (separate) handling of
`path=`. A sandboxed, threat-modeled path-load feature may be revisited later (§16) but is entirely out of
scope now.

### 15.5 Hardened parsing & the fuzz oracle

- **Bounds-checked asset parsing.** glTF/GLB parsing reads the full mesh set (not RGP's first-primitive
  shortcut) with bounds checks on every accessor, index, and buffer-view: reject out-of-range offsets and
  lengths, reject NaN/inf where invalid, reject overlong strings, and reject any declared `count` whose
  expansion would exceed a cap. Critically, the declared-size check happens **before** the allocation, so
  a tiny blob declaring a billion-element accessor is rejected without ever attempting the allocation.
- **Bounded decode at every expansion point.** Expanded-output caps are enforced incrementally, never
  only at the end, at three loci: (1) glTF/GLB accessor expansion (declared `count` × component size
  against `max_verts_per_asset`/`max_indices_per_asset`, plus per-accessor bounds against the actual blob
  length); (2) CBOR decode (cap array/map element counts and nesting depth so a tiny CBOR cannot declare a
  billion-element array); (3) texture decode (check declared dimensions against `max_tex_dim` **before**
  decompressing, and abort if running output exceeds the budget mid-decompress). Any breach aborts decode,
  **frees the partial allocation immediately**, rejects the txn, and emits `cap_exceeded` (or
  `parse_error`).
- **Node-id hardening.** Node ids re-enter stdin inside `tgp;e` reports, so they are hardened against
  injection: scoped to the creating token, capped at ≤ 64 bytes, and restricted to a printable charset
  with **no control or ESC bytes**.
- **Capability gating for risky verbs.** Input-event reporting, explore, and animation playback are
  opt-in per token; nothing that takes control or routes data back to the app happens without explicit
  request (principle 2).

**The fuzz oracle.** A single fuzz harness drives bytes through the **`Parser::advance(&mut Term, bytes)`
ingress seam** into a **GPU-free `Term`** (no wgpu device) and asserts post-conditions via CPU-side
accessors. There is no RGP-differential target (no adapter exists). The oracle is one normative invariant
that must hold for **any** input — adversarial, truncated, over-cap, or pathological:

1. **No crash.** The terminal never panics, aborts, or corrupts the byte stream; after any rejection the
   framer re-syncs on the next `tgp;` header.
2. **Bounded memory at caps.** Allocation never exceeds the advertised caps — per-asset, per-session
   reassembly, VRAM, and expanded output are all respected.
3. **Bounded progress.** The parser never holds indefinitely; consume-N exits after exactly `len` bytes,
   or the inactivity/reset abort fires.
4. **Commit, structured error, or clean drop — never torn.** Every txn either commits a fully valid,
   bounded scene, **or** is rejected atomically with a structured `x` (when the token handshaked), **or**
   is cleanly dropped (no handshaked listener) — never a partial or torn scene, never a leaked buffer.

The harness asserts these via the CPU-side state the testing seams expose: scene node count and world
matrices, pending-bytes, VRAM-used, and the queued PTY replies. At quiescence the pending-bytes accessor
**must return to 0** (no leak) and VRAM-used must equal the committed scene's accounted footprint. The
corpus is seeded from the per-rule cases above — truncated binary headers, over-cap `len`s, duplicate ids,
out-of-bounds accessors, cross-token writes — so the fuzzer exercises framer, demux, decode, caps, and
teardown end-to-end.

## 16. Open questions / deferred

Several questions that were open in the design brainstorm are now **settled** and have moved into the
normative body of this document; they are recorded here as resolved so the phasing and plan can rely on
them. The remainder stay deferred with an explicit disposition.

### 16.1 Resolved (now normative)

| Item | Resolution |
|---|---|
| Per-app isolation model | A **per-app token** (`tok=`, ≤ 16 bytes, established at handshake) namespaces every object id and gates every emitted reply. A frame may only address ids created under its own token; cross-token mutation is rejected with `denied`. The PTY has a single writeback channel, so isolation is *addressing isolation + tagged, gated emission*, not per-process delivery — every `e`/`x`/`a` frame is tagged with `tok=` and the reader demuxes (§8.4, §12.5). |
| Relationship to RGP | **Clean break.** Native TGP shares no code, no semantics, and no namespace with RGP. The existing `ratty;g;` implementation remains an untouched, independent legacy path; there is no adapter and TGP never bridges to or inherits from it. |
| Error reporting opt-in | A completed `tgp;q`/`tgp;r` handshake **is** the opt-in. There is no separate `errors=` flag; dumb readers never handshake and therefore never receive `e`/`x`/`a` (§7, §15.3). |
| Error-code vocabulary | A single **closed, versioned enum** (§15.3). `detail=` is advisory and non-load-bearing. |
| Capability reply shape | One closed, scope-suffixed set shared between the `tgp;r` reply and the cap list (§7.1, §15.2): `v=`/`vmin=` range, a frozen `feat=` token set with implication rules, `enc=b64,bin` (b64 is the safe default; `bin` is verified via a probe→ack), `color=srgb`, and explicitly-named caps. |
| Picking implementation | Picking has a **mandatory CPU path** (ray-cast against node/instance world AABBs, or a CPU-rasterized pick buffer) so click→`(node, inst, world-point)` resolves with no GPU. The GPU color-ID pass is an optional accelerator that must agree with the CPU result (§12.4). |
| Terminal-side timing | All terminal-side time (animation clock, explore damping, camera-report throttle, reassembly/inactivity timeouts) runs off **one injectable monotonic clock** so behavior is deterministic and testable (§14, §12.3). |

### 16.2 Still deferred

| Item | Disposition |
|---|---|
| Exact binary encoding (CBOR vs custom TLV/flatbuffer) | v1 = CBOR; framing is encoding-agnostic so it can change later without a framing break. |
| Final op naming / field schema | Strawman in §8; to be pinned during writing-plans. |
| Declarative client library (app sends desired state; lib diffs → patches) | Additive on top of the imperative wire; later. |
| Per-instance material handles (material arrays / bindless) | v1 = per-instance tint + one shared material. |
| UV / barycentric in pick reports | Deferred; v1 reports node + instance + world point. |
| Shadows, IBL, post-processing, HDR / wide-gamut | Deferred (fidelity phase). |
| Skeletal animation production polish | Model supports it; staged. |
| Text as a 3D scene node (labels in 3D space) | Possible later via a `text` node kind. |
| Sandboxed / allowlisted path loading | Out for now (§15.4); revisit only with a real threat-modeled design. |
| tmux passthrough specifics + binary-vs-base64 negotiation defaults | Validate against real tmux/ssh configs during implementation; b64 is the conservative default until the `enc=bin` probe succeeds. |

---

## 17. Suggested phasing (non-binding; for writing-plans)

Five phases. Earlier phases stand alone and are independently shippable; later phases add the
differentiators and the fidelity/hardening work.

1. **Foundation.** `tgp;` framing (control frames + the length-prefixed binary frame), CBOR decode, and the
   capability handshake — `tgp;q` sent before `ESC[c`, `tgp;r` enqueued before the DA1 reply, with the
   `tok=` token established here. The retained scene model (graph + per-node/per-viewport dirty flags +
   generation tokens), **per-token id namespacing**, inline GLB assets (hardened, full mesh set), the
   default material, and one viewport rendering on the new wire. A **deterministic injectable clock** and the
   **CPU-side geometry/pick spine** (world matrices, viewport pixel rects, instance decode, CPU pick
   resolution — all queryable without a GPU) are stood up in this phase so every later feature is testable
   from the start.

2. **Differentiators.** Multiple viewports (inline + pinned) with offscreen depth-aware compositing and
   terminal-driven reflow; GPU instancing; **CPU-first picking** plus event reports, subscriptions, and the
   explore controller. Inline viewport placement is governed by authoritative placeholder cells (§10), and
   the inline cell caps from the handshake are enforced (over-cap viewport requests are rejected).

3. **Materials & light.** Registered PBR materials + textures; punctual lights as nodes; tone-mapping into
   the sRGB offscreen so the composite matches the text plane; per-viewport MSAA; theme-tint.

4. **Animation.** glTF clip import; opt-in playback verbs driven off the injectable clock (with the
   single-controller-per-camera rule and last-evaluated-pose `stop` semantics); then skinning.

5. **Hardening.** Single fuzz oracle at the `Parser::advance(&mut Term, …)` seam (every input yields a
   bounded scene or a structured error / clean drop, never a crash or leak), caps enforcement across all
   tokens, structured errors everywhere, lifecycle correctness (RIS teardown across all tokens, alt-screen
   and scrollback binding), and accessibility surfacing.

---

## 18. Worked examples (end-to-end)

Every frame carries a `tok=` in its text header; binary patches announce an exact `len=` and the raw bytes
follow outside escape-scanning. The token namespaces all ids and tags every reply, so a second app (or a
`curl | cat` line) sharing the PTY cannot address this app's objects and the reader can demux replies.

### 18.1 Inline molecule with instancing + click-to-identify

```
# 1) handshake: send tgp;q BEFORE DA1 so detection never hangs.
#    The app proposes its token; the terminal echoes it (or assigns one) in tgp;r,
#    which is enqueued before the DA1 reply on the shared FIFO.
app → term:  ESC _ tgp;q;tok=mol7;v=2;vmin=1;enc=bin ESC \   ESC[c
term → app:  ESC _ tgp;r;tok=mol7;v=2;vmin=1;
                  feat=geom,graph,instance,material,pbr,light,pick,event,explore,anim,bintrailer;
                  enc=b64,bin;color=srgb;
                  max_verts_per_asset=4000000;max_instances_per_buffer=1000000;
                  max_vram_mb_session=512;max_msg_mb=64;max_inline_cols=200;max_inline_rows=80 ESC \
             ESC[?…c

# 2) (optional) verify the binary transport before bulk streaming: enc=bin probe → ack.
app → term:  ESC _ tgp;p;tok=mol7;txn=0;enc=bin;len=2 ESC \  <2 raw bytes: empty CBOR patch>
term → app:  ESC _ tgp;a;tok=mol7;txn=0 ESC \

# 3) upload one sphere mesh + one cylinder mesh (inline GLB) and build the scene in one atomic patch.
#    The text header (tok, txn, enc, len) survives even if the binary payload is capped/aborted.
app → term:  ESC _ tgp;p;tok=mol7;txn=1;enc=bin;len=20512 ESC \  <20512 raw bytes of CBOR: [
                {do:asset.add,     id:1, fmt:glb, data:<sphere bytes>},
                {do:asset.add,     id:2, fmt:glb, data:<cylinder bytes>},
                {do:node.upsert,   id:"mol", trs:{...}},                  # group
                {do:node.instances,id:"atoms", parent:"mol", mesh:1,
                   xforms:<N transforms>, tints:<N colors>},              # all atoms, ONE draw
                {do:node.instances,id:"bonds", parent:"mol", mesh:2,
                   xforms:<M transforms>, tints:<M colors>},
              ]>
term → app:  ESC _ tgp;a;tok=mol7;txn=1 ESC \

# 4) place an INLINE viewport. The placeholder cells (cells=col,row,cols,rows) are authoritative;
#    a viewport that requests more than max_inline_cols/rows is rejected with x.
#    The camera node may not exist yet — the viewport auto-frames until "cam" is created.
app → term:  ESC _ tgp;vp;tok=mol7;id=2;anchor=inline;cells=0,12,40,16;camera=cam;
                  explore=orbit,zoom;lock=y ESC \
app → term:  ESC _ tgp;p;tok=mol7;txn=2;enc=bin;len=96 ESC \
                  <96 raw bytes of CBOR: [{do:node.upsert, id:"cam", camera:{...}}]>

# 5) subscribe to clicks for this viewport (the handshake already opted this token into reports).
app → term:  ESC _ tgp;sub;tok=mol7;vp=2;nodes=*;ev=click ESC \

# 6) user drags → terminal spins the camera NODE itself (no app round-trip); model nodes untouched.
# 7) user clicks an atom → CPU pick resolves (node, inst, world point).
term → app:  ESC _ tgp;e;tok=mol7;vp=2;ev=click;node=atoms;inst=6;x=0.31;y=0.62;
                  wx=1.2;wy=0.0;wz=-0.4 ESC \

# 8) the app decides what the click means — the terminal only reported it.
app → term:  (prints) "Oxygen (O), 16.00 u"
app → term:  ESC _ tgp;p;tok=mol7;txn=3;enc=bin;len=… ESC \   # re-tint instance 6 to highlight
```

Scrolling the terminal: the inline viewport scrolls with its placeholder cells (its cached layer is
re-composited at the new position; no re-render), and if the cursor was over instance 6 a `leave` fires
because the rect moved. Resizing the window: the terminal recomputes the pixel rect from the cell rect
automatically; the app does nothing. Evicting the placeholder cells from scrollback destroys the viewport
and frees its layer.

### 18.2 Pinned rotating inspector

```
app → term:  ESC _ tgp;vp;tok=insp;id=9;anchor=pinned;cells=80,0,40,24;camera=cam9;
                  explore=orbit,zoom,pan;clear=#101216 ESC \
```

Stays in the right-hand region while logs scroll on the left. The app streams `node.upsert` transforms only
when *it* wants motion; the user can still orbit (terminal-side) independently. Because `explore` owns the
camera, an `anim;play` aimed at `cam9` would be rejected with `camera_busy`.

### 18.3 Articulated robot arm (hierarchy + animation)

```
app → term:  ESC _ tgp;p;tok=arm;txn=1;enc=bin;len=… ESC \  <raw CBOR: [
                {do:asset.add,   id:1}, {do:asset.add, id:2},
                {do:asset.add,   id:3}, {do:asset.add, id:4},     # segments
                {do:node.upsert, id:"base"},
                {do:node.upsert, id:"shoulder", parent:"base",     mesh:1},
                {do:node.upsert, id:"elbow",    parent:"shoulder", mesh:2},
                {do:node.upsert, id:"wrist",    parent:"elbow",    mesh:3},
                {do:node.upsert, id:"hand",     parent:"wrist",    mesh:4},
             ]>
# rotate the shoulder → elbow/wrist/hand all follow, ONE op:
app → term:  ESC _ tgp;p;tok=arm;txn=2;enc=bin;len=… ESC \
                  <raw CBOR: [{do:node.upsert, id:"shoulder", trs:{rot:…}}]>
# or hand control to the terminal (advanced off the injectable clock):
app → term:  ESC _ tgp;anim;tok=arm;play;clip=0;node=base;loop=1;speed=1 ESC \
# stopping leaves each node at its last-evaluated pose (no snap):
app → term:  ESC _ tgp;anim;tok=arm;stop;node=base ESC \
```

### 18.4 Live point cloud (bulk data-viz)

```
# per frame, re-upload only the instance buffer (no graph changes, no mesh re-upload):
app → term:  ESC _ tgp;p;tok=pts;txn=N;enc=bin;len=131072 ESC \
               <131072 raw bytes of CBOR: [
                  {do:node.instances, id:"points", mesh:1, xforms:<bulk>, tints:<bulk>}
               ]>
```

No hierarchy needed here — this is the class-2 path: instancing + diff frames, not transform propagation.
Each `node.instances` bumps `scene_revision` and the node's instance-dirty flag (never `asset_revision`),
and an over-cap frame is rejected atomically while the last committed instance buffer is retained, so the
stream never flickers.

---

## 19. Glossary

- **Node** — an entry in the scene graph (mesh / light / camera / group), with a local transform and an
  optional parent.
- **Instanced node** — a node that draws one mesh many times in a single GPU draw via a per-instance buffer.
- **Viewport** — a cell-anchored window rendering the scene from a camera; inline or pinned. An inline
  viewport's placeholder cells are the authoritative source of its position.
- **Patch** — a transactional list of mutation ops; the core update message, carried in a length-prefixed
  binary frame.
- **Token** — a per-app handle (`tok=`, ≤ 16 bytes) established at handshake. It carries in the text header
  of every frame and identifies the owning app.
- **Namespace** — the per-token id space. Node / asset / clip / viewport / material ids are scoped to the
  token that created them; a frame may only address ids in its own namespace, and replies are tagged with
  `tok=` so a shared PTY reader can demux them.
- **Sticker** (informal) — a viewport's offscreen color+depth layer, composited into the grid with a depth
  test so it interleaves with text.
- **CPU pick** — the mandatory GPU-free resolution of a pointer event to `(node, instance, world-point)`,
  via ray-cast against world AABBs / instance transforms or a CPU-rasterized pick buffer. The GPU color-ID
  pass is an optional accelerator that must agree with it.
- **Deterministic clock** — the single injectable monotonic clock that drives all terminal-side timing
  (animation, explore damping, report throttling, timeouts), so behavior is reproducible and tests advance
  time explicitly.
