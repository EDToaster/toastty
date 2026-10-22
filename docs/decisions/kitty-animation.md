# Decision: kitty graphics animation

**Status:** accepted (stack E, E1)
**Date:** 2026-10-21
**Slug:** `kitty-animation`

## TL;DR

toastty implements the kitty graphics protocol's animation: `a=f` frame
data, `a=a` animation control, `a=c` frame composition, and the
`d=f` / `d=F` frame delete. The model and every reply follow kitty 0.49.2,
measured live against the reference with a conformance probe (below).

- Frames live with their image in the registry
  (`toastty-graphics/src/frames.rs`, an `Animation` attached to the
  registry entry). They are stored LZ4-compressed, in their own budget of
  twice the image quota.
- What `ImageRegistry::get(id)` returns is the frame on display, composed.
  So every placement and every Unicode placeholder draws the current
  frame, and the renderer needs no knowledge of animation.
- Playback is driven from the event loop's redraw handler. The loop sleeps
  until the next frame is due, and only while an animation is running on a
  displayed image. An idle terminal does no animation work.

## Frame storage

Frames are stored the way kitty stores them, not as full canvases:

- Each frame is a rectangle of RGBA pixels (`x`, `y`, `s`, `v`).
- Under the rectangle is either a background colour (`Y=`, RGBA with R
  in the high byte, default transparent black) or a *lazy reference* to
  a base frame (`c=`).
- A frame's full pixels are composed on demand ("coalesced"): its base,
  coalesced, then its own rectangle alpha-blended on top (or overwritten
  with `X=1`).

Two kitty behaviours follow from this, and both were checked live:

- **Live references.** Editing a base frame later (`a=f,r=`, or `a=c`
  onto it) shows through in every frame built on it.
- **Cheap deltas.** A GIF sent as deltas costs only its deltas.
  `kitten icat` sends every GIF frame with `c=<previous frame>`.

Kitty's rules, reproduced exactly:

- **Coalesce on create.** A new frame is coalesced at once, cutting the
  reference, when its base chain is long. The test is kitty's
  `reference_chain_too_large`: five frames, or two image areas of drawn
  pixels.
- **Edits.** Editing a frame (`a=f,r=N`), or composing onto it (`a=c`),
  coalesces it into a full frame with no base. `c=` and `Y=` don't apply
  to an edit.
- **Full-size frames.** A full-size frame with no base is shown as sent:
  `Y=` doesn't show through its translucent pixels.
- **Blend arithmetic.** Alpha blending uses kitty's straight-alpha `over`,
  with the same `f32` arithmetic and truncation. A half-red pixel over
  white is `(255,126,126)`, as in kitty.

The registry keeps the current frame composed in the entry's display
buffer. When playback moves to a frame built on the frame on display (the
GIF case), only the new frame's rectangle is composed onto the buffer, so a
tick costs the delta, not the image. Any other change recomposes the frame.

**Bounded work and memory.** A client can repeat any frame command
cheaply, so none of them may scale with the frame count or allocate
outside the budget (#47 review):

- `Animation` keeps a running byte total, a count of frames with a gap,
  and an id → index map, rather than recomputing them. Adding a frame and
  a tick are O(1) in the number of frames. 1M one-pixel frames load in
  1.3 s; before, 80k took 5.6–8 s and it grew quadratically.
- Each frame is charged `FRAME_OVERHEAD` on top of its pixels: the
  `Frame` itself plus about 96 bytes of allocation and index entries.
  1M tiny frames are charged 172 MB, against 154 MB of real RSS, so the
  budget bounds them.
- **Deleting a frame others are built on** (`d=f`) keeps it as a
  *hidden base*. It has no number, but it keeps its pixels for those
  frames and is freed, in cascade down its own chain, when the last
  frame built on it goes, by deletion or by an edit that makes it
  self-contained. The delete allocates nothing, and the dependents keep
  their pixels. Coalescing the dependents instead, as this PR first did,
  turned 300 one-pixel frames on a 1000×1000 root into +1.1 GB outside
  the budget.

## Commands

**Frame numbers and replies.** Frame numbers are 1-based; frame 1 is the
root (the transmitted image).

- `a=f` targets the frame `r=` names, or a new frame after the last when
  `r=` is 0 or past the end.
- `c=` naming the frame being created (`c=2` while making frame 2) is
  OK. kitty adds the frame before looking its base up, so the frame
  finds itself. We draw it on the background.
- kitty tags the reply with that frame, errors included:
  `i=1,r=2;OK`, `i=1,r=3;EINVAL:No frame with number: 9 found`.
- Error order is decode errors (`ENODATA`, `EINVAL:Zero width/height…`,
  `EBADPNG`, inflate), then frame size (`Frame width N larger than image
  width: M`), then base frame. New frames that don't fit get
  `ENOSPC:Cache size exceeded cannot add new frames`.

**Gaps.** `z=` sets the gap: 0 keeps the default (40 ms for a new frame;
the root starts at 0), and a negative value makes the frame gapless.

**Chunked frames.** These go through the transmit chunk machinery (the
#37 review asked that continuations route correctly):

- The frame keys come from the first chunk.
- The ids in the reply come from the last chunk when it says `a=f`. That
  is usually none, so no reply.
- A bare `m=0` last chunk is answered with the first chunk's ids.
- A first chunk naming an unknown image gets ENOENT. Its id-less
  continuations are then dropped.

All of this is as measured in kitty.

**`a=a` (animation control).**

- `r=` with `z=` sets a frame's gap (`z=0` ignored).
- `c=` makes a frame current (out of range: ignored).
- `s=1` stops, `s=2` runs but waits at the last frame for more (loading),
  and `s=3` runs.
- `v=` sets the loop count: 0 ignored, 1 forever, N plays N-1 loops.
  `v=2` plays the frames once and stops on the last.
- Stopping resets the loop counter. Starting again (`s=2`/`s=3` after
  `s=1`) gives the current frame its full gap from then, as in kitty.
- New images start stopped.
- On an image with no frames yet, `a=a` only records the root gap, the
  state and the loop count, which apply once frames arrive. No root copy
  is made until then, so `a=a` allocates nothing.
- `a=a` never replies, except ENOENT for an unknown image, so `kitten
  icat`'s control commands (sent without `q=`) put nothing in the shell.

**`a=c` (composition).**

- It copies a `w`×`h` rectangle (default: the whole image) from frame
  `r=` at `X`,`Y` onto frame `c=` at `x`,`y`, blending unless `C=1`.
- Errors, in kitty's order and words:
  - source frame missing: ENOENT,
    `No source frame number N exists in image id: M\n`;
  - destination frame missing: ENOENT, same form;
  - destination rectangle out of bounds: EINVAL;
  - source rectangle out of bounds: EINVAL;
  - same frame with overlapping rectangles: EINVAL.
- The ENOENT texts end in a newline, as kitty's do.
- A success replies `OK`, keyed by the client's own `i=` / `I=`.

**`d=f` / `d=F`.** These delete frame `r=` (0 is the root; past the end
clamps to the last). The current frame moves exactly as kitty's index
arithmetic moves it: kitty compares the deleted frame's index among the
non-root frames with the current frame's index among all frames.

With frames left, `d=F` also deletes just a frame. On a single-frame image,
`d=F` deletes and frees the image and `d=f` does nothing (DEL-2's rule).

## Playback

`Animation::tick(now)` is kitty's `scan_active_animations`:

- When the current frame's gap has passed, it advances, skipping gapless
  frames.
- At the last frame, a loading animation waits and a running one wraps,
  until its loops are used up.
- It returns when the next frame is due.
- A frame due within `ANIMATION_SLACK` (10 ms) of the tick is shown in
  it, so the frames of several animations that fall close together share
  one redraw. kitty batches repaints the same way (`repaint_delay`).
  An early frame keeps its own schedule: its gap counts from when it was
  due, so playback doesn't drift. Twelve offset copies of a GIF went from
  12.8–13.5% to 5.9–7.5% CPU.

`Term::tick_animations(now)` ticks the animations of images that have a
placement on the active screen, real or virtual (a `U=1` placement for
Unicode placeholders). kitty also animates only images it draws.
It marks their placements dirty and bumps `image_revision` and the
registry generation. It is a no-op when no animation is running.

`main.rs` calls it at the top of every redraw and adds
`Term::next_animation_frame()` to the redraw deadlines.

For the loop to actually sleep between frames it needs #50 (PERF-1).
Without it, `ControlSignal::RedrawIn(d)` redraws immediately as well as
setting the deadline, so any pending deadline (a blinking cursor, or a
running animation) spins the loop. Playback is correct either way; #50
is what keeps it at a few percent CPU. The figures below were measured
with it.

## Rendering and the texture cache

The renderer needs no changes. C1's texture cache (#24) keys each
image's texture on `ImageRegistry::generation(id)`. The registry keeps
C1's semantics, a new value on every insert, and adds one: a new value
on every change of the displayed frame. So a frame change drops the old
texture and uploads the new frame on its next draw. That covers real
placements and Unicode placeholders, which bind through the same
`resolve_kitty_textures`.

Caching each frame as its own texture would mean keying the cache on
`(id, frame, epoch)` instead of `id`, so frames cycling within the GPU
budget stop re-uploading. We measured whether that is worth doing: a
400-frame 480×270 GIF at 25 fps costs about 5% CPU with re-uploads, and
a 240×120 one 2%. It is left as a possible follow-up.

## Quota

kitty keeps frames in a disk cache with a separate quota, five times the
image quota. toastty keeps them in RAM, and bounds them two ways.

**A separate frame budget.**

- Stored frames, root copies included, count against
  `FRAME_BUDGET_FACTOR` (2) × the image cap: 640 MB by default.
- The image cap counts only displayed images, an animated image's
  display buffer included. So frames can't push images out, and
  vice versa.
- To make room for a frame, the registry may evict *animated* images
  without placements, oldest first. It never evicts placed ones or the
  animation itself. Failing that, the frame gets kitty's
  `ENOSPC:Cache size exceeded cannot add new frames`.
- The root's copy, made when an image gets its first frame (or `a=c`),
  goes through the same check.
- Worst case, everything resident is bounded at 320 MB of images plus
  640 MB of frames.

**LZ4 compression.**

- Frame pixels are stored compressed with `lz4_flex` (pure Rust, safe
  mode). A block stays raw if compression saves less than an eighth, or
  if it's under 4 KiB.
- Pixels are decompressed only when a frame is composed: a new frame on
  display, an edit or a composition.
- A self-contained full frame (the root, or one coalesced on create)
  decompresses straight into the display buffer, reusing its allocation.
- LZ4 decompresses at memory speed, so this costs little CPU.
- GIF frames are palette images, so even a dithered, photographic one
  compresses several times.
- zlib (already a dependency) compresses better, but inflating a 2 MB
  frame takes milliseconds. At 25 fps that is 10–15% of a core, against
  about 1% for LZ4.

Measured (Xvfb, fresh instance, `kitten icat`, 10 s average; RSS over a
~100 MB baseline):

| GIF | before (shared quota, raw) | after (frame budget, LZ4) |
|---|---|---|
| 800×600, 300 full frames (576 MB raw) | plays ~163 frames; +319 MB; 6.1% CPU | **all 300 frames**; +0 MB; 9.5% CPU |
| 480×270, 400 full frames (207 MB raw) | all frames; +187 MB; 4.8% CPU | all frames; **+0 MB**; 5.8% CPU |
| 640×360, 150 dithered photo frames (138 MB raw) | all frames; +126 MB; 4.1% CPU | all frames; **+35 MB**; 5.6% CPU |

The test GIFs with flat colour compress to almost nothing. The dithered
one, a worst case for GIF, compresses 3.6×. The extra CPU is the
decompression of a full frame per tick.

## Deliberate divergences from kitty

- **Frame storage.** Frames live in RAM, LZ4-compressed, in a 2× budget;
  kitty has a 5× on-disk cache. Animations past the budget get ENOSPC
  sooner than in kitty, though compression makes that rare.
- **Deleting a base frame.** toastty keeps it as a hidden base, so the
  frames built on it keep their pixels. kitty leaves them pointing at a
  missing frame and keeps showing a stale frame.
- **Editing a base frame under the frame on display.** Editing the root
  (`a=f,r=1`) while frame 2 (built on it) is on screen redraws frame 2
  with the edit at once. kitty keeps showing the old frame 2 until the
  frame changes. An edit to a frame the one on display doesn't depend on
  doesn't touch the display at all (no recompose, no re-upload).
- **Leading comma.** As for every reply (`kitty-replies.md`), an
  `I=`-only reply has no stray leading comma (`I=7,r=2;OK`, not
  `,I=7,r=2;OK`).

## Verification

`scripts/kgp-stress/anim-probe.py` ran 86 cases in kitty 0.49.2 and in
toastty: 52 reply cases and 34 pixel cases, with screenshots.
`anim-diff.py` compares two runs' replies.

- The probe runs inside the terminal. For the screenshots it asks a
  driver via `SHOTDIR/req`; the driver takes one and answers with
  `SHOTDIR/ack`.
- kitty on Xvfb (software GL) sometimes skips a repaint. Use `SETTLE=1.5`
  and retake blank shots.

- **Replies.** All are byte-identical. The only differences are the
  leading comma and image-id numbering caused by an unrelated `d=A`
  behaviour.
- **Pixels.** All 34 match, except the base-frame delete above.

The behaviour is locked by these tests:

- `frames.rs`: compose, blend, chains, playback, deletes;
- `registry.rs`: quota, eviction, generations;
- `handler.rs`: replies, chunking, error order;
- `term/animation_tests.rs`: round trips through `Term`, including an
  icat-style GIF stream and chunked frames.
