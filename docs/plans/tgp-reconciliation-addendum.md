# TGP reconciliation addendum (G11-G12)

> **Authoritative.** This addendum supersedes the base spine (`tgp-reconciliation.md`, G1-G10) and the per-section design slices wherever they conflict. It ratifies the user-approved **private per-token id map** isolation model and fixes a single consistent error-code reachability taxonomy across phases P1 and P5.

## G11 — Per-token isolation model (ratified)

**The v1 isolation model is a PRIVATE per-token id map.** Each handshaked token `T` owns a *private namespace* — a `T → (id → object)` map that no other token can index. A frame bearing token `T` can only name ids that live in `T`'s own map; there is no global id table, no ownership tag on a shared id, and no cross-token lookup primitive anywhere in the terminal.

Concretely, an id reference resolves as `(bearing_token, id)`, never as `id` alone:

- A frame with `tok=B` naming id `X` resolves to the entry `{B, X}` in B's map — **never** to `{A, X}`, even when A previously created an `X`.
- Two tokens that both create `"cam"` (or asset `7`, or viewport `9`) hold **two distinct, coexisting entries** `{A,"cam"}` and `{B,"cam"}`. They are not a collision; they never alias; neither can observe the other.

**Cross-token addressing is therefore IMPOSSIBLE BY CONSTRUCTION.** There is no code path that can take a reference from token B and land it on an object owned by token A *while B and A are distinct tokens*. This *is* the confused-deputy fix, and against **accidental** cross-app collisions it is **strictly stronger** than the base spine's ownership-tagged single-global-namespace design: under a global namespace, an accidental cross-token reference is *attempted and then denied* (a check that can be forgotten, mis-ordered, or bypassed); under a private map, the attempt is **unrepresentable** — there is no shared key to collide on in the first place.

**Scope of the guarantee (threat-model honesty).** The "strictly stronger" claim above is scoped to *accidental* collisions and to adversaries who do **not** know the victim's token. It is **not** a defense against an adversary who *learns* the victim's token. The token is a pure **bearer credential**: app-chosen, ≤16 bytes, printable, with **no unguessability/randomness requirement and no per-process binding** (`s07.md:137`, `s08.md:71`), and it is echoed in cleartext in every frame's text header and in every emitted `tgp;r`/`x`/`e`/`a`. A co-resident adversary who can read the shared PTY stream — the exact "hostile `curl | cat`" of the `s15.1` threat model — can **observe** a victim's token and then write frames bearing `tok=<victim>`. Those frames resolve under the **same** token map and successfully mutate/delete the victim's objects, and crucially they **never** trigger `denied`: a stolen-token write is a *same-token* op, not a cross-token attempt, so there is no cross-token event for `denied` to backstop. **Therefore: reserving `denied` is logically sound (no cross-token PATH exists), but token confidentiality/unguessability is now LOAD-BEARING for isolation.** The private map defeats the *accidental* confused-deputy and the *unguessing* adversary; it does **not** defeat an adversary who has read the victim's token off the shared stream. (This does not change the v1 token format — it is a threat-model disclosure: any deployment that needs to resist a stream-reading adversary must treat the token as a secret, since `denied` will never catch a stolen-token write.)

### Consequences (applied everywhere — binding on all phases)

**(a) `code=denied` has NO reachable trigger for cross-token addressing in v1.** Because a frame can only ever name ids inside its own map, there is no ingress event that produces "you named someone else's object." `denied` is therefore **RESERVED** in the closed G3 enum: it is a member of the versioned wire vocabulary (so the closed set stays stable across versions) but has **no reachable ingress trigger**. It is carried *only* by the closed-set encoder round-trip test (encode→decode every G3 code), exactly as `msg_too_large` is reserved by precedent (and on its own construction-based ground: no cross-token PATH exists). Any "isolation" test that asserts `denied` on a *plain cross-token id collision* is **UNSOUND and must be rewritten**: a "collision" between `{A,"cam"}` and `{B,"cam"}` is not an error condition at all — the two entries simply coexist as distinct map keys (cf. the P2-T8 z-order tiebreak, where viewports from different tokens coexist and are merely ordered, never rejected). The correct rewrite asserts *coexistence* (both objects present, independently mutable, neither observable from the other), not a `denied` reply.

**(b) A patch ref to an id absent from the bearing token's map → `bad_ref`, with NO existence oracle.** When a frame with token `T` references an id that is not in `T`'s map — including an id that *does* exist under some other token `A` — the reference is unresolved within `T`'s namespace and the whole txn is rejected with `code=bad_ref;detail=<kind>:<id>` (per G2 reference-resolution). The error **MUST NOT** leak whether that id exists elsewhere: `bad_ref` for "absent in my map" and `bad_ref` for "absent everywhere" are byte-for-byte indistinguishable. There is **no existence oracle** — a probing token cannot use `bad_ref` vs any other code to enumerate or detect another token's ids. (This is the natural and only outcome under a private map: B's resolver never even consults A's map, so it has nothing to leak.) This holds for **every** resolver that consults the bearing token's map — `bad_ref`, viewport late-bind, `unknown_node`, `unknown_clip`, `invalid_sub`, `dup_id` — each of which is causally blind to the other token's contents, so its code/`detail` is identical regardless of cross-token existence.

**(b′) Implementation note (normative): no shared-storage side-channel.** The "no existence oracle" guarantee above is specified at the **code/`detail`** level, but a naïve implementation could still leak existence through a *shared-storage timing/allocation side-channel* — e.g. a single global id table keyed by raw id (or by a hash that mixes tokens into shared buckets), where a lookup whose key happens to be occupied by **another token's** entry probes, collides, or allocates measurably differently from a fully-absent key. Such a channel would let a probing token detect another token's ids by timing alone, defeating (b) below the spec level. **The implementation MUST close this channel.** Either (preferred) the per-token maps are **physically disjoint** allocations — token `T`'s lookups touch only `T`'s own storage and never index, hash into, or contend any structure holding another token's entries — **or** lookup cost/observable allocation behavior MUST be provably independent of other tokens' entries (e.g. per-token sub-tables with no cross-token bucket sharing). A single global `(id → object)` table, or any structure where a cross-token-occupied key is reachable on `T`'s lookup path, is **non-conforming**. This is what makes the spec-level indistinguishability of (b) survive implementation.

**(c) A viewport `camera`/`root` referencing an id absent from the token's map → late-bind default per G2, never `denied`, never reject.** A viewport's `camera`/`root` are **not** patch refs (G2): a `vp` may legally name a camera/root node that does not (yet) resolve in the owning token's map. The viewport renders with the **implicit auto-framing default camera** (missing/unresolved `camera`) or the **whole-scene root** (missing/unresolved `root`), and silently adopts the real node if/when it later appears in that token's map. This is **never** `denied` and **never** a reject; an optional diagnostic `tgp;x;code=bad_ref` MAY be sent to the owning token on entering the missing state (G2/s10.5), but rendering never stops.

**(d) A TGP frame that requires a token but lacks a valid handshake → `bad_token`, not `denied`.** A frame that needs a session token but carries a malformed/unacceptable token, or proposes a token through a handshake that is rejected, draws `code=bad_token` (synonym `token_denied→bad_token`, per G3). This is distinct from `denied`: `bad_token` is about *establishing/validating* a token; `denied` would have been about *cross-token addressing under a shared namespace* — which no longer exists. (A well-formed `tgp;p` from a writer that simply never handshaked is dropped **silently** per G5, not answered with `bad_token`.)

### Supersession

This G11 model **supersedes** the global-namespace "denied on cross-token reference" prose in the base spine **G1** (line 13, "→ `x code=denied`") and in **s08.4** (the `denied;detail=token` cross-token reference text) and **s15.2** (the `code=denied` confused-deputy text). Everywhere those documents say a cross-token reference is *answered with `denied`*, the ratified behavior is instead: the reference is *unrepresentable* (private map), so the only observable outcomes are coexistence (a's own id), `bad_ref` (an id absent from the bearing token's map, with no existence oracle), late-bind default (viewport camera/root), or `bad_token` (no valid handshake). The `denied` code itself remains in the closed enum, **RESERVED**.

## G12 — Error-code reachability

This section fixes **one** consistent error-code taxonomy across phases P1 (wire/parse/caps) and P5 (security/fuzz/closed-set), so the two phases cannot disagree about which codes are reachable and which are reserved.

### G12.1 — Over-msg-cap canonical code (resolves the `msg_too_large` vs `cap_exceeded;detail=max_msg` split)

An over-`max_msg_mb` **per-message decoded-size breach** emits **`code=cap_exceeded;detail=max_msg`** — honoring the explicit s08.7 text ("On overflow the in-flight buffer is dropped → `x;code=cap_exceeded;detail=max_msg|max_reasm`"). This is the canonical code on **both** loci that detect the per-message breach:

1. **The `enc=bin` drain path.** An over-cap binary header (`len` > `max_msg_mb`) is detected at text-header parse time; the terminal still enters consume-N, **drains exactly `len` bytes** (without buffering them), returns to Ground, then emits `cap_exceeded;detail=max_msg`. (`len > BIN_HARD_CEIL` is the separate unrecoverable variant: `cap_exceeded;detail=unrecoverable`, stop at the header boundary — s08.7.)
2. **The decode-size reject path.** An `enc=b64` (or already-buffered) payload whose **decoded** size exceeds `max_msg_mb` is rejected with the **same** `cap_exceeded;detail=max_msg` — both encodings share one decoded-size cap and one error contract (s08.3).

**Every over-msg-cap test MUST assert `cap_exceeded;detail=max_msg`** — this binds P1-T2 and P1-T9 (the over-msg-cap drain/reject tests) and the reachable-code assertions of **P1-T16** and **P5-T1**. The `detail=` is advisory/non-load-bearing (G3); tests branch on `code=cap_exceeded` and MAY additionally check `detail=max_msg` as documentation, never as control flow.

**Consequence for `msg_too_large`:** the base spine G3 enum and s15.2/s15.3 list a separate `msg_too_large` code and even show it in an example (`s15.3` line 112). Under this ruling the over-message-size breach is funneled through `cap_exceeded;detail=max_msg`, so **`msg_too_large` has no reachable ingress trigger** and is **RESERVED** (closed-set test only). This is a deliberate single-taxonomy choice: one cap family (`cap_exceeded`) with a `detail=` discriminator, rather than two codes for the same class of breach. P1 and P5 MUST both treat `msg_too_large` as reserved.

**Supersession (explicit — mirrors the G11 list).** This G12.1 ruling **supersedes** every live, contradictory `msg_too_large` locus, which would otherwise normatively direct an implementer to emit the now-reserved code:

- **`s15.2` message-size locus** (`s15.md:55`): "On over-cap the terminal still enters consume-N, drains exactly `len` bytes, returns to Ground, and emits `code=msg_too_large` citing `txn`" → **read instead as** `code=cap_exceeded;detail=max_msg` (the drain-then-error shape is unchanged; only the code changes). An implementer reading `s15.2` in isolation MUST treat this locus as emitting `cap_exceeded;detail=max_msg`.
- **`s15.3` structured-error example** (`s15.md:112`): `ESC _ tgp;x;tok=A1;txn=51;code=msg_too_large;detail=len=…,cap=… ESC \` → **read instead as** `code=cap_exceeded;detail=max_msg` (the correlation/`detail` shape is otherwise unchanged).
- **`s15.3` prose** (`s15.md:120`): "`op=` is omitted exactly when no op index was ever parsed (e.g. a pre-decode `msg_too_large`)" → **read instead as** "a pre-decode `cap_exceeded;detail=max_msg`"; the `op=`-omission rule itself is unchanged.

Everywhere those loci say the over-message-size breach is answered with `msg_too_large`, the ratified behavior is `cap_exceeded;detail=max_msg`. The `msg_too_large` code itself remains in the closed G3 enum, **RESERVED**.

### G12.2 — Final reachability call on the three reserved candidates

The base review flagged `msg_too_large`, `unknown_op`, and `denied` as reserved candidates. The final v1 call:

- **`msg_too_large` → RESERVED.** Subsumed by `cap_exceeded;detail=max_msg` (G12.1).
- **`unknown_op` → REACHABLE.** It fires via the **closed control-frame-verb locus**: an `anim` control frame has a CLOSED verb set `{play,pause,seek,stop}` (s14.3) that is ALWAYS fully parsed (s07.2), so a verb outside that set (e.g. `tgp;anim;…;frobnicate`) is a malformed control frame and draws `code=unknown_op` (P4-T7 `unknown_verb_errors_unknown_op`). This is **distinct** from a patch `do:` op, where an unknown op string is **skipped, not fatal** (s07.3 forward-compat) — patches never emit `unknown_op`. So `unknown_op` is reachable via the closed control-frame-verb locus, and is only *reserved for patch ops*.
- **`denied` → RESERVED.** Per G11(a): cross-token addressing is unrepresentable under the private map, so no ingress event triggers `denied`. Carried only by the closed-set encoder round-trip test (reserved on its own construction-based ground — cf. `msg_too_large` as a reserved-by-precedent example).

`msg_too_large` and `denied` are members of the closed, versioned wire enum (so the set is stable and forward-compatible) but have **no reachable ingress trigger**. The remaining 24 codes are **REACHABLE**, each with the canonical ingress trigger named in the table below.

### G12.3 — Reachability table (all 26 G3 codes)

`REACHABLE` = there is at least one ingress frame (`Parser::advance`-driven) that emits this code to a handshaked token. `RESERVED` = no reachable ingress trigger; the code exists in the closed enum and is exercised **only** by the closed-set encoder round-trip test (P1-T16 / P5-T1). `detail=` shown is canonical/advisory only (non-load-bearing, G3).

| # | Code | Status | Canonical ingress trigger (REACHABLE) / note (RESERVED) |
|---|------|--------|----------------------------------------------------------|
| 1 | `parse_error` | REACHABLE | Frame header/body fails to parse from a handshaked token: CBOR decode failure, `trailing_bytes`, `truncated` CBOR (s08.3), `base64` decode error, `field_order` (`len` before `enc`), `instance_nan`, `trailer_corrupt`. |
| 2 | `msg_too_large` | RESERVED | Subsumed by `cap_exceeded;detail=max_msg` (G12.1). Closed-set test only. |
| 3 | `truncated` | REACHABLE | Binary frame in consume-N aborted by EOF/inactivity-bound/reset with `remaining > 0` (s08.2 cross-read lifecycle, s15.2 truncation locus). |
| 4 | `cap_exceeded` | REACHABLE | Any advertised cap breached at allocation point: `detail=max_msg` (per-message, G12.1), `max_reasm`, `max_instances`, `max_inline_cols`/`max_inline_rows`, `max_viewport_px`, expanded-output/accessor/texture caps, `unrecoverable` (`len > BIN_HARD_CEIL`). |
| 5 | `vram_exhausted` | REACHABLE | Two-phase CPU VRAM accounting: txn net VRAM delta exceeds `max_vram_mb_session` before upload (s15.2 VRAM locus). |
| 6 | `bad_ref` | REACHABLE | Patch ref (`parent`/`mesh`/`asset`/`material`/`tex`) unresolved in the **bearing token's** map at txn-final state — including an id present only under another token (G11(b), no existence oracle). `detail=<kind>:<id>`. |
| 7 | `dup_id` | REACHABLE | `asset.add` / chunk targeting an already-**committed** asset id in the token's map (s08.4 collision rule, s08.7). `detail=<id>`. |
| 8 | `unknown_op` | REACHABLE | Unrecognized verb in a CLOSED control-frame verb set (e.g. an `anim` verb not in {play,pause,seek,stop}, s14.3) — the frame is fully parsed (s07.2) and the garbage verb is fatal. NOTE: distinct from a patch `do:` op, where an unknown op is SKIPPED not fatal (s07.3) — patches never emit `unknown_op`. |
| 9 | `unknown_node` | REACHABLE | An op/controller targets a node id absent from the token's map where the op requires an existing node (non-`upsert` node ops, e.g. animation/explore targeting a missing node). |
| 10 | `unknown_clip` | REACHABLE | `anim;play`/control names a clip id absent from the token's map (s14 animation control). |
| 11 | `kind_conflict` | REACHABLE | Multiple kind selectors in one op (`detail=multiple_selectors`); an upsert selector implying a different kind than the existing node; `Instanced` node with a node-level tint (`detail=instanced_node_tint`) — **this is the sole canonical code for the node-level-tint-on-Instanced trigger**, superseding the base-spine G3 `instanced_node_tint→bad_param` synonym (s08.6 / s08.md:118 / s06.md:117-119; see G12.4). |
| 12 | `cycle` | REACHABLE | Parent-chain walk at commit finds a self-parent or cycle (s08.6 acyclic check). `detail=<id>`. |
| 13 | `depth_exceeded` | REACHABLE | Parent chain exceeds `max_node_depth` at commit (s08.6). |
| 14 | `bad_index` | REACHABLE | `node.instance_set{index}` with `index >= count` (s08.6 single-instance re-tint). |
| 15 | `bad_layout` | REACHABLE | `node.instances` byte-string lengths mismatch `count` (`detail=xforms_len|tints_len|count_mismatch`) (s08.6). |
| 16 | `bad_param` | REACHABLE | Out-of-range/invalid op field, incl. node-id charset/length violation (`detail=charset|too_long`) (s08.4 id hardening). **Note:** the `instanced_node_tint` trigger does NOT map here — it is canonically `kind_conflict;detail=instanced_node_tint` (row 11); the base-spine G3 `instanced_node_tint→bad_param` synonym is **superseded** (see G12.4). |
| 17 | `chunk_conflict` | REACHABLE | Second open `more=1` stream for an id with an in-flight buffer, or `fmt` change mid-stream (`detail=chunk_interleave|chunk_fmt_mismatch`); prior buffer retained (s08.7). |
| 18 | `unsupported` | REACHABLE | A negotiated-version frame requests a verb/feature path the terminal gates off (controller/feature gating, s15.3). |
| 19 | `denied` | RESERVED | Cross-token addressing is unrepresentable under the private per-token map (G11(a)). No ingress trigger. Closed-set test only (reserved on its own construction-based ground; cf. `msg_too_large` as the reserved-by-precedent example). |
| 20 | `node_busy` | REACHABLE | An op contends a node already owned by a terminal-side controller (e.g. transform write to a node whose TRS is owned by active playback/explore) (s15.3 controller gating). |
| 21 | `camera_busy` | REACHABLE | `anim;play` on a viewport camera while `explore` owns it, or vice-versa (G2 camera-owned-by-one-controller). |
| 22 | `not_playing` | REACHABLE | `anim;stop`/`pause`/control on a clip/node that is not currently playing (s14). |
| 23 | `no_targets` | REACHABLE | A `sub`/op resolves to an empty target set (no nodes/viewport matched) (s12/s15.3). |
| 24 | `invalid_sub` | REACHABLE | Malformed/illegal subscription (`sub` to a nonexistent viewport, conflicting flags) (s12 subscriptions). |
| 25 | `bad_token` | REACHABLE | Handshake proposes a malformed/unacceptable token, or a token-requiring frame lacks a valid handshake (G11(d); synonym `token_denied→bad_token`). |
| 26 | `null_not_allowed` | REACHABLE | CBOR null on a non-nullable field (`id`, kind selector) in `node.upsert` (`detail=<field>`) (s08.6 sparse-merge). |

**Tally:** 24 REACHABLE, 2 RESERVED (`msg_too_large`, `denied`). P1 and P5 MUST agree on exactly this partition; the closed-set encoder round-trip test (P1-T16 / P5-T1) covers all 26, and the reachable-ingress test suites cover exactly the 24 marked REACHABLE.

### G12.4 — `instanced_node_tint` maps to exactly ONE code (resolves the synonym-vs-detail double-map)

A node-level tint on an `Instanced` node is a **single trigger** and MUST emit **exactly one** code. There were two contradictory mappings in the inherited material:

1. The design slices map it to **`kind_conflict;detail=instanced_node_tint`** (`s08.md:118`: "An `Instanced` node ... **must not** set a node-level tint (→ `x;code=kind_conflict;detail=instanced_node_tint`)"; `s06.md:117-119`: "rejected with `x code=kind_conflict`, `detail=instanced_node_tint`").
2. The base-spine **G3 synonym map** (`tgp-reconciliation.md:38`) lists `instanced_node_tint→bad_param`.

**Ruling: the canonical code is `kind_conflict;detail=instanced_node_tint` (G12.3 row 11).** The trigger is a *kind invariant* violation (an `Instanced` node may not carry the `Mesh`-style node-level tint), which is exactly what `kind_conflict` denotes; the slices are the live, more-specific normative text. The base-spine G3 synonym-map entry **`instanced_node_tint→bad_param` is hereby SUPERSEDED and struck** — `instanced_node_tint` is a `detail=` discriminator on `kind_conflict`, **not** a synonym for `bad_param`. Row 16 (`bad_param`) therefore no longer carries this trigger. The same trigger consequently maps to exactly one reachable code, and P1 and P5 see a single partition with no double-mapped trigger.

(`bad_param` remains reachable on its own triggers — out-of-range op fields and node-id charset/length violations, G12.3 row 16 — just not on `instanced_node_tint`.)

