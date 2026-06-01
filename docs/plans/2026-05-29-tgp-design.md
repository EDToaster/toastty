# TGP — Toastty Graphics Protocol (Design)

- **Status:** Draft (design approved in brainstorm; not yet planned/implemented)
- **Date:** 2026-05-29
- **Supersedes / extends:** RGP (Ratty Graphics Protocol), as implemented in `crates/toastty-graphics` + `crates/toastty-render`
- **Related:** `docs/decisions/rgp-protocol.md`, `docs/decisions/rgp-3d-path.md`

> **Note on code references.** File:line references to the *current* RGP implementation in this
> document come from a grounding pass taken at design time (2026-05-29). They orient the
> implementation but **must be re-verified before coding** — the tree may have moved. They are
> marked _(grounding)_.

---

## 1. Summary

TGP is a modern, retained-mode 3D graphics protocol for the toastty terminal. It is a deliberate
**clean break** from RGP: a new `tgp;` escape-sequence namespace with a compact binary wire format,
a real **scene graph**, multiple **cell-anchored viewports**, true **GPU instancing**, **registerable
materials & lights**, and — the category-defining feature no terminal graphics protocol has — **interactive
3D** (clicking/hovering objects routed back to the application).

RGP is not discarded: an internal **adapter** translates incoming `ratty;g;` frames into the TGP scene
model, so existing RGP apps and demos (e.g. `molecule-viewer`) keep working unchanged while the renderer
is rebuilt around TGP.

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
  _term.rs:3417_). Does not survive SSH well (not fully GPU-resident).

TGP fixes all of the above and adds the differentiators below.

### 1.2 What sets TGP apart

1. **Interactive 3D** — hit-testing + event routing back to the app. *Nobody* in the field does this.
2. **Retained scene graph** with parent/child transforms, animation, skinning — vs RGP's flat list.
3. **True cheap instancing** — one mesh, thousands of tinted copies in a single draw.
4. **Multiple terminal-native viewports** that interleave with text, and reflow on scroll/resize.
5. **Safe by default** — no file access at all (assets are inline), structured capability negotiation,
   app-owned fallback.

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
   parsers, structured errors instead of crashes.
4. **Fallback is the app's job.** The terminal answers a reliable capability query; the app decides
   whether to send TGP or degrade to sixel/ASCII/text. The terminal never auto-degrades.
5. **Compact on the wire.** Binary encoding, diff-style updates, instancing — designed to survive SSH/PTY
   bandwidth.
6. **Don't re-spec what works — generalize it.** The `revision` / `asset_revision` split (RGP commit
   96da70a) is the template for TGP's dirty-tracking.

---

## 3. Goals / non-goals

### 3.1 Goals (v1 scope)

- Clean-break `tgp;` namespace + compact binary framing.
- Capability negotiation (always-answered, per-feature flags, versioned).
- Retained scene graph: typed nodes, parent transforms, instancing, per-node visibility + alt-text.
- Multiple viewports: inline (flows with text) and pinned (fixed), app's choice; offscreen depth-aware
  compositing; terminal-driven reflow on resize/scroll.
- One default material (zero config); registerable PBR/material + lights (opt-in).
- Interactivity: opt-in terminal-side explore controller; opt-in picking + click/hover event reports.
- Assets inline only (glTF/GLB bytes in the protocol); **no file loading**.
- RGP adapter so existing `ratty;g;` apps keep working.
- Structured error replies; resource caps; bounds-checked asset parsing.

### 3.2 Non-goals (explicitly deferred — see §16)

- Sandboxed file/path loading (kept out entirely "for now").
- Image-based lighting (IBL), real-time shadows, post-processing stacks.
- HDR / wide-gamut surface signaling.
- A declarative client library (the wire is imperative-patch; a declarative layer can sit on top later).
- Full skeletal animation polish (skinning is in the model; production-grade rig support is staged).
- Text rendered as a 3D scene node ("labels in 3D space") — possible later via a `text` node kind.

---

## 4. Relationship to RGP

TGP and RGP feed **one** internal scene model and **one** renderer.

```
  app bytes on the PTY
        │
        ├── ESC _ ratty;g;… ESC \   ──►  RGP adapter  ─┐
        │                                              ├──►  TGP scene model ──► renderer
        └── ESC _ tgp;…              ──►  TGP parser  ──┘
```

- **Namespace.** Native TGP uses the `tgp;` APC prefix. RGP's `ratty;g;` prefix is still recognized and
  demultiplexed _(today at term.rs:3371–3422 (grounding))_.
- **Adapter mapping.** `r` (register) → `asset.add`; `p` (place) → `node.upsert` (flat, no parent);
  `u` (update) → a `patch` of property sets; `d` (delete) → `node.remove`/`scene.clear`. RGP placements
  become flat (parent-less) nodes with non-instanced draws — i.e. exactly today's behavior.
- **Compatibility carve-out for security.** RGP frames keep RGP semantics, **including** permissive
  `path=` loading (preserving the documented ecosystem-compat decision). **Native TGP has no path feature
  at all.** This lets TGP be safe-by-default without changing RGP behavior.
- **Capabilities.** The TGP capability reply is separate from RGP's `s`-query reply _(reply.rs:34–39
  (grounding))_; an app detects TGP specifically (see §7).

---

## 5. Architecture overview

```
┌──────────────────────────────────────────────────────────────────────┐
│ toastty terminal                                                       │
│                                                                        │
│  PTY bytes ─► escape parser ─► [tgp framer]    [ratty;g; framer]       │
│                                     │                 │                │
│                                     ▼                 ▼                │
│                              TGP op decoder      RGP adapter           │
│                                     └───────┬─────────┘                │
│                                             ▼                          │
│                                   ┌───────────────────┐                │
│                                   │   Scene model     │  (retained)    │
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
│        └──────────  event reports ──► PTY (stdin to app)               │
└──────────────────────────────────────────────────────────────────────┘
```

Crates (current layout, _grounding_): protocol/model in `crates/toastty-graphics/src/rgp/*`
(`scene.rs`, `operation.rs`, `handler.rs`, `parser.rs`); rendering in `crates/toastty-render/src/rgp/*`
(`pipeline.rs`) + `crates/toastty-render/shaders/rgp.wgsl`. TGP will add sibling `tgp` modules; the scene
model and renderer are generalized rather than forked, with the RGP path retained behind the adapter.

---

## 6. The scene model

A single retained scene per terminal session, plus a set of viewports that look into it.

### 6.1 Data structures (conceptual)

```rust
struct Scene {
    assets:     Map<AssetId,    Asset>,      // meshes, textures, skins — uploaded once
    materials:  Map<MaterialId, Material>,   // default material is implicit id 0
    nodes:      Map<NodeId,     Node>,       // the graph
    roots:      Set<NodeId>,                 // parent-less nodes
    viewports:  Map<ViewportId, Viewport>,
    clips:      Map<ClipId,     AnimationClip>,

    asset_revision:  u64,   // bumps on asset/material/skin upload changes  → re-upload GPU buffers
    scene_revision:  u64,   // bumps on transform/visibility/instance/material-ref changes → re-render only
    dirty:           DirtySet, // per-node + per-viewport dirty flags (see §6.5)
}

struct Node {
    id:        NodeId,        // app-assigned string (see §8.4)
    parent:    Option<NodeId>,
    trs:       Trs,           // local transform (translate / rotate quat / scale)
    kind:      NodeKind,
    visible:   bool,          // cheap show/hide without delete+re-add
    alt:       Option<String>,// accessibility caption (see §13)
}

enum NodeKind {
    Group,                                       // transform-only container
    Mesh   { asset: AssetId, material: MaterialId, tint: Rgba },
    Instanced { asset: AssetId, material: MaterialId,
                instances: InstanceBuffer },     // see §6.4
    Light  { light: Light },                     // see §11.3
    Camera { camera: Camera },                   // see §10
    // future: Text { … }  (labels in 3D space) — deferred
}
```

### 6.2 Transforms & hierarchy

- Each node has a **local** TRS. **World transform = product of the parent chain** (root → … → node).
- "Move the parent, children follow" — the headline reason for the graph. A robot arm is a chain of
  `Group`/`Mesh` nodes; rotating the shoulder cascades to the hand with **one** patch op, not N.
- World transforms are computed lazily on the **dirty subtree** before each render (see §6.5), never the
  whole tree unless the whole tree changed.

### 6.3 Why a graph (recap of the decision)

Use-case analysis identified three dynamism classes: (1) **inspect** (rigid model + camera orbit), (2)
**bulk data-viz** (many independent moving instances), (3) **articulated/animated** (correlated hierarchy).
Transform propagation pays off only for class 3. TGP commits to the full graph **now** because (a)
articulated/skeletal animation is a day-one goal, and (b) glTF's animation + skinning are *defined* over a
node tree — flattening on import is lossy for exactly those assets. Classes 1 & 2 pay almost nothing: a
flat scene has trivial 1-long parent chains, and bulk data uses instancing (§6.4), not propagation.

### 6.4 Instancing

One registered mesh, drawn many times in **one** GPU draw call — the fix for RGP's "one draw per placement"
_(pipeline.rs:287)_ and for `molecule-viewer`'s "one asset id per element type" hack.

- An `Instanced` node carries an **instance buffer**: a packed array of per-instance records
  `{ transform: Trs|mat4, tint: Rgba, material: Option<MaterialId> }`.
- Renderer uploads the buffer to an instance vertex buffer and issues `draw_indexed(indices, 0..N)` with
  per-instance attributes (`@location` instance-step vertex attributes; `instance_index` for picking, §12.4).
- **Static instances:** uploaded once (bumps `asset_revision` only when the buffer identity/size changes).
- **Dynamic/bulk instances** (class 2 data-viz): the buffer can be re-uploaded per frame via a
  `node.instances` op carrying only the changed buffer — this is the point-cloud / live-chart path.
- **Per-instance material** is optional and limited in v1 to a tint + a single shared material; full
  per-instance material handles (material arrays / bindless) are deferred.

### 6.5 Dirty-tracking (generalizing 96da70a)

Three independent notions of "changed", so we never do more GPU work than necessary:

| Change | Bumps | Effect |
|---|---|---|
| Asset/material/skin upload added or replaced | `asset_revision` | Re-upload GPU buffers/textures |
| Node transform / visibility / tint / instance buffer | `scene_revision` + node dirty flag | Recompute dirty subtree world matrices; re-render |
| Viewport anchor/size/camera/settings | viewport dirty flag | Recompute pixel rect; re-render (or just re-blit cached layer if only moved — §10.4) |

This directly extends the existing split (transform spam stays uniform-only and never triggers re-upload),
adding **per-node** and **per-viewport** granularity so a 10k-node scene with one moving node recomputes one
subtree.

---

## 7. Capability negotiation

The single thing the terminal *must* do well, because the app's entire fallback strategy depends on it
(principle 4). The historically flaky part of terminal protocols is "I asked and heard nothing — is it
unsupported, or just slow?" TGP solves this explicitly.

### 7.1 Query / reply

```
app → term:   ESC _ tgp;q;v=2 ESC \           # "do you speak TGP? I support up to v2"
term → app:   ESC _ tgp;r;v=2;
                 feat=geom,graph,instance,material,pbr,light,pick,event,explore,anim,skin,binframe;
                 max_verts=4000000;max_instances=1000000;max_vram_mb=512;
                 max_msg_mb=64;enc=bin,b64;color=srgb ESC \
```

- **Always-answered.** The app sends the TGP query **together with** a query every terminal answers —
  primary Device Attributes (`ESC [ c`). If DA1 replies but no `tgp;r` arrives, the app concludes "no TGP"
  with certainty and never hangs. (This pairing is documented as the recommended detection handshake.)
- **Per-feature flags** (`feat=`). An app can degrade *partially*: e.g. draw geometry but skip `pick`
  if a terminal lacks it. Missing flag = feature absent.
- **Versioned.** App states its max version; terminal replies with the negotiated version. Unknown future
  versions degrade to the highest mutually understood one.
- **Limits.** The reply advertises hard caps (verts, instances, VRAM, message size) so the app can
  pre-trim instead of getting silently rejected.
- **Encoding.** `enc=` lists supported payload encodings (raw binary frames and/or base64; see §8.3).

### 7.2 Versioning & forward-compat

- Unknown ops/fields inside a patch are **skipped**, not fatal (RGP already relies on field-skipping
  forward-compat). A negotiated version gates whole feature sets; field-skipping handles minor additions.

---

## 8. The wire protocol

### 8.1 Goals

Compact (binary, not text key=value or JSON), binary-safe through the PTY, diff-friendly, and able to carry
large asset blobs inline.

### 8.2 Framing

Two frame shapes, chosen per message:

**(a) Control frames** — small, must survive every transport (tmux, ssh, screen). Text APC, human-debuggable:

```
ESC _ tgp ; <type> ; <k=v ; …> ESC \
```

Used for: capability query/reply (`q`/`r`), event reports (`e`), error replies (`x`), acks (`a`), and any
op small enough to not need binary. Values that need bytes use base64.

**(b) Binary frames** — bulk scene data (patches with geometry/instance/texture payloads). A **text header
announces an exact byte length, then that many raw bytes follow**, read outside escape-scanning:

```
ESC _ tgp ; p ; txn=42 ; enc=bin ; len=20512 ESC \  <20512 raw bytes of CBOR>
```

- **Why length-prefixed raw bytes:** true compactness (no base64 33% tax) and zero ambiguity. The parser,
  on seeing a `tgp` binary header, switches to "consume exactly `len` bytes" mode — bypassing the C0/ESC
  scanning that would otherwise corrupt binary data. This is the key change to the APC pre-scanner
  _(parser.rs:3–130 / term.rs demux (grounding))_: add a binary-length read state.
- **Robustness fallback:** if a transport can't carry raw bytes (some tmux passthrough configs), the app
  negotiates `enc=b64` (from §7.1) and sends `enc=b64;len=<encoded-len>` with base64 payload + `ST`
  terminated as usual. The terminal accepts both; the app picks based on `enc=` in the capability reply and
  whether it's behind a multiplexer.

### 8.3 Payload encoding

- Recommended v1 semantic encoding: **CBOR** (compact, self-describing, schema-less, easy to evolve) for
  the op structure, with large binary sub-blobs (vertex buffers, instance buffers, texture bytes, GLB) as
  CBOR byte strings.
- **Open:** a fully custom TLV/flatbuffer layout would be marginally smaller and zero-copy, at higher
  implementation cost. CBOR is the pragmatic v1 choice; the framing (§8.2) is encoding-agnostic, so this can
  change without a framing break. (See §16.)

### 8.4 Identity & addressing

- **Node ids: app-assigned strings** (UTF-8, bounded length, e.g. ≤ 64 bytes). Human-friendly
  (`"wheel_fl"`), and they double as the handles in event reports (§12.5) and in patch addressing. The app
  picks them; the terminal treats them opaquely.
- **Asset / material / clip ids: numeric handles** (`u32`). Compact for the hot upload path; assigned by
  the app (like RGP's `u32` asset ids today).
- **Namespace:** one global namespace per session for the scene. Viewports reference a camera **node** and
  an optional subtree **root** node by id.
- **Collision rule:** `upsert` on an existing node id mutates it; `add` on an existing asset id is an error
  (assets are immutable once uploaded — replace via remove+add, which bumps `asset_revision`).

### 8.5 Message taxonomy

| Type | Frame | Meaning |
|---|---|---|
| `q` / `r` | control | capability query / reply (§7) |
| `p` | binary | **patch**: a transactional list of ops (§8.6) |
| `vp` | control/binary | viewport create/update/destroy (§10) |
| `sub` | control | subscribe/unsubscribe to input events for a viewport/nodes (§12) |
| `anim` | control | animation playback control (opt-in) (§14) |
| `e` | control | **event report** terminal → app (§12.5) |
| `x` | control | **error reply** terminal → app (§15.3) |
| `a` | control | ack (optional, if app subscribed) for a committed `txn` |

### 8.6 The patch (the core mutation message)

```
# Conceptually (CBOR on the wire); shown as readable pseudo-JSON:
{ txn: 42, ops: [
    { do: "asset.add",     id: 7, fmt: "glb",  data: <bytes> },
    { do: "material.add",  id: 3, model: "pbr", base: [..], metallic: 0.9, rough: 0.3,
                           tex_base: 8 /* asset id of a texture */ },
    { do: "node.upsert",   id: "car",      trs: {...} },
    { do: "node.upsert",   id: "wheel_fl", parent: "car", mesh: 7, material: 3 },
    { do: "node.instances",id: "atoms",    mesh: 5, xforms: <bytes>, tints: <bytes> },
    { do: "node.visible",  id: "ghost",    visible: false },
    { do: "node.remove",   id: "old" },
] }
```

**Semantics:**

- **Atomic / transactional.** All ops in a patch apply together or not at all. Success bumps
  `scene_revision` **once** (no mid-frame tearing — the "transactional frame" feature for free). Any op
  error rejects the **whole** txn and emits an `x` error citing the offending op index (§15.3); the scene
  is unchanged.
- **Ordering.** Patches apply in receive order. `txn` is an app-chosen correlation id echoed in acks/errors.
- **Idempotent addressing.** `node.upsert` creates or mutates; only the provided fields change (sparse
  update — generalizes RGP's `u` verb). Omitted fields are preserved.
- **Bulk fields are byte blobs.** `xforms`/`tints`/vertex data ride as binary sub-blobs in the same frame.

### 8.7 Chunking

Large assets exceeding a single frame are chunked, generalizing RGP's `more=1|0` _(handler.rs:26–31,
189–203 (grounding))_: an `asset.add` may span multiple binary frames keyed by `id` with a `more` flag;
the terminal reassembles before parsing. Per-asset and per-session byte caps apply throughout (§15.2).

---

## 9. (reserved)

---

## 10. Viewports & compositing

A **viewport** is a window into the scene, anchored to terminal cells. Many can exist at once.

### 10.1 Viewport object

```rust
struct Viewport {
    id:        ViewportId,
    anchor:    Anchor,          // Inline | Pinned  (see §10.2)
    cells:     CellRect,        // position + size in cells (col,row,cols,rows)
    camera:    NodeId,          // a Camera node in the scene
    root:      Option<NodeId>,  // subtree to render; default = whole scene
    z:         i32,             // order among overlapping viewports
    clear:     Option<Rgba>,    // background; None = transparent (composite over text)
    render:    RenderOpts,      // msaa, tone-map operator, theme-tint (see §11)
    explore:   Option<ExploreOpts>, // opt-in camera controller (§12.3)
    clip_to_scroll_region: bool,    // don't bleed across panes (default true)
}

enum Anchor {
    Inline { line: ScrollbackLine, col: u16 }, // flows with text; scrolls away
    Pinned { col: u16, row: u16 },             // fixed screen region; text scrolls under/beside
}
```

### 10.2 Inline vs pinned (app chooses per viewport)

- **Inline** — occupies real cells in the text grid, like an image embedded in a notebook/chat log. It
  scrolls with the surrounding text and scrolls off-screen (state retained, not rendered while off-screen).
  Implemented via a **Unicode-placeholder-style cell binding** (à la kitty's Unicode placeholders) so the
  region flows naturally through line-based apps (vim, less, tmux) that only understand cells.
- **Pinned** — fixed to a screen region; text scrolls underneath/around it. For dashboards, monitors, HUDs,
  inspectors.

### 10.3 Cell → pixel mapping & reflow

- Cell rects map to pixel rects using cell metrics, generalizing _(pipeline.rs:196–212 (grounding))_.
- **Terminal-driven reflow (deliberate choice).** On `SIGWINCH` / font-size change, the terminal recomputes
  each viewport's pixel rect from its cell rect **automatically** — the app does **not** re-issue placements
  (the kitty model). This is the "terminal-native" feel: viewports reflow like text. If the app wants to
  react (e.g. swap LOD), it can subscribe to a `resize` event (§12), but it isn't required to.

### 10.4 Compositing — "stickers with depth"

Each viewport renders to its **own offscreen target carrying color + depth** ("a sticker that knows how far
back each dot is"), then composites into the main framebuffer with a **depth test against the shared
text/scene depth**.

- **Text ↔ 3D interleaving is preserved.** The text plane sits at a known depth (generalizing the NDC
  z=0.5 plane, _lib.rs:1539–1553 (grounding)_). A solid object can be partly in front of text (near side
  covers it) and partly behind (far side hidden) — **exact for opaque geometry**.
- **Smooth scroll/resize.** Because each viewport is its own layer, scrolling/moving a viewport that hasn't
  changed content is a **re-composite of the cached layer** (re-run the depth test against the new text
  positions), not a re-render. Re-render happens only on content/camera/size change (driven by the dirty
  flags, §6.5).
- **Transparency caveat (honest).** A semi-transparent dot (glass, smoke, anti-aliased edges) overlapping
  text must pick a single depth for the test, so translucent-over-text is **approximate**, not pixel-perfect.
  Opaque geometry is exact. This is the standard trade every renderer makes.
- **Z-order.** Overlapping viewports composite by `z`. `clip_to_scroll_region` prevents a viewport from
  bleeding across tmux panes / scroll regions.

### 10.5 One scene, many cameras (decision)

TGP uses **one shared scene** rendered by **N viewports**, each via its own `Camera` node — not N
independent scenes. This shares asset/material uploads (cheap) and matches "register once, view many ways."
A viewport may set `root` to a subtree for isolation when desired.

---

## 11. Rendering pipeline

### 11.1 Per-viewport flow

```
for each visible viewport (dirty or first-draw):
    recompute dirty world matrices for its root subtree
    render subtree → offscreen {color, depth} target
        - default-material pipeline for default-material nodes
        - PBR pipeline for nodes referencing registered materials
        - instanced draw for Instanced nodes (one draw, 0..N)
    (if pick subscribed) render subtree → pick target (node-id/instance-id colors)  [§12.4]
    apply per-viewport tone-map (if PBR/linear) + msaa resolve
then:
    composite all viewport layers into the framebuffer by z, depth-tested vs text plane
```

### 11.2 Materials

- **Default material (implicit `MaterialId 0`).** A clean **matte lit** look: Lambertian diffuse + ambient
  (+ a soft hemispherical term for legibility at small sizes), `base × per-node tint × brightness`
  (reusing RGP's existing color/brightness). Zero config; legible when tiny. This generalizes `rgp.wgsl`.
- **Registered materials (opt-in).** `material.add` with `model: "pbr"` → metal-roughness core: baseColor,
  metallic, roughness, normal, emissive, occlusion; optional texture maps (texture = an `asset` of an image
  type; KTX2/Basis support is a later add). A separate PBR pipeline; nodes opt in by referencing the
  material id. PBR is linear-light, so PBR viewports get a tone-map operator (`render.tone_map`:
  none/Reinhard/ACES) before composite.
- **Theme-tint (optional `render` flag).** A viewport may request that the default look adopt the user's
  terminal palette so 3D blends with the theme rather than looking like a foreign overlay. Off by default.

### 11.3 Lighting

- **Implicit default lighting.** With zero lights registered, the default material is lit by a built-in key
  + ambient so objects look good out of the box (removes RGP's "hardcoded sun" as the *only* option, but
  keeps a sensible default).
- **Registered lights as nodes (opt-in).** `Light` nodes (KHR_lights_punctual style: directional / point /
  spot, with color + intensity). Because lights are nodes, they **inherit transforms and animate** (a
  headlight parented to a car). Forward rendering; a per-viewport light-count cap (advertised in caps).
- **Deferred:** shadows, IBL/environment maps, area lights.

### 11.4 Anti-aliasing

Tiny per-cell viewports alias badly, so per-viewport MSAA (resolve before composite) matters more here than
in full-screen rendering. `render.msaa` is per-viewport; default a modest sample count.

---

## 12. Interactivity (the headline)

Off by default (principle 1: the app drives). The app opts into two independent capabilities per viewport:
**explore** (terminal-side camera control) and **events** (clicks/hover routed to the app).

### 12.1 Model

```
                 ┌─────────── app subscribes (sub) per viewport ───────────┐
 user input ──► interaction router                                          │
   (mouse)        │  if explore enabled  → terminal updates camera node     │  (opt-in)
                  │                         (takes control of camera only)   │
                  │  if events subscribed→ pick → emit `e` report to app ────┘
                  │  if raw mode         → forward raw input events to app
```

### 12.2 Opt-in, app-in-control (recap of decision)

By default the app issues all transforms and receives no input. The app explicitly turns on the pieces it
wants; only then does the terminal take that piece of control. Three levels, independently selectable:

1. **Explore** — terminal-driven orbit/zoom/pan (§12.3).
2. **Events** — terminal reports semantic hits (`click`/`hover`/`enter`/`leave`) on subscribed nodes (§12.5).
3. **Raw** — terminal forwards raw pointer events for the viewport; the app drives everything itself
   (escape hatch).

### 12.3 Explore controller (opt-in)

```rust
struct ExploreOpts {
    orbit:   bool,         // drag to rotate
    zoom:    bool,         // scroll/pinch to zoom
    pan:     bool,         // drag to pan
    lock_axes: AxisMask,   // e.g. lock to Y for turntable-only
    zoom_min: f32, zoom_max: f32,
    auto_spin: Option<{ axis: Vec3, speed: f32 }>, // replaces RGP's hardcoded spin (scene.rs:27)
    damping: f32,
    initial: Option<CameraPose>,
}
```

- **Crucial nuance — explore moves the _camera node_, not the model.** The terminal mutates the viewport's
  `Camera` node transform, never the model's nodes. The app's model transforms stay app-owned (principle 1).
  This is why "the terminal handles tumbling" doesn't violate "the app issues transforms": they touch
  *different* nodes.
- **Sync-back (optional).** If the app subscribes to `camera` events, the terminal reports camera pose
  changes so the app can persist/sync them. Otherwise the camera lives terminal-side.
- **Auto-spin** is just an explore option, not a default (RGP's always-on spin is gone).

### 12.4 Picking (implementation)

GPU **color-ID picking**: render the subtree to a pick target where each pixel encodes `node-id`
(+ `instance-index` for `Instanced` nodes, via `instance_index`). On a pointer event inside a viewport, read
back the single pixel under the cursor → resolve to node/instance. O(1), pixel-perfect, scales to huge
scenes; no CPU ray-casting.

- Pick target rendered only when events are subscribed for that viewport (no cost otherwise).
- v1 reports node id + instance index + world-space hit point (from depth). UV/barycentric reporting needs
  interpolated attributes in the pick pass and is **deferred** (heavier).

### 12.5 Event report wire format (terminal → app)

Mode-gated: only emitted for viewports/nodes the app subscribed to (`sub`), so a non-interactive app /
dumb reader never sees them. Control frame on stdin:

```
ESC _ tgp;e;vp=2;ev=click;node=O1;inst=5;btn=1;x=0.31;y=0.62;wx=1.4;wy=0.0;wz=-2.1 ESC \
ESC _ tgp;e;vp=2;ev=enter;node=bond_3 ESC \
ESC _ tgp;e;vp=2;ev=leave;node=bond_3 ESC \
ESC _ tgp;e;vp=2;ev=resize;cols=20;rows=12 ESC \   # if subscribed (§10.3)
```

- `ev` ∈ `click | dblclick | down | up | hover | enter | leave | drag | wheel | camera | resize`.
- `node` is the picked node id (string), `inst` the instance index when applicable; `x,y` viewport-local
  normalized coords; `wx,wy,wz` world-space hit point.
- **Subscription granularity.** `sub` selects a viewport and either "all nodes" or a list of node ids, and
  which `ev` types to receive — so an app gets only the events it asked for.
- **Disambiguation.** Reports are TGP-namespaced APC, distinct from SGR mouse; an app reading stdin routes
  `ESC _ tgp;e;…` to its TGP handler. Normal mouse reporting is unaffected unless the app enabled it.

### 12.6 Worked interaction loop

1. App: `sub vp=2 nodes=* ev=click,hover`.
2. User clicks an atom. Terminal: pick pass → pixel → `node=O1, inst=5`.
3. Terminal → app: `ESC _ tgp;e;vp=2;ev=click;node=O1;inst=5;… ESC \`.
4. App reacts (its choice): prints "Oxygen, 16.00 u", or sends a `patch` tinting `O1` highlighted, or opens
   a menu. The terminal did **not** decide what a click means — it only reported it.

---

## 13. Accessibility

- Every node (and the scene) may carry `alt` text. A `Mesh` for a molecule carries
  `alt: "caffeine molecule, C8H10N4O2"`.
- The terminal surfaces `alt` to assistive tech and as a cell-level fallback caption when a viewport can't
  be presented visually. Because alt travels **in the scene** (app-provided) but is surfaced by the
  **terminal** (which owns AT integration), this is the one place the terminal adds value beyond rendering —
  consistent with "fallback is the app's job" for *visual* fallback, while AT exposure needs terminal help.
- No existing terminal graphics protocol carries structured alt-text; this is a cheap lead.

---

## 14. Animation

- **Authored clips travel with the asset.** glTF animation channels (TRS keyframes; STEP/LINEAR/CUBICSPLINE)
  are imported and stored as `AnimationClip`s referencing node ids.
- **App-driven by default.** The app can animate by streaming transform patches each frame (principle 1).
- **Terminal-side playback is opt-in.** `anim;play;clip=…;node=…;loop=1;speed=1.0` (plus `pause`/`seek`/
  `stop`). When the app opts in, the terminal advances the clip clock and updates node transforms itself —
  taking control of those nodes, exactly like the explore controller takes the camera. Removes RGP's
  hardcoded spin _(scene.rs:27)_ in favor of explicit, app-controlled motion.
- **Skinning** (joints + skin matrices, vertex skinning in-shader) is in the model; production polish is
  staged (§16).

---

## 15. Security & robustness

### 15.1 Threat model

TGP bytes arrive on the PTY from **untrusted sources** (a `curl`, a `cat`'d file, a log line, a web page
rendered in a pager). The protocol must be safe to *receive*, not just to *send*.

### 15.2 Resource caps

Advertised in the capability reply (§7.1) and enforced:

- Max vertices/indices per asset; max total VRAM; max nodes; max instances per buffer; max texture
  dimensions; max message size; max chunk reassembly size (generalizing RGP's 64 MiB/256 MiB caps,
  _handler.rs:26–31 (grounding)_); max decode/parse time (guard against decompression bombs).
- Exceeding a cap → structured error (§15.3), never a crash or unbounded allocation.

### 15.3 Structured errors (vs RGP's silent drop)

RGP silently drops malformed input _(term.rs:3417 (grounding))_. TGP replies (when the app negotiated error
reporting) with:

```
ESC _ tgp;x;txn=42;op=3;code=cap_exceeded;detail=max_verts ESC \
ESC _ tgp;x;txn=43;code=parse_error;detail=accessor_oob ESC \
```

- Errors are **per-txn** and cite the offending op index; the txn is rejected atomically (scene unchanged).
- Errors are only emitted to apps that opted in (avoid spamming dumb readers).

### 15.4 No file access

Native TGP has **no `path=`** and no file/temp/shared-mem loading of any kind — assets are always inline in
the protocol bytes. This removes the entire file-access attack surface (the permissive `path_resolver`
_(path_resolver.rs:45–77)_ applies to RGP-via-adapter only). A sandboxed path-load feature may be added
later (§16) but is out of scope now.

### 15.5 Hardened parsing

- glTF/GLB parsing reads the full mesh set (not just first-primitive like RGP today) with **bounds checks**
  on every accessor/index/buffer-view (reject OOB, NaN/inf where invalid, overlong strings).
- A **fuzz harness** targets both the wire framer/decoder and the asset parser (terminals have a history of
  crashing on malformed graphics data).
- **Capability gating** for risky verbs: input-event reporting and (future) any path-like feature are
  opt-in per session.

---

## 16. Open questions / deferred

| Item | Disposition |
|---|---|
| Exact binary encoding (CBOR vs custom TLV/flatbuffer) | v1 = CBOR; framing is encoding-agnostic so it can change later. |
| Final op naming / field schema | Strawman in §8; to be pinned during writing-plans. |
| Declarative client library (app sends desired state; lib diffs → patches) | Additive on top of the imperative wire; later. |
| Per-instance material handles (material arrays/bindless) | v1 = per-instance tint only. |
| UV/barycentric in pick reports | Deferred; v1 = node + instance + world point. |
| Shadows, IBL, post-processing, HDR/wide-gamut | Deferred (fidelity phase). |
| Skeletal animation production polish | Model supports it; staged. |
| Text as a 3D scene node (labels in 3D) | Possible later via a `text` node kind. |
| Sandboxed/allowlisted path loading | Out for now (§15.4); revisit with a real threat-modeled design. |
| tmux passthrough specifics + binary vs base64 negotiation defaults | Validate against real tmux/ssh configs during implementation. |

---

## 17. Suggested phasing (non-binding; for writing-plans)

1. **Foundation.** `tgp;` framing (control + binary-length frames), CBOR decode, capability query/reply with
   always-answered pairing, scene model (graph + dirty-tracking), inline GLB assets (hardened, full mesh),
   default material, RGP adapter, one viewport at parity with today's renderer on the new wire.
2. **Differentiators.** Multiple viewports (inline + pinned) with offscreen depth-aware compositing +
   terminal-driven reflow; GPU instancing; picking + event reports + subscriptions; explore controller.
3. **Materials & light.** Registered PBR materials + textures; punctual lights as nodes; tone-mapping;
   per-viewport MSAA; theme-tint.
4. **Animation.** glTF clip import; opt-in playback verbs; (then) skinning.
5. **Hardening.** Fuzz harness, caps enforcement, structured errors everywhere, accessibility surfacing.

---

## 18. Worked examples (end-to-end)

### 18.1 Inline molecule with instancing + click-to-identify

```
# 1) detect (paired with DA1 so it never hangs)
app → term:  ESC[c   ESC _ tgp;q;v=2 ESC \
term → app:  ESC[?…c  ESC _ tgp;r;v=2;feat=…,instance,pick,event,explore;… ESC \

# 2) upload one sphere mesh + one cylinder mesh (inline GLB), build scene
app → term:  ESC _ tgp;p;txn=1;enc=bin;len=… ESC \ <CBOR: [
                {do:asset.add, id:1, fmt:glb, data:<sphere bytes>},
                {do:asset.add, id:2, fmt:glb, data:<cylinder bytes>},
                {do:node.upsert, id:"mol", trs:{...}},                 # group
                {do:node.instances, id:"atoms",  parent:"mol", mesh:1,
                   xforms:<N transforms>, tints:<N colors>},           # all atoms, ONE draw
                {do:node.instances, id:"bonds",  parent:"mol", mesh:2,
                   xforms:<M transforms>, tints:<M colors>},
              ]>

# 3) place an INLINE viewport on the current line, with turntable explore
app → term:  ESC _ tgp;vp;id=2;anchor=inline;cells=0,12,40,16;camera="cam";
                  explore=orbit,zoom;lock=y ESC \
app → term:  ESC _ tgp;p;txn=2;… ESC \ <CBOR:[{do:node.upsert,id:"cam",camera:{...}}]>

# 4) subscribe to clicks
app → term:  ESC _ tgp;sub;vp=2;nodes=*;ev=click ESC \

# 5) user drags → terminal spins the camera node itself (no app round-trip)
# 6) user clicks an atom
term → app:  ESC _ tgp;e;vp=2;ev=click;node="atoms";inst=6;wx=1.2;wy=0;wz=-0.4 ESC \
# 7) app decides what that means
app → term:  (prints) "Oxygen (O), 16.00 u"
app → term:  ESC _ tgp;p;txn=3;… ESC \   # e.g. re-tint instance 6 to highlight
```

Scrolling the terminal: the inline viewport scrolls away with the text (its cached layer is re-composited
at the new position; no re-render). Resizing the window: the terminal recomputes its pixel rect from the
cell rect automatically; the app does nothing.

### 18.2 Pinned rotating inspector

```
app → term:  ESC _ tgp;vp;id=9;anchor=pinned;cells=80,0,40,24;camera="cam9";
                  explore=orbit,zoom,pan;clear=#101216 ESC \
```

Stays in the right-hand region while logs scroll on the left. The app streams `node.upsert` transforms only
when *it* wants motion; the user can still orbit (terminal-side) independently.

### 18.3 Articulated robot arm (hierarchy + animation)

```
app → term:  ESC _ tgp;p;txn=1;… ESC \ <CBOR:[
                {do:asset.add, id:1..4, …},                          # segments
                {do:node.upsert, id:"base"},
                {do:node.upsert, id:"shoulder", parent:"base",  mesh:1},
                {do:node.upsert, id:"elbow",    parent:"shoulder", mesh:2},
                {do:node.upsert, id:"wrist",    parent:"elbow", mesh:3},
                {do:node.upsert, id:"hand",     parent:"wrist", mesh:4},
             ]>
# rotate the shoulder → elbow/wrist/hand all follow, ONE op:
app → term:  ESC _ tgp;p;txn=2;… ESC \ <CBOR:[{do:node.upsert,id:"shoulder",trs:{rot:…}}]>
# or hand control to the terminal:
app → term:  ESC _ tgp;anim;play;clip=0;node="base";loop=1;speed=1 ESC \
```

### 18.4 Live point cloud (bulk data-viz)

```
# per frame, re-upload only the instance buffer (no graph changes, no re-upload of the mesh):
app → term:  ESC _ tgp;p;txn=N;enc=bin;len=… ESC \
               <CBOR:[{do:node.instances, id:"points", mesh:1, xforms:<bulk>, tints:<bulk>}]>
```

No hierarchy needed here — this is the class-2 path: instancing + diff frames, not transform propagation.

---

## 19. Glossary

- **Node** — an entry in the scene graph (mesh/light/camera/group), with a local transform and optional
  parent.
- **Instanced node** — a node that draws one mesh many times in a single GPU draw via a per-instance buffer.
- **Viewport** — a cell-anchored window rendering the scene from a camera; inline or pinned.
- **Patch** — a transactional list of mutation ops; the core update message.
- **Sticker** (informal) — a viewport's offscreen color+depth layer, composited into the grid with a depth
  test so it interleaves with text.
- **Adapter** — the internal translator from RGP `ratty;g;` frames into the TGP scene model.
```
