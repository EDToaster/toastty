# Decision: Redraw Policy

**Status:** proposed
**Date:** 2026-05-22
**Slug:** `redraw-policy`
**Companion prototype:** [`prototypes/redraw-policy/`](../../prototypes/redraw-policy/)

## TL;DR

Use **policy (B) damage-tracked partial redraw**, with one specialization:
**suppress submit entirely when there is no damage and no pending animation.**
Drop "FullVsync" outright — it idles a CPU/GPU at 60Hz for no reason. Drop
"Hybrid" — it adds branching without measurable benefit on the workloads we
care about, because GPU draw cost on modern hardware is dominated by per-submit
overhead, not per-instance work.

Mode 2026 sits *above* the redraw policy: the dispatcher owns a
`pause_rendering: Arc<AtomicBool>` that every renderer thread reads at frame
start. Whichever redraw policy is in force, when `pause_rendering` is true the
frame is skipped. A 1-second timeout (matching tmux) defends against stuck-BSU
apps.

## Context

toastty's architecture doc already hints at this in *Frame loop ↔ synchronized
output*:

> `Modes` exposes a `pause_rendering` flag set by the mode 2026 handler. The
> renderer reads it at frame start; if set, skip the frame and try again next
> tick. A timeout (~1s, matching tmux) forces a flush so a stuck app can't
> freeze the terminal.

That paragraph commits to the dispatcher↔renderer contract but leaves open
*what the renderer is doing the rest of the time.* Three options were on the
table:

- **(A) Full-frame redraw at vsync.** Always re-render every cell, every
  frame. Simple, deterministic, no damage bookkeeping. Used by Alacritty
  historically (before the Iosevka-era refactors).
- **(B) Damage-tracked partial redraw.** The dispatcher marks cells dirty;
  the renderer recomputes only the dirty list. Used by kitty.
- **(C) Hybrid.** Full redraw when much of the screen is changing; damage
  track when the screen is mostly idle. Sounds best of both; the prototype
  shows it isn't.

## Evidence

Hardware: Apple M4 Pro (Metal / IntegratedGpu), wgpu 29.0.3, offscreen render
target 1600×960 at 200×60 cells. Frame time = `Instant::now()` bracketing
`queue.submit` + `device.poll(Wait)`. 600 frames per (policy, workload) cell.

```
policy     workload    rendered  skipped    mean_us     p50_us     p99_us instances/frame
------------------------------------------------------------------------------------------
FullVsync  Firehose         600        0       1549       1545       1615          12000
FullVsync  SyncOutput       410      190       1547       1545       1611          12000
FullVsync  Idle             600        0       1550       1545       1632          12000
Damage     Firehose         600        0       1535       1532       1595            200
Damage     SyncOutput       410      190       1535       1531       1592             19
Damage     Idle              20      580       1533       1532       1552              1
Hybrid     Firehose         600        0       1540       1532       1580            200
Hybrid     SyncOutput       410      190       1537       1533       1592             19
Hybrid     Idle              20      580       1534       1534       1547              1

stuck-BSU: timeout fired at frame Some(2), rendered before=0, after=1
```

Workload definitions (see [`src/main.rs`](../../prototypes/redraw-policy/src/main.rs)):

- **Firehose.** Scroll one row up; rewrite bottom row. Models `yes`-like
  output. We assume virtual scrollback (row-ring), so only the bottom row is
  *content-dirty* — the rest of the grid is visually shifted but
  semantically unchanged. A naive `memcpy` implementation would mark every
  cell dirty; the prototype calls that out explicitly.
- **SyncOutput.** 16-frame cycle: frame 0 emits BSU, frames 1–4 stream 50
  cell writes each (200 total updates per cycle), frame 5 emits ESU, frames
  6–15 are idle (one status-bar cell tick). Roughly matches what neovim/tmux
  produce at 60Hz.
- **Idle.** A blinking cursor: one cell toggled every 30 frames (500ms).

### What the numbers say

1. **GPU draw time per frame is essentially constant at ~1.55ms across all
   policies and workloads, regardless of instance count.** 12000 instances
   and 1 instance render in the same wall time. The per-submit fixed cost
   (encoder + queue + device wait) dominates the per-instance work entirely.
   On this hardware, drawing more cells is free; **drawing at all is what
   costs.**

2. **Total render-thread work, per 10-second run:**

   | policy×workload    | rendered frames | total render time | "CPU %" share |
   | ------------------ | ---------------:| -----------------:| -------------:|
   | FullVsync Idle     |             600 |             929ms |        ~9.3 % |
   | Damage Idle        |              20 |              31ms |        ~0.3 % |
   | Hybrid Idle        |              20 |              31ms |        ~0.3 % |
   | FullVsync Firehose |             600 |             929ms |        ~9.3 % |
   | Damage Firehose    |             600 |             921ms |        ~9.2 % |
   | Damage SyncOutput  |             410 |             629ms |        ~6.3 % |

   Idle is the killer case: damage tracking is **30× cheaper** because it
   skips 580 of 600 frames. On a busy machine running a multiplexer with N
   shells, this is the difference between idle terminals contributing to
   fan noise and contributing to nothing.

3. **Mode 2026 frame skipping is policy-independent.** All three policies
   skipped exactly 190/600 frames in the SyncOutput workload — the BSU
   covers frames 0–4 of every 16, that's 5/16 ≈ 31.25%, and 600 × 5/16 = 187
   (off-by-three is the warm-up frame and rounding). The skip happens at the
   *scheduler* layer, above the policy. The renderer/dispatcher contract is
   what matters, not the renderer's choice of policy.

4. **Stuck-BSU timeout works.** With a 50ms timeout and 16ms frame pacing,
   the timeout fires at frame 2 (~48ms elapsed) and forces a flush. After
   the forced flush, frames continue to render only when there is new
   damage — so a misbehaving app can't trap the renderer indefinitely *and*
   doesn't get to spam frames after timeout either.

### Tearing math (no real swapchain in the prototype)

The prototype renders offscreen, so I can't *see* tearing — but the math is
straightforward:

| Scenario | Policy honors `pause_rendering`? | Visible partial state |
| --- | --- | --- |
| BSU active, vsync hits | yes | none — frame skipped |
| BSU active, vsync hits | **no** | partial grid visible for 16.7ms at 60Hz |
| BSU exceeds 1s timeout | yes | flush happens; renderer paints whatever the dispatcher left |
| BSU exceeds 1s timeout, app then sends ESU | yes | a re-paint with the *true* end state runs on the next frame |

The interesting case is the third: if the app gets stuck halfway through a
BSU we *will* show the partial state. That's correct behavior — the
alternative is freezing the terminal forever. tmux made the same call with
its 1s timeout.

There is one nastier sub-case: **ESU arrives 1ms after the timeout fires.**
Now we drew the partial state, then immediately get an ESU with the final
state. The user sees a single-frame flash of intermediate state. The fix
(noted in the contract below) is for the dispatcher to mark "BSU was
force-flushed" so the *next* frame is a guaranteed full redraw even if the
damage list is small — otherwise the partial state would persist as background.

## Why not (A) FullVsync

* 9% CPU at idle, on a fast laptop, for no user-visible benefit. Twelve
  idle terminals would consume an entire P-core.
* On battery: refusing to skip frames means refusing to coalesce with the
  display's vsync hint. macOS/Linux compositors are happy to throttle to
  30Hz or even fully suspend a window's display; FullVsync defeats that.
* Doesn't simplify anything. We *still* need a `pause_rendering` flag for
  mode 2026 — and once we have it, we have the same skip-frame machinery
  that Damage needs.

## Why not (C) Hybrid

The premise of Hybrid is that damage tracking *is more expensive* than full
redraw when most of the screen is dirty, so you'd switch to full redraw
above some threshold. The prototype shows this premise is false on the
hardware that matters:

- 12000 instances draw in 1.55ms.
- 200 instances draw in 1.53ms.

The "savings" from full redraw above the 30% threshold are inside the
measurement noise. Meanwhile Hybrid adds:

- A second code path to maintain (full-build and damage-build instance
  buffers).
- A branchy decision per frame (`if dirty > 30% of cells`).
- Coordination with the framebuffer load op: damage needs `LoadOp::Load`,
  full needs `LoadOp::Clear`. Hybrid has to switch between them per frame,
  which means the previous frame's framebuffer must be present and not
  reused for another window — fiddly on tile-based GPUs (Apple,
  recent ARM Mali).

Where Hybrid *could* win is if there's a third tier of GPU where damage
tracking has measurable per-frame setup cost — e.g., a software renderer
or WebGL2 fallback where each draw call has high CPU-side validation cost.
For toastty's target (native Vulkan/Metal/DX12), the prototype says no.

## Recommendation

### Policy

```
policy = "Damage with submit-suppression"
```

Per frame:

1. Read `pause_rendering`. If true, **skip everything**, including any
   dirty-list accumulation downstream. Increment a frame counter only.
2. Else, if `dirty.is_empty()` AND no animation timer is due, **skip
   submit**. No encoder, no command buffer, no `present()`. Just yield to
   the event loop until the next signal (input, timer, mode-2026 release,
   etc.).
3. Else, build instances from the dirty list. Render pass uses
   `LoadOp::Load` to preserve the previous framebuffer; only the dirty
   cells overdraw. Submit and present.
4. Clear the dirty list.

### Renderer/dispatcher contract

This is the contract the architecture doc gestures at; here it is in full.

```rust
// In toastty-term (owned by dispatcher):
pub struct RenderState {
    /// Mode 2026 BSU. Acquire-load on the renderer; release-store on the
    /// dispatcher. Atomic because the renderer and dispatcher run on
    /// different threads in the v1 architecture (PTY read loop ≠ winit
    /// event loop on macOS).
    pub pause_rendering: Arc<AtomicBool>,

    /// When pause_rendering went true. None when not in a BSU. Read by a
    /// timer on the renderer side (or a watchdog task) to enforce the
    /// 1s timeout. Wrapped in Mutex because it has multi-field state.
    pub bsu_state: Arc<Mutex<Option<BsuState>>>,

    /// Wake the renderer. Damage is accumulated in toastty-term's grid;
    /// when the dispatcher commits a batch of cell writes, it nudges the
    /// renderer via this notifier. The renderer's event loop blocks on
    /// this OR a winit event OR the cursor-blink timer.
    pub wake: Arc<Notify>,    // tokio::sync::Notify or equivalent
}

pub struct BsuState {
    pub started_at: Instant,
    /// If true, the next post-ESU frame must do a full redraw — used when
    /// the timeout fired mid-BSU to flush whatever partial state we have
    /// and then correct it.
    pub timeout_force_flushed: bool,
}
```

Why these specific primitives:

- **`AtomicBool` for `pause_rendering`.** It's read every frame on the
  renderer's hot path. A `Mutex<bool>` would force a lock acquisition per
  frame; a `watch::channel<bool>` introduces a tokio runtime requirement we
  don't otherwise need on this path. Acquire/release ordering is exactly
  what we need: the dispatcher batches writes to the grid *then* releases
  `pause_rendering`, the renderer acquires `pause_rendering` *then* reads
  the grid. Standard release-acquire publication.
- **`Mutex<Option<BsuState>>`** for the timeout watchdog state. This is
  touched at most once per BSU and once per second by the watchdog —
  contention is irrelevant, multi-field consistency matters.
- **`Notify` for wake.** The renderer must not poll at 60Hz when nothing
  has changed. It blocks on a wake signal. The dispatcher fires
  `notify_one()` after committing a batch; the cursor-blink timer fires it
  on tick boundaries.

The dispatcher must call `wake.notify_one()` even on ESU — the renderer
might have been parked since the BSU started.

### BSU timeout enforcement

A separate watchdog: when the dispatcher transitions `pause_rendering`
false→true, it spawns (or arms — single global task) a 1000ms timer. On
fire:

1. CAS `pause_rendering` from true→false (no-op if already cleared by an
   ESU).
2. Set `bsu_state.timeout_force_flushed = true`.
3. Drop `bsu_state` to `None` after the next frame consumes it. The next
   frame, seeing `timeout_force_flushed`, marks the entire grid dirty so
   the post-timeout paint is correct even if the app's BSU left the grid
   half-written.
4. Notify the renderer.

A simpler implementation embeds the timeout check in the renderer's frame
start: if `pause_rendering` is true *and* `started_at.elapsed() > 1s`,
force-clear and fall through to render. The prototype uses this simpler
form (see `Dispatcher::enforce_bsu_timeout`). Both are equivalent in
behavior. The watchdog form is preferable because it doesn't require the
renderer to wake up just to check a timer — it can stay parked.

### Damage data structure

Two-part: `Vec<u32>` of dirty cell indices + parallel `Vec<bool>` mask for
dedup. The prototype uses this. Alternatives considered:

- `HashSet<u32>` — high constant overhead, no benefit over the masked Vec.
- Per-row dirty bitmask (`Vec<u64>` rows) — cleaner for row-coalesced
  damage (full row updates from SGR/erase), but the iterator is more
  complex. Worth revisiting once we have real workloads. *Open question.*
- Dirty rect — too coarse: a scattered 200-cell update would force a
  bounding rect that's ~80% of the screen. Damage tracking shines on
  *small* scattered updates (status lines, syntax highlight repaints),
  which dirty-rect cannot capture.

## Open questions / followups

- **Damage primitive granularity:** cell-level (current) vs row-level
  (smaller dirty set for line-erase operations). Decide after `toastty-term`
  has real workloads. Cell-level is correct for all cases, just sometimes
  redundant.
- **Should `Mutex<BsuState>` be `parking_lot::Mutex`?** wgpu already pulls
  it in by default — would be free. Likely yes.
- **Renderer wake source on macOS:** `winit::event_loop::EventLoopProxy`
  can be used instead of `Notify` to avoid the cross-thread `Notify` —
  worth checking that the macOS main-thread requirement for window events
  doesn't make `Notify` from a non-main thread awkward.
- **Animation cursor blink** must keep the renderer awake on a timer.
  Suggest a single shared timer service in `toastty-render` that owns a
  `Vec<(Deadline, Wake)>` and ticks at the lowest-frequency required
  animation (typically 2Hz for blink). Don't tie this to vsync.
- **Damage during fast scroll (terminfo-style):** if we implement a
  scroll-region-aware grid, scroll becomes a single "shift rows" event,
  not a memcpy + full damage. That decision lives in `toastty-term`, but
  the renderer assumes the cheap-scroll case (only the new bottom row
  dirty, plus a scroll-offset uniform). Documenting this here so it isn't
  forgotten.
