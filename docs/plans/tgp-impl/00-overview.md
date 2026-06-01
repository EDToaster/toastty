# TGP implementation plan — overview & index

TGP (Toastty Graphics Protocol) is a clean-break, retained-mode 3D terminal-graphics
protocol for the toastty terminal (Rust workspace). This document indexes the five-phase
implementation plan, records the validated cross-phase dependency ordering, names the
three GPU-free test seams, and states the protocol-surface coverage guarantee.

**Authoritative sources.** The per-section design slices `sections/s05.md … s15.md`, the
reconciliation spine `../tgp-reconciliation.md` (global invariants **G1–G10**), and the
**authoritative addendum `../tgp-reconciliation-addendum.md` (G11–G12)** are the design
of record; where the spine disagrees with a slice, the spine wins, and **where the addendum
disagrees with the spine or a slice, the addendum wins** (it is explicitly authoritative). There
is **no RGP compatibility/adapter** (G8 clean break) — TGP lives in greenfield `tgp/*` trees in
`toastty-graphics`, `toastty-render`, and `toastty-term`; the legacy `rgp/*` path is untouched.

**Addendum G11/G12 (binding on all phases).** The v1 isolation model is a **private per-token id
map** (G11): each handshaked token owns a physically-disjoint `(id → object)` map, so cross-token
addressing is **unrepresentable by construction**. The consequences are applied uniformly across
P1–P5: a cross-token id "collision" is **coexistence** (two distinct `{tok,id}` entries, never an
error); a cross-token handle/patch ref is **`bad_ref`** with **no existence oracle** (byte-for-byte
identical to absent-everywhere); a cross-token viewport `camera`/`root` is **late-bind default**;
a token-requiring frame with no valid handshake is **`bad_token`** (a well-formed tokenless `tgp;p`
is a silent drop). Therefore **`denied` is RESERVED** — no reachable ingress trigger, carried only
by the closed-set encoder round-trip. G12 fixes one error-code reachability taxonomy across the
phases: **24 of 26 codes are REACHABLE, 2 are RESERVED** (`msg_too_large`, `denied`); `unknown_op`
is REACHABLE via the closed control-frame anim-verb locus (an `anim` verb ∉ {play,pause,seek,stop}),
and RESERVED for patch `do:` ops (which skip unknowns per §7.3). An over-`max_msg_mb` breach is
canonically **`cap_exceeded;detail=max_msg`** (G12.1, `msg_too_large` reserved); and a node-level tint on an `Instanced` node is **`kind_conflict;detail=instanced_node_tint`**
(G12.4 — the base-spine `instanced_node_tint→bad_param` synonym is struck). P1-T16 owns the closed
26-variant enum + emitter; P1 and P5 agree on exactly this partition.

This overview is a coherence-validated index. The cross-phase critic findings (dependency
graph, surface partition) are summarized at the end; the per-phase files carry the full task
deliverables, tests, and review logs.

---

## The five phases (one-line goals)

| Phase | File | Goal |
|---|---|---|
| **P1 — Foundation** | `P1.md` (T1–T21) | Stand up the entire CPU-side spine end to end: APC binary/control framer + `apc_end -> ApcEnd` back-channel, CBOR patch decode, always-answered capability handshake (`tgp;r` before DA1, per-token namespace), retained per-token scene model (`u64` revisions + dirty flags), inline GLB assets, the PINNED default material, the injectable clock, the closed `tgp;x`/`tgp;a` enum + emitters, and ONE viewport at compositing parity. |
| **P2 — Differentiators** | `P2.md` (T1–T38) | The features that make TGP more than "GLB in a cell": multiple cell-anchored + pinned viewports with offscreen color+depth targets and depth-aware compositing, terminal-driven reflow, GPU instancing render, the full interactivity spine (CPU picking, `tgp;e` events, `sub` subscriptions, explore camera controller, F4 input routing), device-loss recovery, and RIS/DECSTR teardown for viewport/interaction state. |
| **P3 — Materials & light** | `P3.md` (T1–T19, incl. T4b) | Registered PBR (metal-roughness) materials, hardened PNG/GLB-embedded textures, punctual `Light` nodes with replace-not-augment per-viewport lighting, the per-viewport render stages (theme-tint, tone-map, MSAA clamp-down+ack, always-1× pick target), the sRGB-wire/linear-internal color pipeline, and VRAM accounting for MSAA+resolve+pick targets. |
| **P4 — Animation & accessibility** | `P4.md` (T0–T15, incl. T4a/T9a) | Terminal-side animation playback on the deterministic clock (`anim;play/pause/seek/stop`, `(clip,node)` handles, glTF clip import + clip-id ack), TRS-ownership/`node_busy`/`camera_busy` rules, per-node dirty render scheduling, `alt`-text accessibility surfacing via the CPU pick path, bind-pose skinning (no `feat=skin`), and anim/clip lifecycle teardown. |
| **P5 — Hardening & lifecycle** | `P5.md` (T1–T26) | Close the protocol against untrusted input and tie off every lifecycle event: enforce every allocation/framing/structural cap *before* allocation with the drain-then-error shape, the closed versioned error taxonomy end-to-end (consuming P1-T16's enum, with the G12 reachable/reserved partition), per-token isolation via the **private per-token id map** (the G11 confused-deputy fix; `denied` RESERVED), the single fuzz oracle at `Parser::advance(&mut Term)`, and G10 lifecycle (RIS/DECSTR/alt-screen/erase/eviction/device-loss/PTY-EOF). |

---

## Validated cross-phase dependency ordering

The phases form a strict DAG **P1 → P2 → P3 → P4 → P5** at the phase level: every cross-phase
`Depends on` points to an **earlier** phase, and no task depends on a later phase
(forward-dependency-free at phase granularity). The notable concrete cross-phase edges:

- **P2** builds only on **P1** (scene model, framer/CBOR decode, caps/handshake, world matrices,
  the deterministic clock, `Term::tgp_scene()`/`pty_replies` seams, and the `apc_end` back-channel
  P1 introduced). P2 internally re-orders for the shared `destroy_viewport` helper: **P2-T36 is
  built before its callers P2-T11/T12/T13 and P2-T1's `op=remove`** even though it is numbered
  later (numeric order ≠ topological order, documented in P2-T36).
- **P3** builds on **P1** (T6/T7/T8/T9/T11/T12/T13/T14/T15) and **P2** (T1 multi-viewport, T6
  offscreen targets, T20 picking, T21/T23/T24 sub/event/input, T37 device-loss). `asset.remove`
  (P3-T4b) is a **plan-defined op** pending §8.6 ratification (the design names "remove + add" but
  leaves the verb unnamed).
- **P4** uses cross-phase **aliases** mapped in its preamble to P1/P2: `TGP-FOUNDATION`,
  `SCENE-MODEL`, `ASSET-ADD`, `ACK-FRAME`, `ERROR-FRAME`, `APC-END-BACKCHANNEL`, `CAPS-REPLY`
  (P1); `CLOCK`, `PICK`, `EVENT-EMIT`, `EXPLORE` (P2). P4 extends the closed `ev=` enum with a
  node-scoped `anim_end` token (P4-T9a) and extends the `Viewport`/`tgp;vp` op with an `alt` key
  (P4-T1) — both flagged as design-reconciliation items.
- **P5** depends on P1–P4 (mostly via prose phase references: "P1 handshake/token plumbing",
  "P2 instancing", "P3 lights", "P4 anim") plus its own intra-phase chain rooted at P5-T1
  (the closed error enum/emitter) and P5-T2 (handshake+focus emission gate). The fuzz oracle
  (P5-T26) depends transitively on the cap/lifecycle tasks it asserts.

**Within-phase dependencies** are acyclic in every phase (P1's `T11→T15` install-seam edge was
made acyclic by removing the spurious `T15→T14` edge; P3's `T4→T4b` is forward-by-number but
backward-in-build with T4b delivering the op T4's test drives; P2's T36-before-callers is
documented).

### Residual dependency issue — RESOLVED

- **`LIFECYCLE` dangling alias — FIXED.** `P4-T15` (`Depends on: P4-T6, P4-T4, LIFECYCLE`) named an
  alias `LIFECYCLE` that was previously absent from P4's cross-phase alias legend. The P4 cleanup
  pass added it: the legend now maps **`LIFECYCLE` → P2-T38** (RIS/DECSTR teardown machinery) **+
  P2-T15** (alt-screen affinity / screen-buffer binding) — the established lifecycle/affinity
  machinery P4-T15 wires animation state into (G10). The alias now resolves to defined,
  earlier-phase tasks; no dangling alias remains.

A re-scan of every cross-phase `Depends on` line confirms each points to a concrete, defined task in
an earlier or the same phase. **No forward dependencies** (a task depending on a later phase) and **no
dangling task ids or aliases** were found.

---

## The three GPU-free test seams (G6)

Every protocol surface — each op/verb, header field, error code, capability, lifecycle event —
has ≥1 unit test on one of these three seams, all asserting CPU-side state with no GPU:

1. **INGRESS** (covers nearly all surfaces). Feed bytes via `Parser::advance(&mut Term, bytes)`;
   assert internal state via `&self` accessors: `Term::tgp_scene()` nodes (kind, local TRS,
   computed WORLD matrix), viewport pixel rects (cell→pixel CPU-side), decoded instance buffers
   (count/layout/values), asset/material/clip/light tables, `scene_revision`/`asset_revision` +
   per-node/per-viewport dirty flags, and queued PTY reply bytes in `pty_replies`
   (`tgp;r`/`tgp;x`/`tgp;a`). Tests state concrete input bytes and concrete accessor assertions.

2. **INTERACTION** (section-12 surfaces). Inject synthetic pointer/focus events via the `Term`
   input API (`input_pointer`/`input_focus`, P2-T24) — **never** via `Parser::advance`. Assert
   camera node TRS after explore, queued `tgp;e`/`x`/`a` reply bytes (`tok=`/`vp=`/`node=`/`inst=`),
   and hover/sub state. Picking is **CPU-resolvable** (ray-cast vs node world AABBs / instance
   transforms; P2-T20) so picks resolve with no GPU; any GPU color-ID pass is an accelerator that
   MUST agree. Time advances on the ONE injectable deterministic clock (P1-T5), stepped explicitly.

3. **LIFECYCLE** (G10). Drive RIS / DECSTR / alt-screen switch / CSI 2J / ED / resize /
   scrollback-eviction / device-loss / PTY-EOF as byte sequences or `Term` calls; assert
   teardown / affinity / recovery via accessors (`vram_used`, `viewport_alive`, `tgp_token_valid`,
   `pending_bytes`, etc.).

All geometry stays CPU-side and queryable (world matrices, pixel rects, instance decode, pick
resolution); the GPU only consumes it.

---

## Protocol-surface coverage (G6 testing contract)

**Every protocol surface introduced in v1 is covered by ≥1 unit test.** Cross-checked:

- **Frame types** (`q r p vp sub anim e x a`): all owned and tested — P1 (q/r/p/x/a), P2 (vp/sub/e),
  P4 (anim). P1-T3 `header_every_frame_type_recognized` asserts all nine map to a `FrameType`.
- **Ops** (`asset.add material.add node.upsert node.instances node.instance_set node.visible
  node.remove`): owned across P1/P2/P3, each decode+apply tested. `asset.remove` is the
  plan-defined removal op (P3-T4b) flagged for §8.6 ratification.
- **Anim verbs** (`play pause seek stop`): P4-T7, with verb/field error tests.
- **Closed `ev=` set** (`click dblclick down up hover enter leave drag wheel camera resize
  destroyed`): all owned/tested in P2; `anim_end` is the P4-T9a closed-enum extension.
- **Closed error enum (26 codes)**: **owned by P1-T16** (the 26-variant `TgpErrorCode` enum +
  `encode_error`/`wire_code`); **P5-T1 CONSUMES it** (adds only the `ErrorCorr` routing +
  `emit_error` formatter + handshake/focus gating, no second enum). The closed-set round-trip is
  co-owned (P1-T16 `closed_set_encoder_roundtrips_all_26`, P5-T1 `error_code_wire_strings_closed_set`).
  Per the **G12.3 partition, P1 and P5 agree exactly: 24 REACHABLE, 2 RESERVED.** Each reachable
  code has ≥1 concrete ingress reject test across P1-T16 (`every_p1_error_code_emitted`,
  P1-reachable subset), P4-T7 (`unknown_op` via the closed anim-verb locus), and P5-T24 (the
  remaining semantic codes); the 2 RESERVED codes (`denied`, `msg_too_large`) are covered only by
  the closed-set round-trip (`denied` per G11(a) — cross-token addressing is unrepresentable;
  `msg_too_large` per G12.1 — over-msg-cap funnels through `cap_exceeded;detail=max_msg`).
  `unknown_op` is REACHABLE via the closed control-frame anim-verb locus (an `anim` verb ∉
  {play,pause,seek,stop}, P4-T7) but RESERVED for patch `do:` ops (which skip unknowns per §7.3).
  `instanced_node_tint` is `kind_conflict;detail=instanced_node_tint` (G12.4) in P1, P2,
  and P5 alike.
- **Capabilities / `feat=` (11 tokens) / `enc` / `color`**: P1-T6 advertises the frozen set;
  implication rules property-tested (P1-T6 `feat_set_satisfies_implications`, P3-T2
  `feat_pbr_implies_material_property`). Each feat token has an impl owner; use-time `unsupported`
  gates tested at the verb use-sites (P2-T16 instance, P2-T22 pick, P2-T28 explore).
- **Lifecycle events** (RIS, DECSTR, alt-screen, CSI 2J/ED, eviction, detach, device-loss,
  PTY-EOF, `ev=destroyed/resize`): owned in P2 (T36/T38), P3 (T18), P4 (T15), P5 (T17–T23).

### Residual coverage notes (not gaps)

- **Scene-level `alt`** is **out of scope for v1** by design — §13.1 mentions a scene caption but
  defines no wire op to set it (P4 reconciliation item 2). This is a deliberate documented gap,
  not a missing test.
- **`anim_speed_max`, `camera_report_hz`, `msaa`, `click_deadzone_px`** are advertised caps whose
  enforcement is behavioral (clamp/throttle/option), owned by their feature phase (P4 anim, P2/P4
  explore, P3/P2 render-input), explicitly scoped out of P5's allocation-cap tasks (P5 preamble
  "Cap-ownership scope"). Their tests live in the owning phase, so G7 advertised==enforced holds
  whole.

No protocol surface present in the design sections is assigned to **no** phase.

---

## Surface-partition findings (critic) — all three duplications RESOLVED

The three previously-flagged surfaces claimed by two phases are now each framed by the later phase
as an explicit **reuse/extension** of the earlier owner (verified in the cleanup passes). The
surfaces stay covered and are now singly-owned. Listed in original order:

1. **`node.instances` decode + instance-buffer hardened validation — P1-T12 owner, P2 reuses.**
   **RESOLVED.** **P1-T12 is the canonical owner** of the `node.instances` byte decode + hardened
   layout/cap/NaN validation + `InstanceBuffer` storage (it carries a "Canonical owner — later phases
   reuse this; do not re-implement" note). **P2-T16/T17 are recast as thin consumers + use-site
   policy**: T16 wires the `NodeKind::Instanced` scene-kind + the P2-only `feat=instance` use-time
   gate; T17 owns only the P2 scene-apply consequences (atomic last-committed-buffer retention on
   reject + the Instanced-kind tint invariant `kind_conflict;detail=instanced_node_tint`); T18
   (`instance_set` re-tint) and T19 (GPU render path) are the genuinely P2-owned write/render paths.
   There is **no second `instance.rs` decoder** — P2's decode-value tests were renamed to
   surface-through-accessor tests (`instances_decode_surfaced_to_node`, etc.) asserting P2 surfaces
   the P1-decoded buffer unaltered. P2-T16/T17/T18/T19 all cite P1-T12 in `Depends on`.

2. **Closed error enum + `tgp;x` emitter — P1-T16 owner, P5-T1 consumes.** **RESOLVED.** P5-T1 is
   retitled "Error-correlator routing + handshake/focus gating — **EXTENDS P1-T16**" with a binding
   DEDUP note: it **reuses** P1-T16's `TgpErrorCode` enum + `wire_code` + `encode_error` (no
   redefinition, no second `error.rs`) and adds **only** the `ErrorCorr`/`TgpError` correlator types,
   the `emit_error` formatter, and the `should_emit` handshake/focus gating. `Depends on` cites
   `P1-T16 (consumed not redefined)`. The closed-set round-trip is co-owned (G12.3) and re-asserted in
   P5-T1 as a regression guard only.

3. **`tgp;a` ack emitter — P1-T17 owner, P5-T25 reuses.** **RESOLVED.** P5-T25 is retitled
   "`max_backchannel_bytes` enforcement on the ack/error/event queue — **EXTENDS P1-T17**" with a
   binding DEDUP note: it **reuses** P1-T17's `emit_ack` formatter + ack sites + correlator routing
   (no re-definition) and adds **only** the new `max_backchannel_bytes` enforcement on the shared
   reply queue + `Term::backchannel_pending(tok)`. `Depends on` cites `P1-T17 (reused not redefined)`.
   The retained ack tests are explicit regression guards.

Softer / partial overlaps (the later phase mostly adds new behavior, so noted not flagged):
**P5-T2** formalizes handshake-gated emission that P1-T10/T16 already arm, but adds genuinely-new
focus/fg-pgrp gating; **P5-T3** restates the per-`(token,id)` namespace keying P1-T7/T11 already
established, but consolidates `resolve_owned` for every reference site under the ratified G11 private
per-token map. RIS/device-loss teardown across P2-T38 / P3-T18 / P4-T15 / P5-T17 is **correct layered
ownership** (each phase tears down only the state it introduced), not duplication.

**Misplaced surfaces:** none. The scope split is honored — the default material is in P1 (P1-T20,
foundation), PBR materials/lights in P3 (P3-T3/T9), instancing render in P2, animation in P4,
caps-hardening/fuzz in P5. The `node.instances` decode is correctly a CPU surface in P1's scope
(P1-T12), consumed — not re-implemented — by P2.
