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

- [ ] **P0 — SCR-1. Partly scrolled-off images are squashed, not cropped.**
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
- [ ] **P0 — SCR-2. Images don't follow the viewport into scrollback.**
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
- [ ] **P0 — SCR-3. New output arriving while scrolled back moves the image.**
  - Repro: scroll back one notch while a background job prints 6 lines.
  - Expected: kitty keeps the text and the image still.
  - Actual: toastty keeps the text anchored, but the image moves up
    2 rows and loses 2 rows.
  - Cause: same as SCR-2.
- [ ] **P1 — SCR-4. Pixel (smooth) scrolling moves text but not images.**
  - Repro: set `smoothing_function = "linear"` and capture frames during a
    wheel animation.
  - Actual: text slides at fractional-row offsets under an image that stays
    on the cell grid.
  - Cause: `lib.rs:2860-2878` builds image instances without the
    `y_translate` that text and rich blocks get (`lib.rs:1835, 1975, 2763`).
- [ ] **P1 — SCR-5. DECSTBM scrolls images that are only partly inside the
  margins.**
  - Repro: place an image on rows 3–8, then `K raw '\x1b[2;7r\x1b[7;1H\n\n\x1b[r'`.
  - Expected (spec): "only images that are entirely within the page area
    (between the margins) must be scrolled". kitty leaves this image alone.
  - Actual: it moves up 2 rows and is clipped into 5 rows.
  - Cause: `image_grid.rs:423` and `:469` check only `row_range.start`;
    they also need `end <= scroll_bottom`.
- [ ] **P1 — SCR-6. IL/DL (`CSI L` / `CSI M`) move images; kitty doesn't.**
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
- [ ] **P1 — SCR-7. Resize and reflow separate images from their text and
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
- [ ] **P1 — SCR-8. Relative placements drift once their parent scrolls off
  the top.**
  - Repro: a child placed at `V=6` under a parent, then output that scrolls
    the parent partly or fully off the top.
  - Expected: kitty keeps the child at its offset from the parent, cropped
    at the top.
  - Actual: the child drifts by 2–3 rows and is drawn in full at the top.
  - Cause: `resolve_relative_positions` (`image_grid.rs:201-249`) uses the
    parent's clamped row. Once the parent is dropped, the child loses its
    anchor.
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
- [ ] **P0 — SCR-10. An image that doesn't fit below the cursor always
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
- [ ] **P2 — SCR-11. Scrolling inside DECSLRM left/right margins leaves
  images in place.**
  - Repro: `K raw '\x1b[?69h\x1b[5;40s\x1b[3;12r'`, then line feeds inside
    the margins.
  - Expected: kitty doesn't implement DECSLRM; it does a plain region scroll
    and crops the image.
  - Actual: toastty moves the text inside the margin rectangle, but the
    image stays over different text.
  - Cause: `term.rs:1975-1981, 2042-2046` return before touching
    `image_grid`.
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

## 2. Sizing, scaling and rendering

- [ ] **P0 — SIZE-1. Images are stretched to whole cells instead of drawn at
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
- [ ] **P1 — SIZE-2. Giving only `c=` or only `r=` distorts the image.**
  - Repro: a 400×200 image with `c=10`.
  - Expected (spec): "the other one is computed based on the source image
    aspect ratio, so that the image is displayed without distortion". kitty
    draws it at 90×45.
  - Actual: rows round up to 3, and the image is stretched to 100×57.
  - Cause: `handler.rs:857-869` plus SIZE-1.
- [ ] **P2 — SIZE-3. Giving both `c=` and `r=` stretches the image; the spec
  says letterbox.**
  - Spec: "When both are specified the image is letterboxed/pillarboxed to
    prevent distortion".
  - kitty 0.49.2 actually stretches too (36×160 for `c=4,r=8`), so toastty
    currently matches kitty.
  - Decide which one to follow, then add a test.
- [ ] **P1 — SIZE-4. The cell span and aspect ratio come from the full image,
  not the source rect.**
  - Repro: a 200×100 image with `x=0,y=0,w=60,h=40`, with no `c=`/`r=`, then
    with `c=12`.
  - Expected: kitty draws it at 60×40 and 108×72.
  - Actual: toastty draws 210×114, a blurry 3.5× zoom, and 120×76.
  - Cause: `handler.rs:851-869` uses `img_w`/`img_h` instead of the source
    rect's size.
- [ ] **P1 — SIZE-5. A partial source rect is ignored.**
  - Repro: `w=` only, `h=` only, `x=` only or `y=` only.
  - Expected (spec): the defaults are "the entire width/height" from
    `x`/`y`. kitty crops accordingly.
  - Actual: the full image is drawn.
  - Cause:
    - `handler.rs:833` treats `w==0 && h==0` as full.
    - `SrcRect::is_full()` (`image_grid.rs:86-88`) treats `w==0 || h==0` as
      full.
    - So any rect with one dimension unset collapses to the full image.
- [ ] **P1 — SIZE-6. The source rect isn't intersected with the image.**
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
- [ ] **P1 — SIZE-7. Images wider or taller than the space left are squashed,
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
- [ ] **P0 — SIZE-8. At most 14 images render; the rest silently disappear.**
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
- [ ] **P1 — SIZE-9. Images with z below INT32_MIN/2 are invisible.**
  - Repro: `K send a=T,i=1,z=-1073741825 --png img.png` over default and
    `\e[41m` cells.
  - Expected (spec): drawn "under cells with non-default background colors".
    kitty shows the image through default-bg cells, and red cells cover it.
  - Actual: nothing is visible.
  - Cause: `lib.rs:3089-3114` draws this layer first, then the bg pass paints
    every cell opaque, including default-bg cells.
- [ ] **P2 — SIZE-10. `X=`/`Y=` pixel offsets aren't clamped to the cell, and
  aren't fitted into `c`/`r`.**
  - Repro: `X=25,Y=40` with 10×19 cells, and `X=5,c=6`.
  - Expected (spec): "the offsets must be smaller than the size of the
    cell". kitty clamps to +8/+19, and fits `X=5,c=6` inside the 6-column
    box.
  - Actual: toastty applies the values as given, and the image overruns the
    box by 5 px.
  - Cause: `instance.rs:143-146` and `handler.rs:893`.
- [ ] **P2 — SIZE-11. Font zoom rescales natural-size images.**
  - Expected: kitty keeps a natural-size image (no `c=`/`r=`) at its pixel
    size. For placements with explicit `c=`/`r=` it keeps the cell span and
    rescales; toastty matches kitty there.
  - Actual: toastty keeps the 21×6-cell span and grows the image from
    205×105 to 273×156. This falls out of SIZE-1.
- [ ] **P2 — SIZE-12. Downscaling aliases badly.**
  - Repro: a 1500×900 grid image with `c=30`.
  - Actual: the 10 px grid's vertical lines vanish. kitty also aliases, but
    the grid stays visible.
  - Cause: `pipeline.rs:161-163` uses linear filtering with no mipmaps.
  - Consider mipmaps, or a box prefilter on upload.
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

- [ ] **P0 — PH-1. Placeholder images don't follow their cells.**
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
- [ ] **P0 — PH-2. Placeholder images aren't scaled to the virtual
  placement.**
  - Repro: `K send a=T,i=1,U=1,q=2,c=10,r=5 --png img.png` with a 200×100
    image, then print a 10×5 block of placeholders. Or run
    `kitten icat --unicode-placeholder`.
  - Expected (spec): the image fits the placeholder rectangle, keeping its
    aspect ratio, centred. kitty does this.
  - Actual: a native-scale top-left crop.
  - Cause: `term.rs:2571-2629`. This also means the placement id in the
    underline colour can't select a different size.
- [ ] **P1 — PH-3. Cells past the image's edge show a stretched 1 px edge
  strip.**
  - Expected: blank. With icat at the default cell size, toastty shows a
    white bar under the image instead.
  - Cause: the clamp at `term.rs:2620-2623`.
- [ ] **P1 — PH-4. Placeholder images are lost in scrollback.**
  - Repro: scroll placeholder lines into history, then scroll back.
  - Expected: they're text, so kitty shows them.
  - Actual: they're missing. A leftover strip stays pinned at screen row 0.
  - Cause: tiles are `ImageGrid` placements, so SCR-2 applies;
    `image_grid.rs:356`.
- [ ] **P1 — PH-5. Placeholders printed before the image arrives never
  display.**
  - Expected: kitty shows the image once it's transmitted.
  - Cause: the lookup at `term.rs:2610` is never re-checked.
- [ ] **P1 — PH-6. Re-sending an image after `d=I` doesn't refill existing
  placeholder cells.**
  - Expected: kitty shows the new image in the existing cells.
- [ ] **P1 — PH-7. Positional deletes erase placeholder images.**
  - Repro: `d=a` removes them, and `d=p` punches a one-cell hole.
  - Expected: kitty leaves placeholder images alone for position-based
    selectors.
- [ ] **P1 — PH-8. Narrowing the window permanently truncates a placeholder
  image.**
  - Repro: narrow the window, then widen it again.
  - Actual: the text reflows back, but the image stays cut to the narrow
    width.
  - Cause: `clip_to` (`term.rs:1865`).
- [ ] **P2 — PH-9. The image shows through placeholders without any `U=1`
  virtual placement.**
  - Expected: kitty shows nothing.
- [ ] **P2 — PH-10. Basic SGR colours (30–37, 90–97) aren't accepted as the
  image id.**
  - Only `38;5` and `38;2` work.
  - Cause: `term.rs:5210`.
- [ ] **P2 — PH-11. Bare `U+10EEEE` cells with no diacritics render image
  row 0.**
  - Expected: kitty shows nothing.
  - Lenient rather than harmful.

## 4. Relative placements

- [ ] **P1 — REL-1. A rejected relative re-placement deletes the existing
  placement.**
  - Repro: re-send a child's `i`/`p` with a `P`/`Q` that causes ECYCLE or
    points at itself.
  - Actual: the old placement is already gone, so later children of it get
    ENOPARENT. kitty keeps everything.
  - Cause: `term.rs:4760-4769` removes before validating.
- [ ] **P1 — REL-2. Deleting a parent leaves its descendants on screen.**
  - Expected (kitty, per spec): children and grandchildren are deleted, and
    images left with no placements are freed even on lowercase `d=i`.
  - Cause: `delete_image` doesn't cascade; `image_grid.rs:194-200` keeps
    orphans in place.
- [ ] **P1 — REL-3. Children don't follow a parent moved by re-sending the
  same `i`+`p`.**
  - Cause: `place_image` (`term.rs:4729-4743`) never calls
    `resolve_relative_positions`.
- [ ] **P1 — REL-4. `a=T` ignores `P`/`Q`/`H`/`V`.**
  - Actual: the image goes to the cursor and moves it.
  - Cause: `handler.rs:596-615`. Only `handle_place` has the relative
    branch.
- [ ] **P1 — REL-5. A child of a virtual (`U=1`) parent anchors to the last
  placeholder cell.**
  - Expected (spec): anchor to the minimum x/y of the placeholder cells.
    kitty puts the children at (5,27)/(9,20); toastty at (7,32)/(11,25).
  - Cause: `ImageGrid::find` (`image_grid.rs:139-145`) returns the most
    recent tile.
- [ ] **P2 — REL-6. Negative `H`/`V` offsets are clamped to column/row 0.**
  - Expected: kitty lets the child go partly off-screen and clips it.
  - Cause: `offset_origin` (`image_grid.rs:538-545`).
- [ ] **P2 — REL-7. Error codes differ from kitty.**

  | Case | toastty | kitty |
  |---|---|---|
  | Self-reference to an existing placement | ECYCLE | EINVAL |
  | Self-reference to a non-existent placement | ECYCLE | ENOPARENT |
  | `U=1` with `P`/`Q` (spec requires EINVAL) | OK | EINVAL |
  | `P` without `Q`, or `Q` without `P` | EINVAL | OK |

## 5. Placement, cursor and image lifetime

- [ ] **P0 — PLC-1. `a=p` on an unknown or freed id replies OK.**
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
- [ ] **P1 — PLC-2. Re-transmitting an existing id keeps its placements,
  which then show the new pixels.**
  - Repro: `a=T,i=1` red, then `a=t,i=1` blue.
  - Expected (spec): "the existing image and all its placements must be
    deleted. The new data … is not actually displayed until a placement for
    it is created". kitty shows nothing.
  - Actual: the old placement turns blue. A second `a=T` with the same id
    shows two copies.
  - Cause: `register_image` (`term.rs:4591-4650`) replaces the registry entry
    without dropping that id's placements.
- [ ] **P1 — PLC-3. `a=p,I=N` is rejected with EINVAL.**
  - Expected (spec): commands that use an image number act on the newest
    image with that number. kitty replies `i=2,I=144;OK`.
  - Actual: `I=144;EINVAL:place requires i=`.
  - Cause: `handler.rs:624-626`. `Term` already keeps an
    `image_number_to_id` map, but uses it only for `d=n`.
- [ ] **P1 — PLC-4. Anonymous images get small ids other clients can
  address.**
  - Repro: an anonymous `a=T`, then another program sends `a=t,i=1` or
    `a=d,d=I,i=1..2000`.
  - Expected: kitty's anonymous image survives commands that name ids.
  - Actual: toastty's anonymous image is overwritten or deleted.
  - Cause: `registry.rs:163, 252` (`next_free_id` hands out the lowest free
    id). Allocate from the top of the id space, or keep anonymous images
    out of the id table.
- [ ] **P2 — PLC-5. A placement that ends at the right edge clamps the cursor
  to the last column.**
  - Actual: the next text lands hidden under the image. kitty wraps to
    column 1 of the next line.
  - Cause: `term.rs:4984-5010`.
- [ ] **P2 — PLC-6. Main and alt screens share one image store.**
  - Repro: `d=I` on the alt screen.
  - Actual: it frees the main screen's image, which is blank after leaving
    alt. kitty keeps a separate store per screen.
  - This bites in practice: a full-screen previewer deleting its own ids
    wipes a shell's icat image that had the same id.
  - Cause: `term.rs:4513, 4854` don't check the stashed main-screen grid.
- [ ] **P2 — PLC-7. Intermittent: an image was missing after re-sending the
  same id.**
  - Seen once and not reproduced in 8 more runs.
  - Possibly a renderer race on same-id replacement. Keep an eye on it.

## 6. Deletion

Every `d=` selector and its uppercase form removed the same placements as
kitty, with the same keep/free behaviour and no leftover pixels. The
remaining gaps:

- [ ] **P1 — DEL-1. `a=d` sends a reply.**
  - Repro: `K send a=d,d=i,i=148 --reply 0.5`. The same happens for ids that
    don't exist.
  - Expected: kitty never replies to deletes, even with `i=` and `q=0`.
  - Actual: toastty replies `i=148;OK`.
  - Clients that don't read replies get those bytes on stdin, where readline
    turns them into garbage on the prompt.
  - Cause: `handle_delete` (`handler.rs:704-707`).
- [ ] **P2 — DEL-2. `d=F` is a no-op.**
  - Expected: kitty deletes and frees a single-frame image. `d=f` is a no-op
    in both.
  - Cause: `delete_image` has no `f`/`F` arm.
- [ ] **P2 — DEL-3. A delete during a chunked upload doesn't abort it.**
  - Expected (spec): the upload is aborted. kitty replies ENOENT to the
    remaining chunks.
  - Actual: the upload completes and displays.
  - Cause: `handler.rs:704`.

## 7. Security and robustness

Anything that writes to the PTY can send these commands: `cat` of a file,
`curl` output, a remote host over ssh.

- [ ] **P0 — SEC-1. `t=t` deletes files outside the temp directory.**
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
- [ ] **P0 — SEC-2. `t=f` on a FIFO freezes the terminal.**
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
- [ ] **P0 — SEC-3. `t=f` with no `S=` reads without a limit.**
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
- [ ] **P1 — SEC-4. File errors reveal details about local files.**
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
- [ ] **P1 — SEC-5. zlib decompression (`o=z`) has no size limit.**
  - Repro: `i=1,s=1,v=1,f=32,o=z` with 1 GiB of zeros, compressed.
  - Actual: toastty inflates all of it (RSS 101 MB → 1123 MB) before
    replying EINVAL. A 400 MiB stream peaked at about 1 GB. kitty stops at
    the expected size (`s*v*4`) and uses no extra memory.
  - Cause: `inflate_zlib` (`handler.rs:746-751`) calls `read_to_end` with no
    cap. Use `take(expected + 1)`.
- [ ] **P1 — SEC-6. No limit on the number of in-flight chunked uploads, and
  abandoned uploads are never freed.**
  - Repro: 16 chunked uploads with different `i=`, each 48 MiB, none
    finished.
  - Actual: toastty's RSS reached 917 MB and stayed there. kitty: 313 MB.
  - Cause: `pending` (`handler.rs:418-425`) has a per-upload cap
    (`DEFAULT_PENDING_CAP`, `handler.rs:30`) but no total cap, no count cap
    and no expiry.
- [ ] **P2 — SEC-7. Oversized PNGs are fully decoded before being rejected.**
  - Repro: a 12000×12000 PNG.
  - Actual: toastty peaks at 1.28 GB RSS, then replies EFBIG. kitty replies
    `ENOMEM: PNG image is too large` straight away.
  - Check the IHDR dimensions against the quota before decoding.

## 8. Wire format and replies

- [ ] **P1 — WIRE-1. `a=q` with `t=f`, `t=t` or `t=s` replies ENOTSUP.**
  - Expected: kitty replies OK. For `t=t` it deletes the file; for a missing
    file it replies EBADF.
  - Effect: `kitten icat --detect-support` reports `stream` in toastty
    (kitty: `memory`), so icat never auto-selects file transfer.
  - Cause: `handler.rs:278-279`.
- [ ] **P1 — WIRE-2. Shared memory (`t=s`) is unsupported, and the shm object
  isn't unlinked.**
  - Actual: `kitten icat --transfer-mode=memory` shows nothing and exits 0,
    because icat sends `q=2`, and objects pile up in `/dev/shm`.
  - The spec says the terminal "must read the data from the memory object
    and then unlink and close it".
  - Cause: `handler.rs:519-533`.
  - Implement it with `shm_open`/`mmap`/`shm_unlink` (rustix has these),
    after the SEC-1–SEC-3 fixes.
- [ ] **P0 — WIRE-3 (= APP-1). Animation commands without `q=` get ENOTSUP
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
- [ ] **P2 — WIRE-4. Base64 is decoded over the joined buffer, not per
  chunk.**
  - Repro: chunks each padded with `=` (`…m=1;AQIDBA==` then `m=0;BQY=`).
  - Actual: toastty replies `EINVAL:bad base64`; kitty accepts it.
  - In the other direction, toastty is more lenient than kitty about
    embedded whitespace and chunks that aren't a multiple of 4. Keep that.
  - Cause: `decode_base64` (`handler.rs:733-744`).
- [ ] **P2 — WIRE-5. An 8-bit `0x9C` byte inside an APC ends it.**
  - Repro: a non-G APC whose UTF-8 text contains `U+041C` (`\xd0\x9c`).
  - Actual: the rest of the payload is printed to the screen. kitty
    swallows the whole sequence.
  - Cause: `crates/toastty-parser/src/parser.rs:171-181`. In UTF-8 mode,
    C1 ST shouldn't end an APC.
- [ ] **P2 — WIRE-6. Malformed headers get an EINVAL reply; kitty stays
  silent.**
  - Cases: `s=abc`, `s=`, `q=3`, `m=2`, `a=X`, `t=x`, `o=y`, `x=-5`,
    `z=2147483648`.
  - The comment at `emit_bad_header_einval` (`handler.rs:922`) says kitty
    replies EINVAL here. kitty 0.49.2 doesn't.
  - Pick one and document it. Replying is defensible, but it is more
    injected stdin (DEL-1).
- [ ] **P2 — WIRE-7. Error codes and validation differ from kitty.**

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

- [ ] **P2 — WIRE-8. `q=` on continuation chunks is ignored.**
  - Actual: toastty uses the first chunk's `q`, so a `q=2` sent only on the
    final chunk still gets `i=60;OK`. kitty stays silent.
  - Cause: `handler.rs:383-397`.
- [ ] **P2 — WIRE-9. Abandoned-upload state corrupts later commands.**
  - Cases:
    - A bare `m=0` after an abandoned upload completes it with EINVAL.
    - A later `a=t,i=57` was appended to a leftover upload and failed.
    - An anonymous `a=T,I=965` was swallowed by a stale anonymous upload,
      so its reply never came.
  - Cause: `handler.rs:352-366, 418`.
  - Drop pending state on any new first chunk without `m=`, and expire old
    uploads (see SEC-6).
- [ ] **P2 — WIRE-10. Chunked `a=q` handles only the first chunk.**
  - Actual: the rest of the chunks route to a different pending upload.
  - Cause: `handle_query` (`handler.rs:271`).
- [ ] **P2 — WIRE-11. Valid transmissions are rejected by `S=` or the pending
  cap.**
  - Cases:
    - `S=999999999` on a direct 1×1 transmission: `EFBIG:declared size >
      budget`. For `t=d`, `S=` is only a hint.
    - A 4096² RGBA chunked upload (64 MiB raw, 89 MiB base64):
      `EFBIG:pending overflow`.
    - kitty accepts both.
  - Cause: `handler.rs:383-388, 403-405`. The cap counts base64 bytes.
- [ ] **P2 — WIRE-12. Animation and composition reply ENOTSUP even for ids
  that don't exist.**
  - Expected: kitty replies ENOENT.

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
  - `a=c,C=2` currently fails header parsing (`C` is parsed as a bool),
    giving EINVAL instead of ENOTSUP.
- [ ] **P1 — Shared-memory transmission** (`t=s`). See WIRE-2.
- [ ] **P2 — Usage hints** (`N=1` transient).
  - The key is accepted and ignored, which the spec allows.
  - It could feed eviction priority.

## 10. Real-world client matrix

| Client / feature | toastty | kitty |
|---|---|---|
| `kitten icat` PNG/JPEG, stream and file modes | works, stretched (SIZE-1) | works |
| `kitten icat --transfer-mode=memory` | blank, shm leak (WIRE-2) | works |
| `kitten icat --detect-support` | `stream` (WIRE-1) | `memory` |
| `kitten icat --place` / `--align` | works, ~1 row too tall (SIZE-13) | works |
| `kitten icat --scale-up` | wrong aspect ratio (SIZE-1, SIZE-13) | works |
| `kitten icat` animated GIF | still frame + text injected into the shell (WIRE-3) | animates |
| More than 14 images, or 500 tiny ones | only 14 visible (SIZE-8) | all visible |
| `kitten icat --unicode-placeholder` | wrong scale; ghosts when cells change (PH-1, PH-2) | works |
| `kitten icat --clear`, `toastty image` | work | work |
| yazi preview: navigation, resize, quit | works | works |
| tmux passthrough icat, copy-mode, detach/reattach | works | works |
| tmux new-window / split over an image | stale image over other content (PH-1) | works |
| yazi inside tmux | works | works |
| `mpv --vo=kitty` incl. resize | works, ~26 fps, one core at 100 % | works |
| Rapid re-placement (3000 moves); 30k lines with 310 images | works | works |

Seen during app testing, not a graphics bug:

- [ ] **P2 — Keyboard-protocol flags leak to the main screen after a
  full-screen app is killed.**
  - Repro: `timeout -s KILL yazi`, then `printf '\e[?1049l'`.
  - Actual: space arrives as `u` and Enter doesn't submit, until a
    `CSI < u`. kitty keeps keyboard flags per screen.

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
