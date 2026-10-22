# Kitty graphics protocol: stress-test checklist

A live stress test of toastty's kitty graphics protocol (KGP) support. It
covers main @ `c0a5336`, October 2026. Each unchecked item is a known gap.
Tick it off in the PR that fixes it and link the PR.

This replaces [`kitty-protocol-audit.md`](kitty-protocol-audit.md) as the list
of open KGP work. That file was a static code read. Most of its items have
since been fixed, and it missed most of what is below. Sixel is out of scope.

How this was checked:

- **Live runs.** toastty and reference **kitty 0.49.2** ran side by side on
  private Xvfb displays (see [AGENTS.md](../AGENTS.md#running-toastty-on-a-virtual-screen)).
  Each test sent the same bytes to both terminals. The screenshots and
  reply transcripts were then compared.
  - Images were measured with PIL, so "stretched" and "cropped" are measured
    results, not eyeballed.
  - toastty used 10×19 px cells and kitty 9×20 px cells, so sizes are
    compared in cells and aspect ratio.
- **Spec.** The reference text is `graphics-protocol.rst` as shipped with
  kitty 0.49.2 (`/usr/share/doc/kitty/html/_sources/graphics-protocol.rst.txt`).
  - Where kitty and the spec disagree, the item says so.
  - The default target is kitty's behaviour, since that is what clients are
    tested against.
- **Real clients.** `kitten icat` (all transfer modes, placeholders, GIF),
  yazi, tmux passthrough, `mpv --vo=kitty` and `toastty image`.
- **Tools.**
  - [`scripts/kgp-stress/kgp.py`](../scripts/kgp-stress/kgp.py) runs inside
    the terminal under test. It generates test-pattern PNGs and sends KGP
    commands with optional chunking, zlib and reply capture.
  - The test pattern has four coloured quadrants, a 10 px grid, a diagonal
    and a border, so crop, squash and stretch are easy to tell apart.
  - [`scripts/kgp-stress/kitty-ref.sh`](../scripts/kgp-stress/kitty-ref.sh)
    starts reference kitty on a private display.

Legend:

- **P0** breaks common apps, is visibly wrong in everyday use, or is a
  security, crash or hang problem.
- **P1** is wrong or missing behaviour that spec-following clients will hit.
- **P2** is a minor divergence.

Repro snippets use `K` for `python3 scripts/kgp-stress/kgp.py` and
`img.png` for a test pattern made with `K img img.png 200x100`.

## 0. Top issues

Fix these first. Most of the rest follows from the first three.

1. **Placements are screen-relative and never reach scrollback.** See §1.
   Every scroll squashes an image instead of cropping it. Scrolling back
   leaves images pinned over unrelated text. Images that scroll off the
   screen are lost.
2. **Images are stretched to whole cells.** See §2. Nothing is drawn at its
   native pixel size, and partial-screen clipping squashes the image instead
   of cropping it.
3. **Unicode-placeholder images are detached from their cells.** See §3.
   They don't erase or move with the text, they aren't scaled to the
   placement, and they're lost in scrollback. This breaks tmux,
   `kitten icat --unicode-placeholder` and image.nvim.
4. **Only 14 images can be on screen.** See SIZE-7.
5. **`t=t` can delete files outside the temp dir.** See SEC-1. Any program
   that writes to the PTY can trigger it, including `cat` of a file or a
   remote host's output.
6. **`t=f` on a FIFO or `/dev/zero` hangs the terminal or exhausts memory.**
   See SEC-2 and SEC-3.
7. **Text gets injected into the shell.** See APP-1 and DEL-1. ENOTSUP
   replies to `kitten icat`'s GIF animation commands, and OK replies to
   deletes, land in readline as typed text.
8. **`a=p` on an unknown id replies OK.** See PLC-1. Clients that cache
   images never re-send them.

---

## 1. Placement model, scrolling and scrollback

Shared root cause:

- `ImageGrid` stores each placement as a screen-relative `u16` row/col range
  (`crates/toastty-graphics/src/image_grid.rs:22-60`).
- Scrolling and clipping trim `row_range`/`col_range` but never adjust
  `src_rect`.
- `build_image_instances` (`crates/toastty-render/src/image/instance.rs:121-178`)
  maps the full source rect onto whatever cells remain. It never applies
  `view_offset_lines` or `view_offset_pixel`.
- No image state is kept in scrollback.

The fix is probably one redesign: anchor placements to absolute line ids
(like `Grid::bottom_id`), keep a pixel-space destination rect, and crop the
UVs at draw time. Don't patch each item separately.

- [x] **P0 — SCR-1. Partly scrolled-off images are squashed, not cropped.**
  - Repro: `K send a=T,i=1,c=20,r=10 --png img.png`, then `seq 1 25`.
    The image ends up 3 rows above the top edge.
  - Expected (spec, "Interaction with other terminal actions"): images
    scroll with the text and "must be clipped". kitty crops the top rows.
  - Actual: the whole image is compressed into the rows still on screen.
  - The same happens on every scroll path:
    - LF at the bottom, and SU (`CSI S`).
    - SD (`CSI T`) and RI, which squash at the bottom edge.
    - DECSTBM region scroll, and IL pushing the image past the bottom.
    - An `a=T` taller than the screen: r=45 on 30 rows is squashed into 29.
    - Stacks of icat images.
  - Cause: `image_grid.rs` `shift_rows_up` (356-392), `shift_rows_down`
    (318-347), `shift_rows_up_within` (445-488) and `shift_rows_down_within`
    (402-436) clamp the range but leave `src_rect` alone.
  - Fixed in #29. Placements are anchored to line ids and keep their whole
    cell box; the renderer crops at draw time. See
    [`decisions/image-placement-model.md`](decisions/image-placement-model.md).
- [x] **P0 — SCR-2. Images don't follow the viewport into scrollback.**
  - Repro: as SCR-1, then scroll up with the mouse wheel.
  - Expected (spec): "When scrolling the screen (… or scrolling through the
    history buffer), images must be scrolled along with text". kitty
    scrolls the image with its rows, and shows the full uncropped image
    when you scroll back to it.
  - Actual:
    - The image stays at its live-screen rows, drawn over unrelated history
      text, while its own rows show blank.
    - An image that scrolled fully off the top (e.g. after `seq 1 40`) is
      gone for good.
    - Same with `smooth_scrolling = false`.
  - Cause:
    - `instance.rs:121-178` doesn't apply the viewport offset. Text does
      (`crates/toastty-render/src/lib.rs:1832-1838, 1876`).
    - `image_grid.rs:382-386` drops placements at row 0.
  - Fixed in #29. Placements live in scrollback until their lines are
    evicted, and the renderer applies the viewport offset.
- [x] **P0 — SCR-3. New output arriving while scrolled back moves the image.**
  - Repro: scroll back one notch while a background job prints 6 lines.
  - Expected: kitty keeps the text and the image still.
  - Actual: toastty keeps the text anchored, but the image moves up
    2 rows and loses 2 rows.
  - Cause: same as SCR-2.
  - Fixed in #29. Same fix as SCR-2: the image is anchored to its line, so
    the viewport compensation that holds the text still holds the image
    too.
- [x] **P1 — SCR-4. Pixel (smooth) scrolling moves text but not images.**
  - Repro: set `smoothing_function = "linear"` and capture frames during a
    wheel animation.
  - Actual: text slides at fractional-row offsets under an image that stays
    on the cell grid.
  - Cause: `lib.rs:2860-2878` builds image instances without the
    `y_translate` that text and rich blocks get (`lib.rs:1835, 1975, 2763`).
  - Fixed in #29. Image instances get the same translation as text
    (`view_offset_lines * cell_h + view_offset_pixel`), and are cropped at
    the window edge.
- [x] **P1 — SCR-5. DECSTBM scrolls images that are only partly inside the
  margins.**
  - Repro: place an image on rows 3–8, then `K raw '\x1b[2;7r\x1b[7;1H\n\n\x1b[r'`.
  - Expected (spec): "only images that are entirely within the page area
    (between the margins) must be scrolled". kitty leaves this image alone.
  - Actual: it moves up 2 rows and is clipped into 5 rows.
  - Cause: `image_grid.rs:423` and `:469` check only `row_range.start`;
    they also need `end <= scroll_bottom`.
  - Fixed in #29. Only placements entirely inside the margins scroll, and
    they are clipped at the margin.
- [x] **P1 — SCR-6. IL/DL (`CSI L` / `CSI M`) move images; kitty doesn't.**
  - Repro: place an image, put the cursor above it, then `K raw '\x1b[2L'`
    or `'\x1b[2M'`.
  - Expected: kitty 0.49.2 leaves images in place and moves only the text.
    The spec lists only index commands and history scrolling.
  - Actual: toastty shifts the image with the text, and squashes it if
    pushed past the bottom (SCR-1).
  - Cause: `crates/toastty-term/src/term.rs:3349-3366, 3429-3441`. The
    comments there say "kitty shifts images on IL/DL", which the reference
    contradicts.
  - Decide deliberately: following the text is arguably more useful. If
    toastty keeps it, document it as a divergence.
  - Fixed in #31. Matches kitty: IL/DL move only the text. Re-checked on
    kitty-ref: kitty also keeps an image whole when DL deletes the rows
    under it, so toastty does not clip it either. Placeholder images move,
    because they are text: their per-cell tiles keep the old shift until
    #28 renders placeholders from the cells.
- [x] **P1 — SCR-7. Resize and reflow separate images from their text and
  truncate them permanently.**
  - Repro: place an image, shrink the window to 8 rows, then grow it back.
    Or narrow it to 15 columns and widen it again.
  - Expected: kitty moves the image with the reflowed text, crops it at the
    window edge, and restores it in full when the window grows.
  - Actual:
    - The text moves but the image doesn't, and the image is squashed.
    - After growing back, the image stays squashed, or stays cut to
      15 columns after a width change.
  - Cause:
    - `term.rs:1813-1869`: `reflow_primary` doesn't move placements.
    - `ImageGrid::clip_to` (`image_grid.rs:491-514`) trims the span for
      good, without touching `src_rect`.
  - Fixed in #31. The primary reflow now reports where each retained row
    went (`Grid::resize_reflow_mapped` / `LineRemap`), and placements follow
    their text, including the stashed primary grid while on the alt screen.
    Alt placements keep their rows. Boxes are never trimmed or dropped for
    hanging off the edge, so shrinking then growing restores the image.
    This covers images in scrollback too: a height change keeps them on
    their history text instead of their screen row, so growing (which
    reveals history) no longer evicts one near the oldest line. The
    column follows too: the top-left cell maps as a content offset into
    its logical line, so an image at column 70 of a 90-cell line moves
    to the next row when narrowed to 58 columns, and back on widening.
  - Correction to "Expected", from kitty-ref: kitty follows the text only on
    height changes. On a width change kitty leaves the image at its screen
    row while the text rewraps under it. toastty follows the text on width
    changes too, as the PM decided (a deliberate divergence; see
    [`decisions/image-placement-model.md`](decisions/image-placement-model.md)).
- [x] **P1 — SCR-8. Relative placements drift once their parent scrolls off
  the top.**
  - Repro: a child placed at `V=6` under a parent, then output that scrolls
    the parent partly or fully off the top.
  - Expected: kitty keeps the child at its offset from the parent, cropped
    at the top.
  - Actual: the child drifts by 2–3 rows and is drawn in full at the top.
  - Cause: `resolve_relative_positions` (`image_grid.rs:201-249`) uses the
    parent's clamped row. Once the parent is dropped, the child loses its
    anchor.
  - Partly addressed in #29: placements now stay in scrollback and children
    resolve as `parent.line + V` with no clamp, so a child keeps its offset
    while the parent scrolls off the top. Left open for A4 to verify against
    kitty and cover the remaining cases.
  - Fixed in #40. Checked on kitty-ref with a small scrollback: a
    parent leaving the history takes its children with it, even when their
    own rows are still retained.
- [x] **P1 — SCR-9 (text bug found while testing scrolling). SD and RI at
  the top pull scrollback back onto the screen.**
  - Repro: `seq 1 100`, then `K raw '\x1b[3T'`.
  - Expected (xterm, kitty): blank lines are inserted at the top.
  - Actual: the most recent history rows reappear on screen, and the
    history shrinks.
  - Cause: `crates/toastty-term/src/grid.rs:299-316`. `Grid::scroll_down`
    rotates the `head - 1` history slot back in, despite its doc comment
    ("a fresh blank row appears at the top"). Called from `term.rs:2050`.
  - Fix: `scroll_down` now rotates the visible slots in place and leaves
    `history_lines` and `bottom_id` alone, matching kitty 0.49.2. Fixed in
    #23.
- [x] **P0 — SCR-10. An image that doesn't fit below the cursor always
  scrolls the screen, even with `C=1` and in the alt screen.**
  - Repro:
    - Put the cursor on the last row and run
      `K send a=T,i=1,C=1,c=10,r=5 --png img.png`.
    - Or do the same inside a full-screen TUI on the alt screen.
  - Expected: kitty clips the image at the bottom and doesn't scroll.
  - Actual: toastty scrolls the whole screen. Marker lines go into
    scrollback, and a full-screen TUI loses its top lines.
  - Combined with SIZE-1's rounding up, even an app that sizes an image to
    fit exactly can overflow by a row.
  - Cause: `term.rs:4697-4712` always scrolls to make room, ignoring `C=1`,
    the alt screen and DECSTBM. The comment there misreads the spec, which
    says nothing about scrolling.
  - Fixed in #29. Placing never scrolls. Like kitty, only the cursor
    advance after a non-`C=1` placement scrolls the region by the
    overshoot. kitty-ref check: with `C=0` kitty *does* scroll the alt
    screen too, so toastty does the same; only `C=1` prevents the scroll
    on both screens.
- [x] **P2 — SCR-11. Scrolling inside DECSLRM left/right margins leaves
  images in place.**
  - Repro: `K raw '\x1b[?69h\x1b[5;40s\x1b[3;12r'`, then line feeds inside
    the margins.
  - Expected: kitty doesn't implement DECSLRM; it does a plain region scroll
    and crops the image.
  - Actual: toastty moves the text inside the margin rectangle, but the
    image stays over different text.
  - Cause: `term.rs:1975-1981, 2042-2046` return before touching
    `image_grid`.
  - Fixed in #31. Inside DECSLRM margins the text moves only within the
    margin rectangle, and images whose columns lie inside the side margins
    move with it, clipped at the top/bottom margins. Images outside a side
    margin, or straddling one, stay with their (unmoved) text. kitty has no
    side margins, so it moves the whole region; inside the rectangle the
    two agree.
- [x] **P2 — SCR-12. Alt screen modes 47 and 1047 clear text and images on
  re-entry; kitty keeps them.**
  - The spec requires clearing only for 1049.
  - toastty is consistent between text and images, so the impact is low.
  - Code: `term.rs:3795-3850`.
  - Fix: matches kitty 0.49.2, checked live with CPR probes and
    screenshots.
    - 47 / 1047 keep the alt text and images between visits. Entry homes
      the cursor and resets the pen. Exit doesn't restore the cursor.
    - 1049 saves the cursor and clears the alt screen (text and images) on
      entry, and restores the cursor on exit.
    - xterm differs: it doesn't home on 47 / 1047, and 1047 reset clears
      the alt screen before leaving.
    - Divergence kept on purpose: kitty 0.49.2 ignores `CSI ? 1048 l`, so
      the cursor isn't restored. toastty keeps xterm's save / restore.
    - Fixed in #27.

- [x] **P2 — SCR-13. ED 3 (`CSI 3 J`) keeps the scrollback text; kitty erases
  it.** Found in the #29 review.
  - Expected (kitty 0.49.2, checked live): `CSI 3 J` erases the screen
    (text and images) and the whole scrollback. The cursor doesn't move. A
    view scrolled back at the time snaps to the live screen. On the alt
    screen it erases the alt screen and leaves the main scrollback alone.
  - xterm differs: its ED 3 erases only the saved lines and leaves the
    screen. toastty follows kitty.
  - `clear` sends ED 3 only when the terminfo has `E3`.
    - toastty runs with `TERM=xterm-256color`, which has it, so `clear`
      erases the history, as in xterm, VTE and alacritty. That is the
      intended behaviour (PM decision).
    - kitty's own `xterm-kitty` terminfo has no `E3`, so `clear` keeps the
      scrollback in kitty.
  - Actual: toastty treated 3 exactly like 2, so the scrollback text
    survived ED 3, and therefore `clear`.
  - Fix: `Grid::clear_history` blanks the history ring. ED 3 calls it on
    the primary screen, snaps the viewport to the live screen (a pixel-only
    touchpad offset included), and drops any selection. Fixed in #38.
- [x] **P1 — SCR-14. An image taller than the screen scrolls it by the
  full overshoot.**
  - Repro: a natural 100×2000 image placed at row 6 without `C=1`.
  - Expected (kitty-ref 0.49.2): the screen scrolls at most one screen
    (region) height. On a 30-row screen the image top lands 25 rows above
    the screen, so source rows 500..1100 are visible, and the cursor sits
    on the last row.
  - Actual: toastty scrolled the full overshoot (up to one ring), so the
    image's bottom landed on the cursor row and a different slice showed.
  - Fixed in #29: the cursor-advance scroll is capped at the region height.

## 2. Sizing, scaling and rendering

- [x] **P0 — SIZE-1. Images are stretched to whole cells instead of drawn at
  native size.**
  - Repro: `K send a=T,i=1 --png img.png` with images of 205×105, 40×40, 3×50
    and 1×1.
  - Expected (spec): the cell count (`ceil(img/cell)`) is only a
    reservation. kitty draws each image at its exact pixel size.
  - Actual:
    - 205×105 is drawn at 210×114, and 40×40 at 40×57, so the aspect ratio
      is distorted.
    - 3×50 becomes 10×57.
    - The resampling blurs pixel-exact content.
    - `kitten icat --scale-up` comes out with the wrong aspect ratio
      (1.33 becomes 1.13).
  - Cause: `instance.rs:149-154` sizes the quad as `cols*cell_w × rows*cell_h`.
    `Placement` has no pixel size (`crates/toastty-graphics/src/kitty/handler.rs:825-900`).
  - Fixed in #39. Drawn at the source's native pixel size, with the origin
    snapped to whole pixels, so texels map 1:1. The cell box is only the
    reservation. Measured: 205×105 → 205×105 (was 210×114), 40×40 → 40×40,
    3×50 → 3×50, 1×1 → 1×1.
- [x] **P1 — SIZE-2. Giving only `c=` or only `r=` distorts the image.**
  - Repro: a 400×200 image with `c=10`.
  - Expected (spec): "the other one is computed based on the source image
    aspect ratio, so that the image is displayed without distortion". kitty
    draws it at 90×45.
  - Actual: rows round up to 3, and the image is stretched to 100×57.
  - Cause: `handler.rs:857-869` plus SIZE-1.
  - Fixed in #39. The other side follows the source aspect, rounded to
    whole pixels, like kitty (400×200 `c=10` → 90×45 on kitty's 9 px cells,
    100×50 on toastty's 10×19).
- [x] **P2 — SIZE-3. Giving both `c=` and `r=` stretches the image; the spec
  says letterbox.**
  - Spec: "When both are specified the image is letterboxed/pillarboxed to
    prevent distortion".
  - kitty 0.49.2 actually stretches too (36×160 for `c=4,r=8`), so toastty
    currently matches kitty.
  - Decide which one to follow, then add a test.
  - Decided in #39: match kitty 0.49.2. Re-verified on kitty-ref: `c=4,r=8`
    on 200×100 is 36×160, stretched to the box less any `X`/`Y`. Locked by
    `kitty_ref_sizes_match`.
- [x] **P1 — SIZE-4. The cell span and aspect ratio come from the full image,
  not the source rect.**
  - Repro: a 200×100 image with `x=0,y=0,w=60,h=40`, with no `c=`/`r=`, then
    with `c=12`.
  - Expected: kitty draws it at 60×40 and 108×72.
  - Actual: toastty draws 210×114, a blurry 3.5× zoom, and 120×76.
  - Cause: `handler.rs:851-869` uses `img_w`/`img_h` instead of the source
    rect's size.
  - Fixed in #39. The span and aspect come from the resolved source rect
    (`w=60,h=40` → 60×40; with `c=12` → 120×80 on 10 px cells, kitty 108×72
    on 9 px).
- [x] **P1 — SIZE-5. A partial source rect is ignored.**
  - Repro: `w=` only, `h=` only, `x=` only or `y=` only.
  - Expected (spec): the defaults are "the entire width/height" from
    `x`/`y`. kitty crops accordingly.
  - Actual: the full image is drawn.
  - Cause:
    - `handler.rs:833` treats `w==0 && h==0` as full.
    - `SrcRect::is_full()` (`image_grid.rs:86-88`) treats `w==0 || h==0` as
      full.
    - So any rect with one dimension unset collapses to the full image.
  - Fixed in #39. The rect is kept as given and resolved with the spec's
    defaults (`SrcRect::resolve`). `w=50`, `h=30`, `x=150` and `y=70` alone
    give 50×100, 200×30, 50×100 and 200×30, as in kitty.
- [x] **P1 — SIZE-6. The source rect isn't intersected with the image.**
  - Repro: a rect partly outside the image, fully outside it, or larger
    than it.
  - Expected (spec): "The displayed area is the intersection of the
    specified rectangle with the source image rectangle". kitty draws a
    55 px slice, nothing, and the full image respectively.
  - Actual: smears of the image's edge pixels, and a solid block for the
    fully-outside rect.
  - Cause:
    - `instance.rs:151-160` computes UVs past 1.0.
    - The sampler clamps to the edge (`crates/toastty-render/src/image/pipeline.rs:164-166`).
  - Fixed in #39. Intersected with the image: partly outside → 50×100,
    fully outside → nothing, oversized → the full image, matching kitty.
- [x] **P1 — SIZE-7. Images wider or taller than the space left are squashed,
  not truncated.**
  - Repro: a 1500×100 image, or `c=300,r=100`, or an image placed with the
    cursor mid-line.
  - Expected (spec): "images wider than the available width will be
    truncated on the right edge". kitty crops at native scale.
  - Actual: toastty compresses the whole image into the remaining box.
  - Cause:
    - The clamp at `term.rs:4717-4725` trims cells but not `src_rect`.
    - In pending-wrap state the image is squashed into the last column
      (`term.rs:4685`).
  - Fixed in #39. The box keeps its size and is cropped at native scale at
    the grid edge (1500×100 at column 20 shows its first 800 px). In the
    pending-wrap state kitty puts the image past the right edge (invisible)
    and wraps the cursor; toastty now does the same.
- [x] **P0 — SIZE-8. At most 14 images render; the rest silently disappear.**
  - Repro:
    - 20, 40 or 500 placements of distinct images, e.g. a grid of
      `kitten icat --place` calls.
    - Or transmit 18 new images without placing them; that hides 2 images
      already on screen.
  - Expected: kitty shows all of them.
  - Actual: only the 14 most recently uploaded textures draw, with no error.
  - Cause:
    - `ImageTextureCache` is capped at 14 entries
      (`crates/toastty-render/src/image/atlas.rs:36-44`, set at `lib.rs:998`)
      and evicts least-recently-inserted.
    - `sync_registry` (`pipeline.rs:191-262`) uploads every image in the
      registry, placed or not, and re-uploads non-resident ones on every
      image change: about 25 % extra CPU when thrashing.
    - `instance.rs:129-134` skips placements whose texture isn't in the
      cache.
    - The cap is tied to the per-stage texture limit, but each draw binds
      only one texture, so that limit doesn't apply.
  - Fix: images upload on their first draw, never on transmit. The cache
    is held to a 256 MiB byte budget, evicting the textures drawn least
    recently. Textures drawn this frame are never evicted, so nothing
    re-uploads per frame. Fixed in #24.
  - Also fixed in #24 (found in review):
    - Re-sending an id at the same size kept the old pixels when the
      first and last 8 pixels matched, because the GPU copy was keyed on a
      partial content fingerprint. It is now keyed on a per-insert
      registry generation. This also covers delete-then-anonymous-send
      reusing the lowest free id.
    - An image wider or taller than the GPU's `max_texture_dimension_2d`
      (e.g. a 20000×4 `a=T`) crashed toastty with a wgpu validation panic.
      It is now box-downscaled on the CPU by the smallest integer factor
      that fits, keeping the aspect ratio, and a warning is logged.
- [x] **P1 — SIZE-9. Images with z below INT32_MIN/2 are invisible.**
  - Repro: `K send a=T,i=1,z=-1073741825 --png img.png` over default and
    `\e[41m` cells.
  - Expected (spec): drawn "under cells with non-default background colors".
    kitty shows the image through default-bg cells, and red cells cover it.
  - Actual: nothing is visible.
  - Cause: `lib.rs:3089-3114` draws this layer first, then the bg pass paints
    every cell opaque, including default-bg cells.
  - Fix: while such a placement is on screen, the frame is a full clear and
    default-bg cell quads are dropped. "Default" is decided by value, as in
    kitty, so an explicit SGR color equal to the default bg also shows the
    image. Selection covers it, as in kitty. Fixed in #26.
  - Divergences from kitty 0.49.2, both kept deliberately:
    - DECSCNM (`CSI ? 5 h`): kitty hides the `z < INT32_MIN/2` image
      entirely (it also turns every cell's bg into the default). toastty
      keeps showing it under cells whose bg equals the effective (swapped)
      default bg. That is the sensible reading of the spec, and toastty's
      DECSCNM already differs from kitty's for SGR backgrounds.
    - With a transparent theme bg (alpha < 1), toastty's "default" check
      compares the alpha too. kitty compares only the 24-bit RGB. So an
      explicit `\e[48;2;r;g;bm` equal to the default RGB covers the image
      in toastty (the cell is painted opaque), but in kitty it lets the
      image through and is drawn translucent.
- [x] **P2 — SIZE-10. `X=`/`Y=` pixel offsets aren't clamped to the cell, and
  aren't fitted into `c`/`r`.**
  - Repro: `X=25,Y=40` with 10×19 cells, and `X=5,c=6`.
  - Expected (spec): "the offsets must be smaller than the size of the
    cell". kitty clamps to +8/+19, and fits `X=5,c=6` inside the 6-column
    box.
  - Actual: toastty applies the values as given, and the image overruns the
    box by 5 px.
  - Cause: `instance.rs:143-146` and `handler.rs:893`.
  - Fixed in #39. Clamped to `cell − 1` (kitty: +8,+19 for `X=25,Y=40`),
    and fitted inside `c`/`r` (`X=5,c=6` → 49×25 on kitty, 55×28 on
    toastty's 10 px cells). The offset also widens the reservation, as in
    kitty.
  - Review of #39: as kitty does, an offset widens only a natural-size
    reservation. With a single `c=`/`r=`, the derived side is the given
    span plus its own offset at the aspect ratio, and the other offset
    doesn't count. At 10×19, `c=10,Y=19` now leaves the cursor at 8;31
    (was 9;31) and `r=3,X=9` at 8;33 (was 8;34), both matching kitty's
    rule.
- [x] **P2 — SIZE-11. Font zoom rescales natural-size images.**
  - Expected: kitty keeps a natural-size image (no `c=`/`r=`) at its pixel
    size. For placements with explicit `c=`/`r=` it keeps the cell span and
    rescales; toastty matches kitty there.
  - Actual: toastty keeps the 21×6-cell span and grows the image from
    205×105 to 273×156. This falls out of SIZE-1.
  - Fixed in #39. The size is computed at render time: after zooming,
    205×105 stays 205×105 (was 231×132), and `c=10` on 400×200 rescales to
    110×55 at 11 px cells (kitty: 120×60 at 12 px).
  - The reserved cells are recomputed on zoom too, as on kitty-ref, so
    cell hit-tests (`d=p/c/x/y`, eviction) follow the drawn image.
- [x] **P2 — SIZE-12. Downscaling aliases badly.**
  - Repro: a 1500×900 grid image with `c=30`.
  - Actual: the 10 px grid's vertical lines vanish. kitty also aliases, but
    the grid stays visible.
  - Cause: `pipeline.rs:161-163` uses linear filtering with no mipmaps.
  - Consider mipmaps, or a box prefilter on upload.
  - Fix: a texture drawn downscaled gets the mip levels that draw samples
    (`floor(log2(ratio)) + 2`, added to in place if it's later drawn
    smaller). Sampling is plain trilinear, with no LOD bias.
    - Blur trade-off: trilinear is slightly soft on thin features (a 1 px
      diagonal at `c=30` widens to about 3 px). A −0.5 bias sharpened it
      but brought aliasing back at moderate downscales: on a zone plate,
      the residual that should be flat rose from 20.9 to 41.2 at 1.5× and
      from 17.5 to 27.9 at 2× (review of #34). The softness is kept; a
      bias of at most −0.25 would need re-measuring first.
    - The filter is alpha-weighted and runs in linear light, with a 3-tap
      footprint for odd sizes.
    - Textures up to 256² build their levels on the CPU; larger ones use
      GPU passes in the frame's encoder.
    - Mip work is capped per frame (16 GPU passes, 256K CPU texels, 4 ms).
      The rest finishes on following frames, and meanwhile those images
      draw from level 0.
    - Images drawn at 1:1 or larger stay single-level and render
      pixel-identically to before.
    - Fixed in #34.
  - Divergence (acknowledged by the PM, kept deliberately): at `c=30` the
    true grid pitch is about 1.8 px, so toastty now draws an even tint.
    kitty's visible grid there is itself aliasing: it shows every 5th line
    at a 9 px pitch.
- [x] **P2 — SIZE-13. `TIOCGWINSZ` pixel size includes the leftover sub-cell
  strip.**
  - Measured: toastty reports 1000×600 for 100×31 cells (19.35 px rows),
    while `CSI 16t` says 19 and `CSI 14t` says 589. kitty reports
    `cols*cell_w × rows*cell_h`.
  - Effect:
    - icat `--place 12x6` and `--scale-up` images come out a row too tall.
    - Placeholder tiles drift.
  - Cause: `crates/toastty/src/main.rs:822-830, 1086-1093` report the
    content rect.
  - Also, `CSI 14;2t` answers the same as `14t`. kitty reports the full
    window there.
  - Fix: `TIOCGWINSZ` pixels and `CSI 14t` both come from
    `Term::text_area_pixel_size` (`cols*cell_w × rows*cell_h`). `CSI 14;2t`
    reports the window surface, padding included. Checked against kitty
    0.49.2 on a 1000×600 window: kitty gives 999×600 / 1000×600, and
    toastty now gives 1000×589 / 1000×600. Each pair is consistent with
    that terminal's own `16t`/`18t`. Fixed in #25.

## 3. Unicode placeholders

Shared root cause:

- When a run of placeholder cells is printed, `finalize_placeholder_run`
  (`term.rs:2546-2648`) turns each `U+10EEEE` cell into its own 1×1
  `ImageGrid` placement at the image's native pixel scale.
- The `U=1` virtual placement itself is thrown away (`handler.rs:633`).
- After that, nothing ties the tile to its text cell, and each tile costs a
  draw call.

The fix: keep virtual placements, including their `c`/`r`, in a table.
Resolve placeholder cells at render time from the cell grid, like text, so
erasing, scrolling, scrollback and reflow all work for free.

Done in #28. See
[`decisions/unicode-placeholders.md`](decisions/unicode-placeholders.md).

- **Deliberate divergence from kitty.** Placeholder cells keep their
  diacritics in toastty's shared extras table. When scrollback holds
  more distinct live placeholder cells than its quota (about 49k), the
  oldest scrollback rows lose their placeholder images: the text stays
  and the image no longer draws. kitty keeps them.
  - **Known edge case.** The visible screen and the alt screen are never
    stripped. If both show huge distinct placeholder images at once (more
    than about 24.5k distinct cells each), new placeholder cells can
    still hit the cap and stay blank.

- [x] **P0 — PH-1. Placeholder images don't follow their cells.**
  - Repro:
    - Draw a placeholder image, then write text over some rows, run
      `CSI 2K`/EL/ECH/DCH/ICH, or overwrite with spaces.
    - In tmux: `kitten icat` an image, then `new-window` or `split-window -h`.
  - Expected: the image is part of the text, so kitty erases and moves it
    with the cells.
  - Actual:
    - The old image stays on top, hiding the new text.
    - In tmux, the image from window 0 is drawn over window 1, and after a
      split it crosses the pane border. `tmux refresh-client` doesn't help.
  - Only ED 2/3, deletes and row shifts remove the tiles.
  - Fixed in #28.
- [x] **P0 — PH-2. Placeholder images aren't scaled to the virtual
  placement.**
  - Repro: `K send a=T,i=1,U=1,q=2,c=10,r=5 --png img.png` with a 200×100
    image, then print a 10×5 block of placeholders. Or run
    `kitten icat --unicode-placeholder`.
  - Expected (spec): the image fits the placeholder rectangle, keeping its
    aspect ratio, centred. kitty does this.
  - Actual: a native-scale top-left crop.
  - Cause: `term.rs:2571-2629`. This also means the placement id in the
    underline colour can't select a different size.
  - Fixed in #28.
- [x] **P1 — PH-3. Cells past the image's edge show a stretched 1 px edge
  strip.**
  - Expected: blank. With icat at the default cell size, toastty shows a
    white bar under the image instead.
  - Cause: the clamp at `term.rs:2620-2623`.
  - Fixed in #28.
- [x] **P1 — PH-4. Placeholder images are lost in scrollback.**
  - Repro: scroll placeholder lines into history, then scroll back.
  - Expected: they're text, so kitty shows them.
  - Actual: they're missing. A leftover strip stays pinned at screen row 0.
  - Cause: tiles are `ImageGrid` placements, so SCR-2 applies;
    `image_grid.rs:356`.
  - Fixed in #28.
- [x] **P1 — PH-5. Placeholders printed before the image arrives never
  display.**
  - Expected: kitty shows the image once it's transmitted.
  - Cause: the lookup at `term.rs:2610` is never re-checked.
  - Fixed in #28.
- [x] **P1 — PH-6. Re-sending an image after `d=I` doesn't refill existing
  placeholder cells.**
  - Expected: kitty shows the new image in the existing cells.
  - Fixed in #28. Re-transmitting an id without `U=1` drops its virtual
    placements, per the spec and kitty 0.49.2. The cells stay blank until a
    new `U=1`.
- [x] **P1 — PH-7. Positional deletes erase placeholder images.**
  - Repro: `d=a` removes them, and `d=p` punches a one-cell hole.
  - Expected: kitty leaves placeholder images alone for position-based
    selectors.
  - Fixed in #28. ED 2/3 and RIS also keep virtual placements, as in
    kitty.
- [x] **P1 — PH-8. Narrowing the window permanently truncates a placeholder
  image.**
  - Repro: narrow the window, then widen it again.
  - Actual: the text reflows back, but the image stays cut to the narrow
    width.
  - Cause: `clip_to` (`term.rs:1865`).
  - Fixed in #28.
- [x] **P2 — PH-9. The image shows through placeholders without any `U=1`
  virtual placement.**
  - Expected: kitty shows nothing.
  - Fixed in #28.
- [x] **P2 — PH-10. Basic SGR colours (30–37, 90–97) aren't accepted as the
  image id.**
  - Only `38;5` and `38;2` work.
  - Cause: `term.rs:5210`.
  - Fixed in #28.
- [x] **P2 — PH-11. Bare `U+10EEEE` cells with no diacritics render image
  row 0.**
  - Expected: kitty shows nothing.
  - Lenient rather than harmful.
  - Fixed in #28. A run whose first cell has no row diacritic is blank,
    as in kitty. A missing column defaults to 0.

## 4. Relative placements

- [x] **P1 — REL-1. A rejected relative re-placement deletes the existing
  placement.**
  - Repro: re-send a child's `i`/`p` with a `P`/`Q` that causes ECYCLE or
    points at itself.
  - Actual: the old placement is already gone, so later children of it get
    ENOPARENT. kitty keeps everything.
  - Cause: `term.rs:4760-4769` removes before validating.
  - Fixed in #40.
- [x] **P1 — REL-2. Deleting a parent leaves its descendants on screen.**
  - Expected (kitty, per spec): children and grandchildren are deleted, and
    images left with no placements are freed even on lowercase `d=i`.
  - Cause: `delete_image` doesn't cascade; `image_grid.rs:194-200` keeps
    orphans in place.
  - Fixed in #40. Every removal path cascades, including history
    eviction and ED 2. Children deleted directly keep their image, as in
    kitty.
  - Whether a cascaded child's image is freed depends on the delete, as on
    kitty-ref 0.49.2:
    - freed on `d=i/I`, `n/N`, `r/R`, `z/Z`, `q/Q` (lowercase too), and on
      every uppercase delete;
    - kept on lowercase `d=a`, `p`, `x`, `y`, `c`, and when the parent
      leaves the history;
    - freed when the parent's image is re-transmitted or evicted by the
      quota (kitty-ref: `a=p` on the child's id gets ENOENT, while an
      unrelated newer image stays).
  - `ESC[2J` doesn't clear a relative child by itself: it goes with its
    parent. kitty-ref: a child on screen whose parent is in the history
    survives `ESC[2J` with its image; a child in the history whose parent
    is on screen goes, and both images are freed (PLC-8).
    The "freed even on lowercase `d=i`" above is that rule's id-like
    half.
- [x] **P1 — REL-3. Children don't follow a parent moved by re-sending the
  same `i`+`p`.**
  - Cause: `place_image` (`term.rs:4729-4743`) never calls
    `resolve_relative_positions`.
  - Fixed in #40.
- [x] **P1 — REL-4. `a=T` ignores `P`/`Q`/`H`/`V`.**
  - Actual: the image goes to the cursor and moves it.
  - Cause: `handler.rs:596-615`. Only `handle_place` has the relative
    branch.
  - Fixed in #40.
- [x] **P1 — REL-5. A child of a virtual (`U=1`) parent anchors to the last
  placeholder cell.**
  - Expected (spec): anchor to the minimum x/y of the placeholder cells.
    kitty puts the children at (5,27)/(9,20); toastty at (7,32)/(11,25).
  - Cause: `ImageGrid::find` (`image_grid.rs:139-145`) returns the most
    recent tile.
  - Fixed in #40. On the merge train (with #28) the origin comes from
    `Term::placeholder_origin`, through the grid's virtual-parent
    snapshot; a child goes with its virtual parent.
- [x] **P2 — REL-6. Negative `H`/`V` offsets are clamped to column/row 0.**
  - Expected: kitty lets the child go partly off-screen and clips it.
  - Cause: `offset_origin` (`image_grid.rs:538-545`).
  - Fixed in #40. `Placement::col` is signed, and the renderer crops
    the part left of column 0.
- [x] **P2 — REL-7. Error codes differ from kitty.**

  | Case | toastty | kitty |
  |---|---|---|
  | Self-reference to an existing placement | ECYCLE | EINVAL |
  | Self-reference to a non-existent placement | ECYCLE | ENOPARENT |
  | `U=1` with `P`/`Q` (spec requires EINVAL) | OK | EINVAL |
  | `P` without `Q`, or `Q` without `P` | EINVAL | OK |

  - Fixed in #40. All four rows now match kitty. `P` without `Q` is
    bound once, at creation, to the parent image's oldest placement, so
    a P-only cycle is ECYCLE, a P-only self-reference is EINVAL, and
    deleting that placement deletes the child. `Q` without `P`
    is an ordinary placement.
  - kitty accepts `a=T,U=1` with `P=`/`Q=` (it makes the virtual
    placement), unlike `a=p`. toastty does the same.

## 5. Placement, cursor and image lifetime

- [x] **P0 — PLC-1. `a=p` on an unknown or freed id replies OK.**
  - Repro: `K send a=p,i=143999 --reply 0.5`.
  - Expected (spec): ENOENT. kitty replies
    `i=143999;ENOENT:Put command refers to non-existent image …`, and still
    sends it with `q=1`.
  - Actual: toastty replies OK, creates an invisible 1×1 placement, and
    moves the cursor. Same with `p=`, with `U=1`, and after eviction.
  - Clients that use `a=p` to check whether an image is cached never re-send
    it.
  - Cause: `handle_place` (`handler.rs:623-700`) never calls
    `sink.image_exists`.
  - Fixed in #37. `a=p` resolves the image first and replies kitty's
    `ENOENT:Put command refers to non-existent image with id: N and
    number: M` (sent with `q=1`, silent with `q=2`), with no placement and
    no cursor move. Same with `p=`, `U=1`, and after the image is freed.
- [x] **P1 — PLC-2. Re-transmitting an existing id keeps its placements,
  which then show the new pixels.**
  - Repro: `a=T,i=1` red, then `a=t,i=1` blue.
  - Expected (spec): "the existing image and all its placements must be
    deleted. The new data … is not actually displayed until a placement for
    it is created". kitty shows nothing.
  - Actual: the old placement turns blue. A second `a=T` with the same id
    shows two copies.
  - Cause: `register_image` (`term.rs:4591-4650`) replaces the registry entry
    without dropping that id's placements.
  - Fixed in #42. Re-transmitting an id deletes the old image and all its
    placements; the new data shows once placed (kitty-ref: `a=T` red then
    `a=t` blue shows nothing; a second `a=T` leaves one copy). Each
    transmission gets a fresh store key. See
    [`decisions/image-store.md`](decisions/image-store.md).
- [x] **P1 — PLC-3. `a=p,I=N` is rejected with EINVAL.**
  - Expected (spec): commands that use an image number act on the newest
    image with that number. kitty replies `i=2,I=144;OK`.
  - Actual: `I=144;EINVAL:place requires i=`.
  - Cause: `handler.rs:624-626`. `Term` already keeps an
    `image_number_to_id` map, but uses it only for `d=n`.
  - Fixed in #37. `a=p,I=N` places the newest stored image numbered N
    and replies `i=<id>,I=N;OK`. If that image is freed, the next newest
    with the number takes over, as in kitty (`Term::image_number_to_id`
    is now `HashMap<u32, Vec<u32>>`).
- [x] **P1 — PLC-4. Anonymous images get small ids other clients can
  address.**
  - Repro: an anonymous `a=T`, then another program sends `a=t,i=1` or
    `a=d,d=I,i=1..2000`.
  - Expected: kitty's anonymous image survives commands that name ids.
  - Actual: toastty's anonymous image is overwritten or deleted.
  - Cause: `registry.rs:163, 252` (`next_free_id` hands out the lowest free
    id). Allocate from the top of the id space, or keep anonymous images
    out of the id table.
  - Fixed in #42. Anonymous images (no `i=`, no `I=`) have no client id at
    all, so no id or id-range command reaches them (kitty-ref: one survives
    `d=R,x=1,y=4294967295`, `a=t,i=1` and `d=I,i=1`). Positional deletes
    still remove them. `I=` without `i=` gets the lowest free id, as in
    kitty (`I=5` → `i=1`).
- [x] **P2 — PLC-5. A placement that ends at the right edge clamps the cursor
  to the last column.**
  - Actual: the next text lands hidden under the image. kitty wraps to
    column 1 of the next line.
  - Cause: `term.rs:4984-5010`.
  - Fixed in #39. The cursor wraps to column 1 of the next line, as in kitty
    (`if (x >= columns) { x = 0; y++; }`).
- [x] **P2 — PLC-6. Main and alt screens share one image store.**
  - Repro: `d=I` on the alt screen.
  - Actual: it frees the main screen's image, which is blank after leaving
    alt. kitty keeps a separate store per screen.
  - This bites in practice: a full-screen previewer deleting its own ids
    wipes a shell's icat image that had the same id.
  - Cause: `term.rs:4513, 4854` don't check the stashed main-screen grid.
  - Fixed in #42. One `ImageStore` per screen (bytes, ids, numbers,
    quota), swapped with the image grid on screen switches. Deletes
    (`d=i/I/n/N/r/R`, positional), same-id replacement and quota eviction
    on one screen can't reach the other. Each screen has its own quota,
    as in kitty. `d=F` (#35) goes through the same store.
- [x] **P2 — PLC-7. Intermittent: an image was missing after re-sending the
  same id.**
  - Seen once and not reproduced in 8 more runs.
  - Possibly a renderer race on same-id replacement. Keep an eye on it.
  - Not reproduced in #42: 40 live same-id re-sends (20 alternating
    red/blue, 20 identical), grabbed 0.25 s after each, all showed the
    right image. Each re-send now gets a fresh store key, so nothing keyed
    on the id can go stale.
- [x] **P2 — PLC-8. Clears and lost placements don't free image data.**
  - Repro: `a=T,i=60`, `a=t,i=61`, then `ESC[2J`, then `a=p,i=60` and
    `a=p,i=61`; or `a=T,i=1`, `ESC c`, `a=p,i=1`.
  - Expected (kitty-ref): `ESC[2J` frees every image left with no
    placement, never-placed ones included, so both get ENOENT and the
    next `I=` without `i=` is handed `i=1` again. Images placed in the
    history survive. `ESC[3J` frees all of them. RIS frees the whole store
    (ENOENT). An anonymous image's data goes with its last placement,
    since nothing can place it again.
  - Actual: toastty kept every image's bytes through all of these, so the
    store grew, `a=p` still found them, and `I=` ids drifted upwards after
    a clear.
  - Fixed in #42. `ESC[2J`/`ESC[3J` free unplaced images, RIS clears the
    store, and every placement removal frees an anonymous image (sixel
    too) with its last placement. See
    [`decisions/image-store.md`](decisions/image-store.md).

## 6. Deletion

Every `d=` selector and its uppercase form removed the same placements as
kitty, with the same keep/free behaviour and no leftover pixels. The
remaining gaps:

- [x] **P1 — DEL-1. `a=d` sends a reply.**
  - Repro: `K send a=d,d=i,i=148 --reply 0.5`. The same happens for ids that
    don't exist.
  - Expected: kitty never replies to deletes, even with `i=` and `q=0`.
  - Actual: toastty replies `i=148;OK`.
  - Clients that don't read replies get those bytes on stdin, where readline
    turns them into garbage on the prompt.
  - Cause: `handle_delete` (`handler.rs:704-707`).
  - Fixed in #37. `a=d` never replies. Both `i=` and `I=` is still
    EINVAL, as in kitty.
- [x] **P2 — DEL-2. `d=F` is a no-op.**
  - Expected: kitty deletes and frees a single-frame image. `d=f` is a no-op
    in both.
  - Cause: `delete_image` has no `f`/`F` arm.
  - Checked against kitty 0.49.2:
    - `d=F,i=N` deletes every placement of the image (`p=` doesn't scope it)
      and frees its data, even when it has no placements.
    - `d=F,I=N` does the same to the newest image with that number.
      Repeated deletes then reach the older images with that number, one
      at a time (same for `d=N`). toastty now keeps every id per number,
      not just the newest.
    - `d=F` with no `i`/`I`, or with an unknown id, does nothing. So does
      `d=f`.
  - Fix: the new `Term::delete_image_frames` does this. Stack E's
    animation-frame work hooks in there. Fixed in #35.
- [x] **P2 — DEL-3. A delete during a chunked upload doesn't abort it.**
  - Expected (spec): the upload is aborted. kitty replies ENOENT to the
    remaining chunks.
  - Actual: the upload completes and displays.
  - Cause: `handler.rs:704`.
  - Fixed in #44. Any delete aborts the upload in progress, whatever it
    deletes. Correction: kitty 0.49.2 does **not** reply ENOENT to the
    remaining chunks. They get no reply at all: a bare continuation has
    no dimensions, so it fails the first-chunk check, anonymously. (The
    ENOENT seen in the original probe came from a later `a=p`.)

## 7. Security and robustness

Anything that writes to the PTY can send these commands: `cat` of a file,
`curl` output, a remote host over ssh.

- [x] **P0 — SEC-1. `t=t` deletes files outside the temp directory.**
  - Spec: "the terminal emulator should only delete the file if it is in a
    known temporary directory, such as `/tmp`, `/dev/shm`, `TMPDIR` … **and**
    the file has the string `tty-graphics-protocol` in its full file path".
  - Test cases, with toastty / kitty results:

    | Path | toastty | kitty |
    |---|---|---|
    | `/tmp/../home/<user>/file` | deleted | kept |
    | any directory, name contains `tty-graphics-protocol` | deleted | kept |
    | `/tmp/plain-file`, no marker | deleted | kept |
    | temp dir, with marker | deleted | deleted |
    | symlink in `/tmp` pointing into `$HOME` | link unlinked, target kept | link and target kept |

  - Cause: `is_under_temp_dir` (`handler.rs:795-822`) has three problems:
    - `Path::starts_with` is component-wise and doesn't resolve `..`, so
      `/tmp/../…` passes. The canonicalized-path fallback doesn't help,
      because the raw check already returned true.
    - It ORs "under a temp dir" with "name has the marker". The spec and
      kitty require both.
    - It doesn't require a regular file.
  - The file is deleted even when decoding fails. kitty does that too, but
    only after its stricter path check.
  - Fix:
    - Canonicalize first, and reject anything whose canonical parent isn't
      a temp dir.
    - Require the marker.
    - `lstat` and require a regular file.
    - Add tests for each row above.
  - Fixed in #30. The marker is checked on the sent path and the temp
    dir on the resolved path, and only the sent entry is unlinked (a
    final symlink, never its target). This matches kitty live on every
    row above, plus marked links, `..` inside the temp dir and marked
    directory symlinks. One check is stricter than kitty: the unlinked
    entry's own directory must also resolve into a temp dir. See
    `docs/decisions/kitty-file-medium.md`.
- [x] **P0 — SEC-2. `t=f` on a FIFO freezes the terminal.**
  - Repro: `mkfifo f`, then `K send i=1,t=f,s=2,v=2,f=32 --path f`.
  - Expected (spec): "terminal emulators **must** refuse to read any
    device/socket/etc. special files … These checks should be made on the
    path *before* the file is opened, since merely opening a file can have
    side-effects". kitty replies `EBADF:Failed to read image file` in 50 ms.
  - Actual:
    - toastty blocks in `open()` on the main thread, so the UI and input
      freeze.
    - When another process opens the FIFO, toastty accepts the bytes, and
      its late reply lands in the shell as typed text.
  - Cause: `read_file_range` (`handler.rs:763-788`) opens with no file-type
    check, on the PTY thread.
  - Fixed in #30. `stat` before `open`, then
    `open(O_NONBLOCK|O_NOFOLLOW)` and `fstat` (`kitty/medium.rs`). A FIFO
    now replies EBADF at once.
- [x] **P0 — SEC-3. `t=f` with no `S=` reads without a limit.**
  - Repro: `t=f` pointing at `/dev/zero`.
  - Actual: toastty's RSS grew past 3 GB in about 2 s, and the test watchdog
    killed it. kitty replies EBADF.
  - The spec says device files must be refused, and that terminals may
    refuse paths under `/proc`, `/sys` and `/dev`. toastty reads
    `/proc/self/cmdline`.
  - Fix:
    - `stat` the path (following symlinks, as the spec requires) and require
      `S_ISREG` before opening.
    - Open with `O_NONBLOCK`, then `fstat` again to close the race.
    - Cap reads at the expected decoded size, or at the quota.
    - Reject the pseudo-filesystems.
  - Fixed in #30. `/proc`, `/sys` and `/dev` are refused (`/dev/shm`
    stays readable, as in kitty). Reads are capped at `S=`, at
    `s*v*bpp` for raw pixels, or at the registry cap.
- [x] **P1 — SEC-4. File errors reveal details about local files.**
  - Expected (spec): the terminal "**must not** allow such a program to use
    the responses … to learn anything about files it cannot read itself …
    all failures to read an image file … must be reported with a single,
    identical, error response". kitty always replies
    `EBADF:Failed to read image file` and logs the real reason.
  - Actual: toastty echoes the OS error (`No such file or directory`,
    `Is a directory`, `Permission denied`, `Too many levels of symbolic
    links`).
  - After a successful read, decode errors include the byte count
    (`…; got 1234`), which reveals file sizes. A remote host can probe the
    user's local filesystem this way.
  - Cause: `handler.rs:470-505` and the decode-error passthrough in
    `finalize_decoded` (~560-575).
  - Fixed in #30. Every `t=f`/`t=t` failure replies
    `EBADF:Failed to read image file`, including inflate and decode
    failures of the bytes read. The reason goes to the debug log.
  - Divergence (PM decision): kitty replies `EBADPNG:Not a PNG file`
    for a readable non-PNG file. We reply the uniform EBADF instead,
    because that reply would reveal a local file's contents.
- [x] **P1 — SEC-5. zlib decompression (`o=z`) has no size limit.**
  - Repro: `i=1,s=1,v=1,f=32,o=z` with 1 GiB of zeros, compressed.
  - Actual: toastty inflates all of it (RSS 101 MB → 1123 MB) before
    replying EINVAL. A 400 MiB stream peaked at about 1 GB. kitty stops at
    the expected size (`s*v*4`) and uses no extra memory.
  - Cause: `inflate_zlib` (`handler.rs:746-751`) calls `read_to_end` with no
    cap. Use `take(expected + 1)`.
  - Fixed in #30. Inflation stops at `s*v*bpp` for raw pixels, or at
    the registry cap for PNG: `EINVAL:zlib decompress: inflated data
    exceeds N bytes`.
- [x] **P1 — SEC-6. No limit on the number of in-flight chunked uploads, and
  abandoned uploads are never freed.**
  - Repro: 16 chunked uploads with different `i=`, each 48 MiB, none
    finished.
  - Actual: toastty's RSS reached 917 MB and stayed there. kitty: 313 MB.
  - Cause: `pending` (`handler.rs:418-425`) has a per-upload cap
    (`DEFAULT_PENDING_CAP`, `handler.rs:30`) but no total cap, no count cap
    and no expiry.
  - Fixed in #30 with count/byte caps and LRU eviction; superseded in
    #44 by kitty's single loading slot: everything during a load is
    appended to it, so memory in flight is one upload's decoded buffer,
    capped at kitty's buffer size (bounded from the base64 length before
    decoding). An upload with no chunk for 60 s is freed on the next
    graphics command or batch of PTY input.
- [x] **P2 — SEC-7. Oversized PNGs are fully decoded before being rejected.**
  - Repro: a 12000×12000 PNG.
  - Actual: toastty peaks at 1.28 GB RSS, then replies EFBIG. kitty replies
    `ENOMEM: PNG image is too large` straight away.
  - Check the IHDR dimensions against the quota before decoding.
  - Fixed in #30. The IHDR is read first, and `w*h*4` over the
    registry cap replies `ENOMEM:PNG image is too large`, as kitty does.

## 8. Wire format and replies

- [x] **P1 — WIRE-1. `a=q` with `t=f`, `t=t` or `t=s` replies ENOTSUP.**
  - Expected: kitty replies OK. For `t=t` it deletes the file; for a missing
    file it replies EBADF.
  - Effect: `kitten icat --detect-support` reports `stream` in toastty
    (kitty: `memory`), so icat never auto-selects file transfer.
  - Cause: `handler.rs:278-279`.
  - Fixed in #49 for `t=f` and `t=t`, through the hardened
    `medium::read_image_file` path; `t=t` deletes under the SEC-1 rule. All
    16 measured cases match kitty, including two found on the way:
    - kitty checks `f=` and the dimensions before it touches the file
      (`EINVAL:Zero width/height not allowed`, file kept), for `a=t` too;
    - a file shorter than `S=` is read to its end (icat's probe sends a
      3-byte file with `S=47`); B1 had refused it.
  - `kitten icat --detect-support` now reports `files` (kitty: `memory`).
    `a=q,t=s` keeps its ENOTSUP until shared memory lands (WIRE-2, separate
    PR); then detection should report `memory`.
  - `a=q,t=s` fixed in #51 (WIRE-2): `--detect-support` reports `memory`
    on Linux, as in kitty.
- [x] **P1 — WIRE-2. Shared memory (`t=s`) is unsupported, and the shm object
  isn't unlinked.**
  - Actual: `kitten icat --transfer-mode=memory` shows nothing and exits 0,
    because icat sends `q=2`, and objects pile up in `/dev/shm`.
  - The spec says the terminal "must read the data from the memory object
    and then unlink and close it".
  - Cause: `handler.rs:519-533`.
  - Implement it with `shm_open`/`mmap`/`shm_unlink` (rustix has these),
    after the SEC-1–SEC-3 fixes.
  - Fixed in #51 for Linux. The object is the file `/dev/shm/<name>`,
    read with the file mediums' rules (`O_NOFOLLOW | O_NONBLOCK` open
    relative to a `/dev/shm` dirfd, `fstat` regular-file check, bounded
    `read`s, no `mmap`), then unlinked through the same dirfd, also after
    a failed read or decode. `a=q,t=s` reads, decodes and unlinks without
    storing. Every failure is the uniform EBADF. `kitten icat
    --transfer-mode=memory` displays and leaves `/dev/shm` clean. 12
    measured cases match kitty 0.49.2 except that decode failures are
    EBADF, not `EBADPNG` (SEC-4). Design and table:
    `docs/decisions/kitty-file-medium.md`.
  - **macOS / non-Linux: decided: unsupported on macOS (keeps `#![forbid(unsafe_code)]`; clients fall back to file/base64).** Closed.
    macOS shm can only be read with `mmap`, which is `unsafe`, and
    `toastty-graphics` is `#![forbid(unsafe_code)]`. Off Linux `t=s`
    `shm_unlink`s the name (safe) and replies EBADF, so icat falls back
    to another medium.
  - **`mpv --vo=kitty --vo-kitty-use-shm=yes` doesn't work, in kitty
    0.49.2 either.** mpv 0.41.0 sends the name without the leading `/`
    (`mpv-kitty-0x…`); kitty logs "POSIX SHM names must start with /" per
    frame and shows nothing, and toastty refuses it the same way (spec:
    the name **must** start with `/`). mpv unlinks its own object, so
    nothing leaks. Use mpv's default (direct) mode.
- [x] **P0 — WIRE-3 (= APP-1). Animation commands without `q=` get ENOTSUP
  replies that get injected into the shell.**
  - Repro: `kitten icat anim.gif`, then type `echo hello` and press Enter.
  - What happens: icat sends `a=f` frames with `q=2`, but its three `a=a`
    control commands without `q=`.
  - Expected: kitty animates the GIF and sends nothing.
  - Actual:
    - toastty replies `ESC_Gi=…;ENOTSUP:animation ESC\` to each `a=a`.
    - After icat exits, readline reads `ESC _` as "yank last argument", so
      the next command line becomes garbage and bash runs it.
  - Cause: `handler.rs:261-267`.
  - Short-term fix: accept `a=a`/`a=c` silently while showing the first
    frame. Long-term: implement animation (§9).
  - Fixed in #37. Until animation lands (§9), `a=f`/`a=a`/`a=c` on a
    stored image are silent no-ops; `kitten icat anim.gif` shows the first
    frame and nothing reaches the shell. Divergence until then: kitty
    answers `a=f` without `q=` with `i=N,r=<frame>;OK`, and `a=c` with
    frame-composition errors.
- [x] **P2 — WIRE-4. Base64 is decoded over the joined buffer, not per
  chunk.**
  - Repro: chunks each padded with `=` (`…m=1;AQIDBA==` then `m=0;BQY=`).
  - Actual: toastty replies `EINVAL:bad base64`; kitty accepts it.
  - In the other direction, toastty is more lenient than kitty about
    embedded whitespace and chunks that aren't a multiple of 4. Keep that.
  - Cause: `decode_base64` (`handler.rs:733-744`).
  - Fixed in #44. Each chunk is base64-decoded on arrival with its own
    padding, as kitty does. toastty additionally carries an unpadded
    partial quantum into the next chunk and skips whitespace (kitty:
    `ENODATA` for both), and keeps decoding after a `=` inside a chunk
    (kitty stops there). As in kitty, decoding a chunk stops at its first
    junk byte and keeps what came before (`/wAA*w==` gives 3 bytes).
- [x] **P2 — WIRE-5. An 8-bit `0x9C` byte inside an APC ends it.**
  - Repro: a non-G APC whose UTF-8 text contains `U+041C` (`\xd0\x9c`).
  - Actual: the rest of the payload is printed to the screen. kitty
    swallows the whole sequence.
  - Cause: `crates/toastty-parser/src/parser.rs:171-181`. In UTF-8 mode,
    C1 ST shouldn't end an APC.
  - Checked against kitty 0.49.2 for OSC, DCS, APC, PM and SOS. kitty ends
    none of them on 0x9C, whether it is a bare byte, the tail of
    `\xd0\x9c`, or U+009C (`\xc2\x9c`). toastty ended APC (its own
    scanner) and DCS (vte's passthrough) on it.
  - Fix: the APC scanner ends only on `ESC \\` / BEL. A new scanner state
    drops 0x9C inside a DCS before vte sees it (vte ignores bytes >= 0x80
    in a DCS anyway). OSC, PM and SOS were already right. Fixed in #32.
- [x] **P3 — WIRE-13. BEL doesn't end a DCS, PM or SOS; kitty ends all string
  types on BEL.** Found while checking WIRE-5.
  - Repro: `ESC P z hello BEL world ESC \\` (likewise `ESC ^` and `ESC X`).
  - kitty 0.49.2 ends the string at BEL and prints ` world`. toastty (vte)
    keeps going to `ESC \\` and prints nothing.
  - OSC and APC already end on BEL in both.
  - xterm only accepts BEL for OSC, so this is a kitty-ism. Low impact.
  - Fix: as in kitty, a DCS ended by BEL is dispatched as if it ended on
    ST. XTGETTCAP, DECRQSS and sixel all work that way, checked live.
    PM and SOS bodies are swallowed by the scanner up to BEL, ESC, CAN or
    SUB. tmux passthrough is unaffected: a BEL inside the wrapped payload
    is still data. Fixed in #36.
  - Deliberate difference, kept on purpose: inside a DCS, PM or SOS,
    kitty 0.49.2 treats CAN, SUB and an ESC not followed by `\` as data
    and keeps going to ST or BEL (checked live). toastty ends the string
    on them, as xterm and vte do.
- [x] **P2 — WIRE-15. A DCS cancelled by CAN or SUB is still acted on.**
  Found in the #36 review.
  - vte unhooks a DCS the same way on every terminator, so `Term::unhook`
    ran for a cancelled one too:
    - `ESC P $ q m CAN` got a full DECRQSS reply;
    - a sixel cut off by CAN was still placed.
  - Expected: xterm aborts the DCS on CAN / SUB.
  - kitty 0.49.2 (checked live) treats CAN / SUB as data, see WIRE-13. So
    it never answers a query cut off by one either; a later ST answers for
    the whole body, CAN included (`ESC P 0 $ r`).
  - Fix: the parser's DCS state calls `Perform::dcs_cancel` before passing
    CAN / SUB to vte. `Term` then drops the pending query or sixel, so the
    `unhook` that follows does nothing. Fixed in #45.
- [x] **P2 — WIRE-16. A DCS ended by an ESC that isn't ST is still acted
  on.** Found in the #45 review.
  - vte ends a DCS on any ESC and unhooks it as if it had ended on ST:
    - `ESC P $ q m ESC CAN` and `ESC P $ q m ESC [ c` got a full DECRQSS
      reply (the second one a DA1 reply too);
    - `ESC P + q 544e ESC x` got an XTGETTCAP reply;
    - a sixel cut off by an ESC was still placed.
  - kitty 0.49.2 (checked live) treats the ESC as data, see WIRE-13. It
    answers none of these until a real ST, and then answers for the whole
    body (`ESC P 0 $ r` for DECRQSS, nothing for XTGETTCAP).
  - Fix: a new scanner state holds the ESC after a DCS body. `\` makes it
    ST; any other byte calls `Perform::dcs_cancel` (as CAN / SUB do,
    WIRE-15), ends the DCS in vte and then handles the ESC as a fresh
    escape. Works across reads and inside tmux passthrough. Side fix: an
    `ESC _` (APC) right after a DCS body used to leave vte inside the DCS.
    Fixed in #54.
  - Deliberate difference: what follows the ESC still runs (the CSI, a new
    DCS), as in xterm and vte; kitty swallows it as DCS data until ST. So
    `ESC P $ q m ESC P $ q r ST` gets the second query's reply here and
    `ESC P 0 $ r` in kitty.
- [x] **P2 — WIRE-6. Malformed headers get an EINVAL reply; kitty stays
  silent.**
  - Cases: `s=abc`, `s=`, `a=X`, `t=x`, `o=y`, `x=-5`. (Re-measured for
    #37: `q=3` and `m=2` are not malformed in kitty — `q=` and `m=` are
    numbers, `q=3` stores the image silently and `m=2` means `m=1` — and
    `z=2147483648` is accepted.)
  - The comment at `emit_bad_header_einval` (`handler.rs:922`) says kitty
    replies EINVAL here. kitty 0.49.2 doesn't.
  - Pick one and document it. Replying is defensible, but it is more
    injected stdin (DEL-1).
  - Fixed in #37 (PM decision: match kitty). A malformed header gets no
    reply. The parser is now kitty's: unknown keys, whitespace, empty or
    trailing `,` pairs, `+`/hex/overflowing numbers and bad enum values
    are malformed. Re-measured: kitty does accept `z=2147483648` (signed
    keys take a 32-bit magnitude and wrap), and so do we now; an unknown
    `f=` number is a reply-worthy `EINVAL:Unknown image format: N`.
- [x] **P2 — WIRE-7. Error codes and validation differ from kitty.**

  | Case | toastty | kitty |
  |---|---|---|
  | Too little data | `EINVAL:expected N bytes…` | `ENODATA:Insufficient image data` |
  | Too much data | EINVAL | OK |
  | Bad PNG | `EBADF:PNG decode failed…` | `EBADPNG` |
  | Oversized PNG | EFBIG | ENOMEM |
  | zlib stream with checksum missing | OK | `Z_BUF_ERROR` |
  | `a=q` with empty body | OK | `EINVAL:Zero width/height` |
  | `a=q` with only `I=` | `I=150;OK` | silent |
  | Failure with only `I=` | `I=156;EINVAL…` | `i=4,I=156;ENODATA…` (allocates an id) |
  - Fixed in #37, byte-for-byte against kitty 0.49.2:
    `ENODATA:Insufficient image data: GOT < EXPECTED`, `EFBIG:Too much
    data` (more than 10 bytes past `s*v*bpp`; up to 10 extra is OK),
    `EBADPNG:Not a PNG file` / `…truncated…`, `EINVAL:Zero width/height
    not allowed`, `EINVAL:Failed to inflate image data with error:
    Z_BUF_ERROR|Z_DATA_ERROR`, `EINVAL:Image data size post inflation does
    not match expected size` (also for `f=100,o=z` without `S=`, which the
    spec requires), `EINVAL:Must not specify both image id and image
    number`. `a=q` replies only with `i=`; an empty `a=q` is validated like
    any other. A failed `I=`-only transmit reports the id it would get
    (`i=N,I=M;…`). Remaining divergence: kitty writes `I=`-only error
    replies with a stray leading comma (`\x1b_G,I=999;…`); we omit it.

- [x] **P2 — WIRE-8. `q=` on continuation chunks is ignored.**
  - Actual: toastty uses the first chunk's `q`, so a `q=2` sent only on the
    final chunk still gets `i=60;OK`. kitty stays silent.
  - Cause: `handler.rs:383-397`.
  - Fixed in #37. A non-zero `q=` on any continuation chunk applies to
    the upload (the latest wins; `q=0` can't lower it), matching kitty on
    all seven measured cases.
- [x] **P2 — WIRE-9. Abandoned-upload state corrupts later commands.**
  - Cases:
    - A bare `m=0` after an abandoned upload completes it with EINVAL.
    - A later `a=t,i=57` was appended to a leftover upload and failed.
    - An anonymous `a=T,I=965` was swallowed by a stale anonymous upload,
      so its reply never came.
  - Cause: `handler.rs:352-366, 418`.
  - Drop pending state on any new first chunk without `m=`, and expire old
    uploads (see SEC-6).
  - Fixed in #44, by matching kitty rather than isolating uploads:
    kitty has one loading slot and treats every direct transmit during a
    load as its continuation, whatever its `i=`. All three cases above
    behave exactly like kitty now: the bare `m=0` completes the leftover
    upload; `a=t,i=57` is appended and answered under the leftover's id;
    the stale anonymous upload swallows `a=T,I=965` silently.
  - Deliberate divergence: toastty drops an upload idle for 60 s (on the
    next graphics command or PTY input); kitty has no expiry and still
    appends to it after 65 s. The expiry is what keeps an abandoned upload
    from capturing later images, and its memory, forever. A file transmit or any delete aborts the load; `a=p` on the
    loading image replies `ENOENT:Put command refers to image with id: N
    that could not load its data`.
- [x] **P2 — WIRE-10. Chunked `a=q` handles only the first chunk.**
  - Actual: the rest of the chunks route to a different pending upload.
  - Cause: `handle_query` (`handler.rs:271`).
  - Fixed in #44, matching kitty: a chunked `a=q` is checked up front
    (format and dimensions), then consumes the next chunk and ends with
    no reply; nothing is stored. A chunked `a=q` during a transmit gets
    `EILSEQ:More payload loading refers to non-existent image` for that
    upload, which is dropped.
- [x] **P2 — WIRE-11. Valid transmissions are rejected by `S=` or the pending
  cap.**
  - Cases:
    - `S=999999999` on a direct 1×1 transmission: `EFBIG:declared size >
      budget`. For `t=d`, `S=` is only a hint.
    - A 4096² RGBA chunked upload (64 MiB raw, 89 MiB base64):
      `EFBIG:pending overflow`.
    - kitty accepts both.
  - Cause: `handler.rs:383-388, 403-405`. The cap counts base64 bytes.
  - Fixed in #44. `S=` is ignored for `t=d`. The upload cap counts
    decoded bytes and is kitty's buffer size: `s*v*bpp` + 10 (+1 KiB for
    `o=z`), or the image quota for PNG. A 4096² RGBA upload succeeds, as
    in kitty.
- [x] **P2 — WIRE-12. Animation and composition reply ENOTSUP even for ids
  that don't exist.**
  - Expected: kitty replies ENOENT.
  - Fixed in #37. `a=f`/`a=a`/`a=c` on an image that isn't stored reply
    kitty's `ENOENT:Animation command refers to non-existent image with
    id: N and number: M`; with neither `i=` nor `I=` they are silent.
- [x] **P3 — WIRE-14. Bare C1 bytes and some invalid UTF-8 in plain text
  print nothing; kitty prints U+FFFD.** Found in the #32 review.
  - Expected (kitty 0.49.2, checked live): one U+FFFD for each maximal
    invalid subpart, the same rule as `String::from_utf8_lossy`. Examples:
    - a bare `0x80`–`0xFF`, so `0x9c` and `0x9b 31m` give `\ufffd`,
      `\ufffd31m`;
    - `c0 af` gives 2, `ed a0 80` gives 3, `f4 90 80 80` gives 4;
    - a truncated `e2 82` + `A` gives `\ufffdA`.
    - Properly encoded C1 (`c2 9c`, `c2 85`) prints nothing and isn't
      acted on.
  - Actual: vte executes a bare `0x80`–`0x9F` as an 8-bit C1 control,
    which toastty ignores, so it vanished. That also made the longer
    invalid runs come out short.
  - Fix: a UTF-8 sanitizer in the parser's ground path hands vte
    `from_utf8_lossy`-style text. It holds an incomplete trailing sequence
    across reads, and turns one cut off by ESC into U+FFFD. Fixed in
    #41.
  - Side effects:
    - Output no longer depends on where a read splits an invalid
      sequence. Before, vte's partial-UTF-8 path printed different things
      for different splits.
    - Until #32 lands, a DCS body runs through ground text, so the
      sanitizer also rewrites it: a raw `0x9C` there becomes U+FFFD and no
      longer ends the DCS (`ESC P z hi \x9c AFTER ESC \` prints nothing,
      where it used to print `AFTER`). That matches kitty 0.49.2 and is
      what #32 does explicitly. Part of WIRE-5 changes here. The APC
      scanner still ends on `0x9C` until #32.
  - Cost: text is checked once per read, with SIMD (`simdutf8`), up to the
    last character boundary, and runs between escapes inside that span go
    straight to vte. Parser instructions per byte against `main`: ASCII
    +1.8%, CJK +5.1%, emoji +5.9%, sparse UTF-8 +5.7%, `ls --color` +2.2%,
    vim-style redraw +3.3%. That is one SIMD validation pass, about
    0.95 instructions per byte of non-ASCII text.

- [x] **P3 — WIRE-17. A byte >= 0x80 inside a CSI is ignored; kitty aborts
  the CSI and prints the rest.** Found in the #41 review.
  - Repro and expected (kitty 0.49.2, read back with `kitten @ get-text`):
    - `ESC[3 \x9c 1mX` prints plain `1mX`; the SGR is not applied.
    - `ESC[31 \x9c mX` prints `mX`, and `ESC[ \x9c 31mX` prints `31mX`.
    - The high byte itself is dropped, so a multi-byte character there
      leaves its continuation bytes as U+FFFD: `ESC[3 € 1mX` prints
      `��1mX`, and `ESC[3 é 1mX` prints `�1mX`.
    - The same holds after an intermediate (`ESC[1 \x9c qX` prints `qX`).
    - A C0 control inside a CSI is still executed and the CSI goes on
      (`ESC[3 BS 1mX` applies SGR 31).
  - Actual: vte ignores the byte (after #41 it sees U+FFFD's bytes) and
    dispatches `CSI 31 m`, so `X` is red and `1m` vanishes.
  - Fix: the scanner tracks when vte is inside a CSI or escape sequence. A
    byte >= 0x80 there aborts it: CAN goes to vte through a null performer
    (no dispatch), the byte is dropped, and parsing resumes in ground at
    the next byte. All-ASCII reads skip the check (nothing in them can
    abort a sequence). Fixed in #52.
  - Cost, in parser instructions per byte against #41: text +0.1 to 0.7%,
    `ls --color` style +1.9%, vim-like redraw with box drawing +6.9%. The
    last is the check on every sequence in reads that contain non-ASCII
    text.
  - Also seen on kitty-ref, but left as is (vte's behaviour, which matches
    xterm):
    - DEL inside a CSI aborts it in kitty (`ESC[3 DEL 1mX` prints `1mX`).
      vte ignores DEL and applies SGR 31.
    - ESC inside a CSI aborts it in kitty and the ESC is dropped
      (`ESC[3 ESC[32mG` prints `[32mG`). vte and xterm start the new
      sequence, giving a green `G`, which is kinder to truncated output.
- [x] **P3 — WIRE-18. ESC followed by a non-ASCII character uses the next
  ASCII byte as the escape's final; kitty prints the text.** Found in the
  #41 review.
  - Repro and expected (kitty 0.49.2, `kitten @ get-text`):
    - `ESC € A` prints `��A`, and `ESC é A` prints `�A`.
    - `ESC \x9c A`, `ESC \x80 A` and `ESC \xff A` print `A`.
    - `ESC 😀 A` prints three U+FFFD then `A`.
    - Likewise inside an escape with intermediates: `ESC ( € A` prints
      `��A`, and `ESC ( \x9c B X` prints `BX`.
  - Actual: vte ignores the non-ASCII bytes in the escape state and
    dispatches the next ASCII byte (`ESC A`) as the final, swallowing it.
  - Fix: the same rule as WIRE-17. An ESC followed by a byte >= 0x80 is
    dropped with that byte (the ESC still ends any string vte is in), and
    the rest is ground text. Fixed in #52.

## 9. Not implemented

- [ ] **P1 — Animation** (`a=f` frames, `a=a` control, `a=c` composition,
  `d=f/F`).
  - Clients degrade to a still first frame, except for the reply-injection
    bug in WIRE-3.
  - Needed for animated images in `kitten icat` and other clients that use
    kitty animation. `mpv --vo=kitty` sends whole frames as new images, so it
    doesn't need it.
  - Note `Header`'s key overloading for animation (`c`, `r`, `x`, `y`, `z`,
    `X`, `Y`, `s`, `v` mean different things under `a=f/a/c`).
  - `a=c,C=2` used to fail header parsing (`C` was parsed as a bool),
    giving EINVAL instead of ENOTSUP. Fixed in #37: `C` is a number, and
    `Header::frame()`, `Header::animation()` and `Header::composition()` give
    the animation meaning of the overloaded keys.
- [x] **P1 — Shared-memory transmission** (`t=s`). See WIRE-2.
  - Fixed in #51 for Linux; macOS: decided: unsupported on macOS (keeps `#![forbid(unsafe_code)]`; clients fall back to file/base64) (see WIRE-2). Closed.
- [x] **P2 — Usage hints** (`N=1` transient).
  - The key is accepted and ignored, which the spec allows.
  - It could feed eviction priority.
  - Fixed in #49. `N` is a bitmask; bit 0 marks the stored image transient
    (`KittySink::mark_transient`, `ImageRegistry::set_transient`). Under
    storage pressure the oldest unplaced transient image is evicted first,
    then other unplaced images, then placed ones. A re-send without the hint
    clears it.

## 10. Real-world client matrix

| Client / feature | toastty | kitty |
|---|---|---|
| `kitten icat` PNG/JPEG, stream and file modes | works, stretched (SIZE-1) | works |
| `kitten icat --transfer-mode=memory` | works on Linux, `/dev/shm` clean (WIRE-2, #51) | works |
| `kitten icat --detect-support` | `memory` on Linux (WIRE-1, WIRE-2) | `memory` |
| `kitten icat --place` / `--align` | works, ~1 row too tall (SIZE-13) | works |
| `kitten icat --scale-up` | wrong aspect ratio (SIZE-1, SIZE-13) | works |
| `kitten icat` animated GIF | still frame + text injected into the shell (WIRE-3) | animates |
| More than 14 images, or 500 tiny ones | only 14 visible (SIZE-8, fixed in #24) | all visible |
| `kitten icat --unicode-placeholder` | wrong scale; ghosts when cells change (PH-1, PH-2) | works |
| `kitten icat --clear`, `toastty image` | work | work |
| yazi preview: navigation, resize, quit | works | works |
| tmux passthrough icat, copy-mode, detach/reattach | works | works |
| tmux new-window / split over an image | stale image over other content (PH-1) | works |
| yazi inside tmux | works | works |
| `mpv --vo=kitty` incl. resize | works, ~26 fps, one core at 100 % | works |
| `mpv --vo=kitty --vo-kitty-use-shm=yes` | blank: mpv 0.41 sends the name without `/`, refused per spec (WIRE-2) | blank, same refusal |
| Rapid re-placement (3000 moves); 30k lines with 310 images | works | works |

Seen during app testing, not a graphics bug:

- [x] **P2 — Keyboard-protocol flags leak to the main screen after a
  full-screen app is killed.**
  - Repro: `timeout -s KILL yazi`, then `printf '\e[?1049l'`.
  - Actual: space arrives as `u` and Enter doesn't submit, until a
    `CSI < u`. kitty keeps keyboard flags per screen.
  - Fix: toastty keeps one flag stack per screen, as the spec requires.
    Checked against kitty 0.49.2 with a `CSI ? u` probe; the output is
    identical.
    - A screen switch only selects the other stack.
    - The alt stack survives between visits for 47, 1047 and 1049 alike.
    - Each stack holds 8 entries and evicts the oldest when full.
    - `CSI = u` modes 1, 2 and 3 behave the same as in kitty, including
      on an empty stack.
    - RIS and DECSTR (`CSI ! p`, which `tput init` sends) clear both
      stacks.
    - Fixed in #33.
- [x] **P1 — QRY-1. yazi prints "Terminal response timeout" at startup;
  toastty reportedly answers only DA1 of its four queries.** Not reproducible
  on `main`; see the evidence below. Closed in #48.
  - yazi sends one batch: `ESC 7`, `CSI > q` (XTVERSION), `CSI 16 t`,
    `OSC 11 ; ? BEL`, `CSI 0 c` (DA1), `ESC 8`.
  - toastty answers all four, in order, within a few milliseconds:
    `DCS >| … ST`, `CSI 6;19;10 t`, `OSC 11;rgb:1212/1212/1414 ST` and
    `CSI ? 62;4;22 c`. This holds in a shell, as `toastty -- yazi`, and
    inside tmux 3.8. In every case yazi's debug log shows the replies
    arriving in under 100 ms, and it starts without a timeout.
  - The replies match kitty 0.49.2 (checked live). OSC 10 / 11 / 12 / 4
    colour replies use `rgb:RRRR/GGGG/BBBB` and end with ST even when the
    query ended with BEL; kitty does the same.
  - The one change made: XTVERSION now names toastty's own version,
    `toastty 1.0.0 (kitty 0.42.0)`. The `kitty` brand is kept because
    yazi and other apps substring-match it to enable kitty graphics.
- [x] **P1 — PTY-1. The shell inherits toastty's ignored signals and signal
  mask.**
  - If toastty is launched with SIGINT ignored (e.g. `toastty &` from a
    script, or a launcher that ignores SIGINT / SIGQUIT), Ctrl+C can't stop
    foreground jobs, and their `/proc/<pid>/status` shows `SigIgn: 0x6`.
    `std` only resets SIGPIPE; ignored dispositions and the blocked mask
    both survive `exec`.
  - Fix: the PTY child sets every signal back to `SIG_DFL` and clears the
    blocked mask before `exec` (`child_setup` in `toastty-pty`). Fixed in
    #46.
  - kitty 0.49.2 (checked live, launched with SIGINT / SIGQUIT ignored)
    resets SIGINT but leaves SIGQUIT ignored (`SigIgn: 0x4`). toastty
    resets everything.
- [x] **P1 — KBD-1. Under kitty keyboard flags, Enter, Tab, Backspace and
  Space are sent as `CSI … u`.** After a killed TUI, `less` ignores Space and
  Enter, while kitty pages normally. Fixed in #43.
  - Checked live against kitty 0.49.2, key by key (23 keys), for flags 0, 1,
    2, 3, 4, 8, 9, 16, 24 and 31. Everything matches except the items
    below.
  - Enter, Tab and Backspace:
    - Unmodified, they keep their legacy bytes unless report-all (8) is on,
      and they never report a release without it (spec).
    - Modified, they're `CSI code;mods u`, even under report-events (2)
      alone.
  - Text keys (Space, letters, ...) without Ctrl, Alt or Super send their
    text unless report-all is on.
  - Report-events (2) alone: Ctrl, Alt and Esc presses keep their legacy
    bytes, and only releases are escape codes.
  - A press never carries the `:1` event type.
  - Report-alternates (4) alone, and report-text (16) without 8, behave
    as legacy.
  - Under 8: report-text (16) attaches the text (`CSI 97;;97u`; none with
    Ctrl / Alt), and 4 adds the shifted key (`CSI 97:65;2;65u`).
  - Legacy mode (no flags) now sends xterm's modified named keys:
    `CSI 1;2A`, shift+Tab `CSI Z`, ctrl+Backspace `^H`, ctrl+Space NUL,
    alt+Enter `ESC CR`.
  - Stack edge cases:
    - `CSI < 0 u` pops nothing; `CSI < u` pops one. The parser reports
      whether a CSI had parameter digits (`Perform::csi_params_explicit`),
      because vte can't tell the two apart.
    - Flags are masked to 7 bits.
    - An explicit mode 0 in `CSI = f ; 0 u` is ignored.
  - Deliberate differences:
    - Under report-events (2) alone, kitty sends a bare ESC for the
      release of Esc, which reads as a second Escape press. toastty sends
      `CSI 27;1:3u`.
    - kitty's report-text (16) without 8 (undefined in the spec) half
      disambiguates; toastty treats it as legacy.
- [ ] **P2 — KBD-2. Keypad keys and modifier keys don't use kitty's own key
  codes.** Found while checking KBD-1.
  - kitty sends KP_Enter as `CSI 57414 u` under disambiguate, and keypad
    digits as `CSI 57400 u` (and so on) under report-all.
  - Under report-all, kitty also reports presses and releases of the
    modifier keys themselves (`CSI 57441;2u` for Shift).
  - toastty sends the main-keyboard codes and no modifier-key events,
    because the encoder doesn't get the physical key.

Performance notes (Xvfb, software present; not conclusive):

- `mpv --vo=kitty` at 1000×562, 30 fps: 26 fps delivered, with one ~0.5 s
  stall. RSS plateaus around 535 MB because the quota bounds it.
- Idle CPU with `cursor.blink = true` was 65–98 % under Xvfb, and 0 % with
  blink off. Re-check on real hardware.

## 11. Verified working

These behave like kitty or the spec. Keep them working when fixing the above.

**Transmission and envelope**
- [x] APC terminators: ESC `\` and BEL, including byte-at-a-time writes and
  an ESC/`\` split across writes. Non-G APCs are ignored.
- [x] 10–20 pipelined commands get correctly ordered replies, and replies
  interleave correctly with DSR.
- [x] Duplicate keys: the last one wins. `i=4294967295` works. Negative,
  overflowing and hex ids get no reply.
- [x] Chunked uploads:
  - Bare `m=` continuations work.
  - Text and CSI between chunks render as in kitty.
  - Concurrent uploads that carry `i=` on their continuations work, which
    kitty can't do.
- [x] Formats: `f=24`, `f=32` and `f=100`. PNG palette, palette + tRNS, gray
  8/16, gray+alpha, 1-bit, RGB16, RGBA16 and interlaced PNGs all decode
  with exact colours. Alpha blending matches kitty within 1–2 levels.
- [x] `o=z` with raw data and with PNG. An 8000² PNG works.
- [x] `t=f` happy paths: `O=`/`S=` offsets and sizes, symlinks, PNG through a
  file. A temp file with the marker is read and deleted.
- [x] Reply formats:
  - `I=` alone gets a reply with both `i=` and `I=`.
  - Both `i` and `I` gets EINVAL.
  - `p=` is echoed on `a=T`, `a=t` and `a=p`.
  - Anonymous commands get no reply.
  - `q=1` and `q=2` work on single-chunk commands.
- [x] Volume: 5000 tiny transmits took 0.17 s and 5000 `a=T` took 0.92 s.
  kitty: 0.59 s and 2.08 s.
- [x] Quota eviction prefers images with no placements, and placed images
  survive.
- [x] Extreme `c`/`r`/`X`/`Y`/`x`/`y`/`w`/`h`/`H`/`V` values don't crash.
  No panics in any test.
- [x] `CSI 14t`, `16t` and `18t` agree with each other after resize and font
  zoom, and so does `TIOCGWINSZ` (SIZE-13).

**Placement and deletion**
- [x] Unnamed placements accumulate. The same `i`+`p` replaces the previous
  placement, both moving and resizing, with no stale pixels. 3000 rapid
  replacements end with a single image in the right spot.
- [x] Cursor after `a=T`/`a=p` lands on the image's last row, one column
  past it, including mid-line placements and ones with `c`/`r`/`X`/`Y`.
- [x] `C=1` doesn't move the cursor. DECOM origin mode works. 40
  back-to-back placements end at the same cursor position as in kitty.
- [x] Every `d=` selector and its uppercase form:
  - a/A, i/I (with `p`), n/N, c/C, p/P, q/Q, r/R, x/X, y/Y, z/Z.
  - Each removes the same placements as kitty.
  - Lowercase keeps the image data; uppercase frees it only when the image
    is no longer referenced.
- [x] Relative placements:
  - ENOPARENT, ECYCLE, depth 8 OK / 9 ETOODEEP.
  - The cursor never moves.
  - Children follow their parent while it stays fully on screen.

**Rendering and layering**
- [x] z bands: z ≥ 0 above text; INT32_MIN/2 ≤ z < 0 below text but above the
  cell background. Stacking by z works.
- [x] Equal z: the higher id draws on top, per the spec. kitty 0.49.2
  instead puts the most recently created on top. This is a spec/kitty
  difference, not a toastty bug.
- [x] Text, EL and ECH over a normal (non-placeholder) image leave the
  image in place. Reverse video, SGR backgrounds and selection highlighting
  over images match kitty.
- [x] Window padding offsets images correctly.
- [x] Font zoom keeps the cell span of `c=`/`r=` placements and rescales
  them, as kitty does.

**Interaction with other terminal actions**
- [x] `CSI J` and `CSI 1J` leave images alone. `CSI 2J`, `CSI 3J` and RIS
  delete on-screen images.
- [x] SU/SD/RI by a few rows, with the image staying fully on screen, move
  it with the text.
- [x] An image placed on the bottom row auto-scrolls the screen (but see
  SCR-10 for `C=1` and the alt screen).
- [x] Alt screen 1049: main and alt images are independent, the main image
  is restored on exit, and re-entering clears alt.
- [x] DECSTBM: images fully outside the region are untouched, and images
  straddling the top margin are left alone.
- [x] Unicode placeholders:
  - 256-colour and 24-bit fg image ids, the third-diacritic high byte, and
    row-only diacritic inheritance all work.
  - So do partial rectangles and wide-character neighbours.
  - IL shifts the block, and placeholders crop row by row at the live top
    edge.
  - icat placeholders on a clean screen render correctly.

**Not covered**
- Wayland, HiDPI, fractional scaling and IME: Xvfb only exercises X11 at
  scale 1.
- Transparent-background compositing.
- Whether the cursor block draws over images (inconclusive in both
  terminals).
- Copy/selection over placeholder cells.
