# TGP ambiguity register (triage)

- **Source:** 110 findings from 10 parallel adversarial hunters → `tgp-ambiguity-findings.md` (full detail, options, recommendations).
- **Grounding:** code-verified brief embedded in the hunt; see also my notes in this dir.
- **Status:** triaged 2026-05-31. P0 = blocks coherent implementation / core semantics; P1 = important interaction/correctness; P2 = polish/deferrable.
- **Resolution policy:** the ~4 *pivotal product forks* below go to the user. Everything else is resolved by engineering judgment (mostly adopting the hunters' recommendations), then reviewed.

---

## Pivotal forks for the user (everything else I resolve myself)

| # | Fork | Why it's yours, not mine | Findings |
|---|------|--------------------------|----------|
| F1 | **Scene scope across processes** — one shared global scene vs. per-app namespace token vs. per-process-group isolation | Security + architecture; changes the externally-visible trust boundary and event routing | LIFE-6, CAPS-10, SCEN-8 |
| F2 | **RGP coexistence depth** — per-node origin flag (preserve exact RGP semantics) vs. full translation to native TGP vs. parallel RgpScene | Decides renderer unification & demo-compat scope | RGP-1/2/3/8/9 |
| F3 | **Inline viewport position authority + limits** — placeholder-cells authoritative vs. {line,col} authoritative vs. pinned-only-for-v1 | Shapes the headline inline feature & its reflow behavior | VIEW-2/3/4/12, LIFE-1/3/4/9 |
| F4 | **Interactivity default routing** — sub captures viewport cells vs. always-both vs. SGR-wins | Determines how TGP coexists with existing mouse-driven TUIs | EXPL-1/2/8/10 |

---

## P0 items (must be pinned before/at implementation)

**WIRE** — WIRE-2 parser back-channel (`apc_end` must signal binary len; recommend `apc_end()->Option<BinaryFollow{len,enc}>`) · WIRE-1 no terminator after raw bytes; return to Ground (tolerate optional ST) · WIRE-3 `len` authoritative; decode-mismatch→`x parse_error`+resync · WIRE-7 `enc=` precedes `len=`, default b64, only `enc=bin` enters raw-read · WIRE-9 persisted cross-`advance()` remaining-count + capped buffer · SEC-1 hoist `txn` into the **text** header so cap-rejections cite it & still drain N bytes.

**SCENE** — SCEN-1 CBOR-null=clear vs key-absent=preserve · SCEN-3 cycle/self-parent detection at commit → `x cycle` + depth cap · SCEN-4 resolve refs against txn's final state (order-independent) → `x bad_ref` · SCEN-6 pin one canonical instance layout (mat4 f32 LE + RGBA8 + explicit count, bounds-checked) · SCEN-7 instance content change bumps scene_revision + per-node instance-dirty, never asset_revision · SCEN-8 hard-cap id length, forbid control/ESC bytes (re-enter stdin via events), reserve adapter prefix.

**CAPS** — CAPS-1/2 make DA1 pairing normative + FIFO so `tgp;r` precedes DA1 (no hang) · CAPS-7 a completed `tgp;q`/`tgp;r` handshake *is* the error-reporting opt-in.

**VIEW** — VIEW-10 clamp render/alloc to on-screen intersection + advertise max viewport px (VRAM-DoS) ; inline authority = F3.

**RENDER** — MATE-7 pick target always 1× non-MSAA, never tone-mapped · MATE-9 wire colors sRGB, render linear internally, tone-map→sRGB into offscreen so composite vs text matches.

**ANIM** — ANIM-1 injectable monotonic clock (deterministic tests) · ANIM-2 clip exclusively owns a playing node's TRS; app upsert→`x node_busy` unless implicit-stop.

**RGP** — RGP-9 adapter `p` = clear-then-set (replace), `u` = sparse merge (preserve RGP semantics) ; namespace + scene model = F1/F2.

**LIFE** — LIFE-2 RIS = full scene teardown + free GPU (VRAM recovery); DECSTR leaves scene · LIFE-1 viewports bind to screen buffer of creation ; multi-process = F1.

**SEC** — SEC-5 closed versioned error-code enum (codes stable, detail advisory).

## P1 / P2
Carried in `tgp-ambiguity-findings.md` per cluster (CAPS-3/4/5/6/8/9, WIRE-4/5/6/8/10/11, SCEN-2/5/9/10/11, VIEW-1/4/5/6/7/8/9/11, EXPL-3..11, MATE-1/2/3/4/5/6/8/10/11, ANIM-3..11, RGP-3/4/5/6/7/8/10/11, SEC-2/3/4/7/9/10/11, LIFE-3/4/5/7/8/9/10/11). Each has a recommended resolution I'll adopt unless review overturns it.
