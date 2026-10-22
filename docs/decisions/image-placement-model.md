# Decision: Image Placement Model

**Date:** 2026-10-21
**Status:** Accepted (kitty graphics fix programme, PR A1)
**Slug:** `image-placement-model`

## Problem

Until now `ImageGrid` stored each kitty/sixel placement as a screen-relative
`u16` row/column range. Every scroll path trimmed that range in place without
touching the source rect, and the renderer mapped the whole image onto
whatever cells were left. So an image that scrolled partly off the screen was
squashed, not cropped. Placements were dropped at row 0, so nothing reached
scrollback. The renderer also ignored the viewport offset, so a scrolled-back
view showed images pinned over unrelated history text. See SCR-1 to SCR-5 and
SCR-10 in [`kitty-graphics-checklist.md`](../kitty-graphics-checklist.md).

## Model

A `Placement` (`crates/toastty-graphics/src/image_grid.rs`) is:

| Field | Meaning |
|---|---|
| `line: i64` | Absolute line id of the top row of the reserved cell box, in the owning screen's `Grid` line-id space (`Grid::bottom_id`). |
| `col: u16` | Left column of the box. |
| `rows`, `cols: u16` | The reserved cell box. **Never trimmed** by scrolling, resizing or clipping. |
| `clip_rows: Range<u16>` | Rows of the box still shown, relative to `line`. Normally `0..rows`. Only DECSTBM margin scrolls (and IL/DL) narrow it. |
| `src_rect`, `z`, `pix_offset`, `parent`, `rel_offset` | Unchanged from before. |

The screen row of a placement is `line - top_line`, where `top_line` is the
line id of screen row 0 (`Term::image_top_line()`). Negative rows are in
scrollback, and rows at or past the screen height hang below it.

### Why line ids

`Grid::bottom_id` already gives every retained row a stable id. Selection
uses it too. A full-screen scroll up moves the ids, not the placements, so:

- a placement rides along with its text into scrollback for free, with no
  per-scroll work;
- history eviction is a range check against the oldest retained line,
  computed as `top_line - history_lines` in signed space (not
  `Grid::oldest_retained_id()`, which saturates at 0 after SD with no
  history). `ImageGrid` caches the min/max shown line across its
  placements, so the check on every LF is O(1) unless something can
  actually be evicted;
- output that arrives while the user is scrolled back moves text and images
  together, because the viewport compensates for both in the same way.

### Which operations touch placements

| Operation | What happens to placements |
|---|---|
| LF / IND / SU at full-screen margins | Nothing (ids move). Then `retain_lines` evicts anything that left the history. |
| SD / RI at full-screen margins | With #23, `Grid::scroll_down` moves the visible text under fixed ids, so `shift_rows_from(top, 0, +1)` moves every placement whose shown rows reach the screen down by one, untrimmed, including one straddling the top edge (its hidden rows come back, as in kitty), and re-resolves relative children. Placements entirely in scrollback stay on their history text; kitty shifts those too, and this is a deliberate divergence. (Before #23 the grid renumbered rows with the text. The term detects that from `top_line` changing and does nothing, because the anchors already track.) A box pushed partly below the bottom keeps its whole box, so a later SU brings it all back (kitty does the same). |
| DECSTBM partial-region scroll | `scroll_region`: only placements whose shown rows lie **entirely** inside the margins move (spec, SCR-5). They are clipped at the margin through `clip_rows`, and the clip is permanent, as in kitty. |
| IL / DL | Ordinary placements: nothing (SCR-6). kitty 0.49.2 moves only the text, and keeps an image whole even when DL deletes the rows under it (checked on kitty-ref). Unicode-placeholder images are text, so their per-cell tiles (`Placement::from_placeholder`) get the old `scroll_region` shift with their cells. That is interim: delete it (`TODO(#28)`) once placeholders render from the cells. |
| Scroll inside DECSLRM left/right margins | The text moves only inside the margin rectangle, and so do the images whose columns lie entirely inside the left/right margins, clipped at the top/bottom margins (SCR-11). Images outside a side margin, or straddling one, stay with the text that stays. kitty has no left/right margins, so there the whole region (text and images) moves: inside the rectangle the two agree. |
| Placing an image (`a=T`, `a=p`, sixel) | The box is anchored at the cursor line and **never** scrolls the screen by itself. |
| Cursor advance after a placement (not `C=1`) | Like kitty: the cursor goes to `row + rows - 1`. If that is past the bottom margin, the region scrolls up by the overshoot (`region_scroll_up`, so DECSTBM is respected). `C=1` skips this, so it never scrolls (SCR-10). |
| ED 2 | Removes placements whose shown rows reach the screen. Placements entirely in scrollback survive (kitty's `clear_filter_func`, checked on kitty-ref). ED 3 removes all. |
| Resize | Placements follow their text (SCR-7). The primary reflow (`Grid::resize_reflow_mapped`) returns a `LineRemap`: for each old retained row, the new line id its first cell landed on. Every primary placement is mapped through it, including the stashed primary grid while the alt screen is up and the same-size reflow a rich-block re-layout does. The alt grid's `Grid::resize` keeps its top rows' ids, so alt placements stay with their rows. Nothing is dropped for hanging past the new bottom or right edge, and the box is never trimmed, so shrinking then growing restores the image. Only placements whose text left the history are evicted. Each placement's top-left cell is mapped as a content offset into its logical line (`LineRemap::map_cell`), so it follows its text across rows when a line rewraps (a `C=1` image at column 70 of a 90-cell line moves to the next row at 58 columns) and comes back on widening; width round trips and drag-resizes are exact. Placeholder tiles follow their cells the same way until stack F renders them from the cells. |
| Alt screen | Unchanged: the primary `ImageGrid` is stashed on entry and restored on exit. Alt placements use the alt grid's line ids, and the alt grid has no history, so they are evicted as soon as they scroll off the top. |
| Cell-addressed deletes (`d=p/c/q/y/a`) | Translate the cell through `top_line` and test the shown rows. `d=x` tests columns only (kitty does too), so it also reaches placements in scrollback. |

### kitty 0.49.2 behaviour verified on kitty-ref

- IL/DL never move or clip image placements.
- On a height shrink, images move up with their text into scrollback and
  come back intact when you scroll back.
- On a **width** change, kitty does *not* move images with the rewrapped
  text: the image stays at its screen row and the rewrapped text runs
  under it. It is cropped at the right edge and restored when the window
  widens. toastty deliberately differs here: images follow their text on
  width changes too, as the PM decided for SCR-7. That is the same
  "scroll with the text" rule the spec states, and it keeps an image
  from covering unrelated rewrapped lines. Height-only changes match
  kitty exactly.

- A `C=1` image placed on the last row does not scroll the screen. It is
  clipped at the bottom, and later newlines reveal the rest of it. The box was
  not trimmed.
- An image placed with `C=0` near the bottom of the **alt** screen *does*
  scroll the alt screen by the overshoot. So only `C=1` prevents the scroll,
  on both screens. This differs from the A1 brief, which assumed "never
  scroll on the alt screen". We follow kitty.
- SD then SU restores an image that SD pushed partly below the screen.
- ED 2 keeps images that are entirely in scrollback.

## Rendering

`build_image_instances` (`crates/toastty-render/src/image/instance.rs`) takes
an `ImageView { top_line, scroll_px, clip }`:

- `scroll_px = view_offset_lines * cell_h + view_offset_pixel`. This is the
  same translation that text and rich blocks get: logical row `L` is drawn at
  `(L + view_lines) * cell_h + view_pixel`. Images therefore follow wheel
  scrollback and sub-row smooth scrolling (SCR-2, SCR-4).
- For each placement it computes the full destination rect from the untrimmed
  box plus `pix_offset`, and intersects it with `clip` (the content area) and
  with the `clip_rows` band. It then crops the UVs by the same fractions. A
  partly hidden image is cropped and never squashed.
- The clip matches what the text pass covers: the grid itself
  (`cols × rows` cells) at the live bottom. While scrolled back or mid
  smooth-scroll it also includes the partial row above the top and the
  rows that fill the content area below, exactly as text.
- It is O(placements) per frame, with no per-cell work. Fully clipped
  placements are culled before the registry and texture-cache lookups, so a
  lazy texture uploader (stack C, #24) only sees what is visible.

## Interfaces for later work

- **A3, native pixel sizing.** The destination size is computed in one place:
  `dst_w`/`dst_h` in `build_image_instances`. Today it is `cols × cell_w` by
  `rows × cell_h`. A3 can add a pixel size to `Placement` and use it there.
  The crop and UV math already works for any destination rect. `rows`/`cols`
  stay the cell *reservation*, which drives cursor movement and cell
  hit-testing.
- **A5, per-screen image stores.** `ImageGrid` is already per screen (active
  plus stashed). Line ids are per `Grid`, so a per-screen registry can sit
  beside each `ImageGrid` without touching the anchoring.
- **Prompt-aware resize (OSC 133).** A future "reflow everything except
  the prompt" path (`resize_reflow_at_prompt` / `layout_into_ring` on the
  unmerged `prompt-aware-resize` branch) must also produce a `LineRemap`
  (with `ReflowMap` positions) and feed it to `Term::remap_primary_images`,
  or images will stop following their text when resizing at a prompt.
- **A4 (relative placements).** Relative children resolve
  as `parent.line + V` with no clamp at screen row 0, so a child keeps its
  offset while its parent scrolls into history (most of SCR-8). The column
  still clamps at 0.
- **Unicode placeholders (stack F).** Per-cell placeholder placements are
  still created in `finalize_placeholder_run`, now via `Placement::new` and
  anchored at `top_line + cell.row`. They are left for stack F to move to
  render time.
