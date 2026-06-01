# TGP reconciliation & global invariants

Authoritative cross-cluster spine. Resolves the hard conflicts and high-severity issues the per-cluster review (`tgp-review.md`) raised, and binds every cluster's resolutions (`tgp-decisions.md`) into one coherent design. **Where this file disagrees with a per-cluster decision, this file wins.**

---

## G1. F1 token model — made physically implementable

**Problem the review caught:** a PTY exposes a *single* writeback channel (`queue_reply → pty_replies`, the DA1 path). The terminal **cannot** selectively deliver bytes to one of several processes sharing the PTY. "Route events only to the owning app" is not physically achievable; only addressing isolation, emission gating, and tagging are.

**Resolution (refines F1):**
- **Token carried in the TEXT header of every frame:** `tgp;<type>;tok=<≤16B>;…`. Established at handshake — the app proposes `tok=` in `tgp;q`; the terminal accepts/echoes it in `tgp;r` (or assigns one). Token survives a binary cap/abort because it is in the text header (ties to G4 / WIRE).
- **Addressing isolation (hard-enforced):** node/asset/clip/viewport/material ids are namespaced under the creating token. A frame bearing token T may only address/mutate objects created under T. A frame with a different/absent token that tries to upsert/remove/reference T's ids → `x code=denied`. **This is the confused-deputy fix** (a `curl | cat` line cannot overwrite your `cam`). The doc's "one global namespace per session" (§6/§8.4) is rewritten to "one namespace **per token**."
- **Emission is filtered + tagged, not routed:** `tgp;e/x/a` are emitted on the single shared FIFO, **only** for frames/subscriptions bearing a live token, and every emitted frame is tagged `tok=` so the reader can demux. The terminal does not guarantee which process reads them; physical delivery isolation is impossible over one PTY and is the reader's responsibility via the tag. (In practice only the foreground app reads stdin.)
- **Emission gating (absorbs LIFE-7):** the terminal emits events only while the owning token has a live subscription **and** the terminal is focused; it cancels that token's subs/explore/anim and stops emitting when the foreground process group changes or the PTY input side closes — preventing `tgp;e` bytes leaking into a shell prompt as garbage.

## G2. Hard-conflict rulings (single source of truth)

- **Viewport camera/root refs are LATE-BOUND (resolves SCEN-5 ↔ VIEW-5/6):** asset/material/parent refs *inside a patch* resolve against the txn's final committed state; unresolved → `x bad_ref`, whole txn rejected (SCEN-4). **But** a viewport's `camera`/`root` are not patch refs — a `vp` may be created before its camera node (§18.1). A viewport whose camera is missing/removed renders with an **implicit auto-framing default camera** (never rejects, never crashes); a missing `root` defaults to the whole scene. `node.remove` of a node used as a viewport camera/root succeeds and silently falls that viewport back to default (optional diagnostic `x`/event to the owning token). VIEW-5's invented "viewport diagnostic channel" does not exist — diagnostics are normal `tgp;x` to the owning token.
- **Concurrent chunk stream for one id (resolves SCEN-9 ↔ WIRE-5; WIRE owns chunking):** per-id in-flight reassembly buffer. A second `more=1` open for an id that already has an in-flight buffer, or a format change mid-stream, → `x chunk_conflict`, **retaining the prior buffer**. Independent ids interleave freely. (Reject-new-keep-prior beats RGP's abort-prior; F2 frees us from RGP behavior, and it stops an interleaved hostile stream from wiping a legit upload.)
- **In-flight chunks vs asset immutability (resolves SCEN-9 ↔ §8.4):** an in-flight reassembly buffer is **not** a registered asset. The dup-id error and `mesh:` ref resolution check only **committed** assets. `more=0` commits the asset and bumps `asset_revision` once.
- **Instance-buffer revision (resolves SCEN-7 ↔ CAPS-10 ↔ §6.4/§6.5):** any `node.instances` content/size change bumps `scene_revision` + a per-node **instance-dirty** flag, **never** `asset_revision`. An over-cap `node.instances` txn is rejected atomically and **retains the last committed instance buffer** (no flicker).
- **Camera owned by one controller (resolves ANIM-5 ↔ EXPL):** a viewport's camera is driven by at most one terminal-side controller. Enabling `explore` on a viewport stops/blocks `anim;play` on its camera node → `x camera_busy` on the conflicting `anim;play`; and vice-versa.
- **Animation stop/end pose (resolves ANIM-4 ↔ ANIM-6):** `stop` releases the node's TRS ownership and **leaves it at its last-evaluated pose** (no snap-to-bind). `pause` holds. A non-looping clip at end **holds the last frame** and emits an optional `anim_end` event. "Bind pose" terminology is dropped to avoid the contradiction.
- **Hover leave triggers (clarifies EXPL-5 vs F3/VIEW reflow):** hover is recomputed every render from the (CPU-resolvable, G6) pick result; `leave` fires whenever the prior `(vp,node,inst)` is no longer under the cursor for **any** reason — removal, hide, transform, animation, **reflow/scroll-driven rect motion**, or viewport boundary crossing.

## G3. Closed, versioned error-code enum (v1)

The single closed set (gated by `v=`); `detail=` is advisory and explicitly **non-load-bearing**. Every cluster-invented synonym maps here.

```
parse_error  msg_too_large  truncated  cap_exceeded  vram_exhausted
bad_ref  dup_id  unknown_op  unknown_node  unknown_clip
kind_conflict  cycle  depth_exceeded  bad_index  bad_layout  bad_param
chunk_conflict  unsupported  denied  node_busy  camera_busy
not_playing  no_targets  invalid_sub  bad_token  null_not_allowed
```
Synonym map: `forbidden→denied`, `instanced_node_tint→bad_param`, `dup_asset→dup_id`, `binary_decode→parse_error (detail=binary_decode)`, `token_denied→bad_token`.

## G4. Error/ack correlation (generalizes SEC-1 / VIEW-2 / CAPS-9)

`tgp;x` and `tgp;a` cite whatever correlator the offending/acked frame carried, plus `tok=`:
- patch `p`: `txn` (+ `op` index on `x`).
- viewport `vp`: `vp` id (vp frames have no txn).
- chunked `asset.add`: `asset` id.
- frame with no correlator: `x` with `code` only.

`txn` (and `tok`, `enc`, `len`) live in the **text header** so they survive a binary-payload cap/abort. **Acks are available to any handshaked token** (not gated on `sub`); used for txn-commit confirmation and the `enc=bin` transport probe (CAPS-9).

## G5. Error-reporting opt-in = the handshake (owner: CAPS-7)

A completed `tgp;q`/`tgp;r` handshake **is** the error-reporting opt-in for that token; there is no separate `errors=` flag in v1. Dumb readers never handshake → stay silent (no `x`/`e`/`a`). Every cluster's "opted into error reporting" means "handshaked token."

## G6. Testing seams (makes the user's requirement precise)

Two complementary **GPU-free** unit-test seams, both asserting CPU-side internal state:

1. **Ingress (covers nearly all of the protocol):** feed bytes via `Parser::advance(&mut Term, bytes)`; assert via `&self` accessors — `Term::tgp_scene()` nodes (kind, local TRS, **world matrix**), viewport **pixel rects** (cell→pixel computed CPU-side), decoded **instance buffers** (count/layout/values), asset table, `scene_revision`/`asset_revision`/dirty flags, and **queued PTY replies** (`tgp;r`/`x`/`a` bytes in `pty_replies`).
2. **Interaction (explore/events/picking):** inject synthetic pointer/focus events via the `Term` input API, then assert (a) camera node TRS after explore, (b) queued `tgp;e`/`x`/`a` reply bytes (with `tok=`/`vp=`/`node=`/`inst=`), (c) hover/sub state. *(Pointer input does NOT arrive via `Parser::advance` — this is the second seam the review flagged.)*

Cross-cutting testability rules (binding on all clusters):
- **CPU-resolvable picking:** click→`(node,inst,world-point)` resolution MUST have a CPU path (ray-cast vs node world AABBs / instance transforms, or a CPU-rasterized pick buffer) so picks resolve without a GPU. The GPU color-ID pass is an optional acceleration that MUST agree with the CPU result. World-hit-point depth is taken from the same CPU path.
- **Deterministic injectable clock (generalizes ANIM-1):** all terminal-side time — animation playback, explore damping, camera-report throttle, partial-frame/inactivity timeouts — uses ONE injectable monotonic clock; tests advance it explicitly.
- **Geometry CPU-side:** world matrices, viewport pixel rects, instance decode, pick resolution all computed on the CPU and queryable; the GPU only consumes them.

## G7. Capability reply — one closed set (owner: CAPS/SEC; resolves CAPS-3/9, SEC-5, WIRE-4/8)

`§7.1 tgp;r` and the `§15.2` cap list are the **same** closed, scope-suffixed set:
- `v=` + `vmin=` (always answer any well-formed `tgp;q` with a `tgp;r` carrying the supported range, so "`tgp;r` present" unambiguously means TGP exists). `tgp;r` is enqueued **before** the DA1 reply on the shared FIFO; the app sends `tgp;q` **before** `ESC[c` (CAPS-1/2 — the no-hang guarantee).
- `feat=` frozen v1 token set with implication rules: `pbr⇒material`, `instance⇒graph`, `pick⇒event`, `explore⇒event`. Apps MUST ignore unknown tokens. **Drop the redundant `binframe` token** — `enc=` is the single source of truth.
- `enc=b64,bin` — **b64 is the safe default**; `bin` is opt-in and verified via the `enc=bin` probe→ack before bulk streaming.
- Caps (scope-suffixed, same names in reply and spec): `max_verts_per_asset`, `max_indices_per_asset`, `max_instances_per_buffer`, `max_nodes_session`, `max_node_depth`, `max_vram_mb_session`, `max_msg_mb` (**decoded** payload size; applies to both encodings, b64 encoded length bounded at `ceil(max*4/3)`), `max_reasm_mb`, `max_tex_dim`, `max_inline_cols`, `max_inline_rows`, `max_viewport_px`, `max_lights_per_viewport`, `anim_speed_max`, `camera_report_hz`, `max_backchannel_bytes`.
- `color=srgb`: wire colors are sRGB-encoded; the renderer works in linear internally; tone-map then sRGB-encode into the offscreen so the composite against the (sRGB) text plane matches (resolves MATE-9).
- Caps are re-queryable mid-session; on device-loss/headless transitions the terminal pushes an unsolicited `tgp;r` to handshaked tokens (CAPS-4) — overrides LIFE-5's "device loss invisible".

## G8. F2 purge (clean break from RGP)

Delete design §4 (Relationship to RGP) and every RGP-adapter assumption from the resolutions:
- Default material pins its **own** constants (key dir, key/ambient/hemisphere colors, brightness clamp) — no requirement to match `rgp.wgsl` byte-for-bit (MATE-2). Golden-image test against TGP's own constants.
- No adapter id prefix needed; SCEN-8 keeps only id **hardening**: `tok`-scoped, ≤64 bytes, no control/ESC bytes (ids re-enter stdin via `tgp;e`).
- Drop the RGP-vs-native differential fuzz target (CAPS-11); keep the single fuzz oracle (G-test: every input yields a bounded scene or a structured error / clean drop within bounded time+memory, never crash/leak — fuzzed at the `Parser::advance(&mut Term)` seam).
- The existing RGP (`ratty;g;`) implementation remains untouched, independent legacy with its own code path. TGP does not bridge, reuse, or inherit from it.

## G9. Render-gate uniformity (resolves SCEN-10 ↔ ANIM-9)

Revisions are `u64`. The renderer gates on **per-node / per-viewport dirty flags + generation tokens**, not numeric equality (eliminates the wrap hole). Terminal-side animation/playback sets per-node dirty flags and re-renders only the affected viewport(s) via that machinery — it does **not** bump global `scene_revision` every frame (preserves the no-re-upload optimization) and does not re-emit unrelated cells.

## G10. Cross-cutting lifecycle (binds LIFE ↔ all)

- **RIS** = full scene teardown (assets, nodes, viewports, GPU buffers, pending chunk buffers) across **all tokens** — the guaranteed VRAM-recovery path. **DECSTR** leaves the scene intact.
- **Alt-screen**: inline viewports bind to the screen buffer of creation (hidden on switch, restored on return); pinned defaults to current buffer with an opt-in `screen_affinity` for HUDs that persist across both. An inline viewport hidden by alt-screen is NOT scrollback-evicted while hidden.
- **Inline lifetime is bounded by scrollback depth**: eviction of the authoritative placeholder cells destroys the viewport and frees its layer (optional event to the owning token). `CSI 2J`/ED erases placeholder cells like text → destroys/orphans that inline viewport; pinned viewports (not cell-bound) persist.
- **GPU device loss**: CPU scene is the source of truth; rebuild device + force re-upload all assets/instances/targets ignoring dirty gating; never lose CPU state.
- In-flight explore drag / animation survive resize & theme change by tracking state in **resolution-independent** terms (normalized coords + clock phase); only device loss may cancel an in-flight drag (synthetic `up`+`leave`).

---

## Section ownership for the v2 design rewrite

| Doc sections | Owner inputs |
|---|---|
| §1–3, §5 (drop §4) | G1, G8; clean-break framing |
| §6, §8.4, §8.6 (scene/ids/patch) | SCEN decisions + G1, G2, G9 |
| §7, §16 (caps/open) | CAPS + SEC decisions + G7 |
| §8.1–8.3, §8.5, §8.7 (wire) | WIRE decisions + G4 |
| §10 (viewports) | VIEW decisions + G2, G10 |
| §11 (rendering) | MATE decisions + G6 (pick), G7 (sRGB) |
| §12 (interactivity) | EXPL decisions + F4, G6 |
| §13, §14 (a11y, animation) | ANIM decisions + G2, G6, G9 |
| §15 (security) | SEC decisions + G3, G5, G8 |
| §17–19 (phasing/examples/glossary) | all; reflect token + clean-break |
