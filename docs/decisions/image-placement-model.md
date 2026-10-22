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
| IL / DL | Same `scroll_region` call over `[cursor row, bottom margin]`. SCR-6 (kitty doesn't move images on IL/DL) is left for A2. |
| Placing an image (`a=T`, `a=p`, sixel) | The box is anchored at the cursor line and **never** scrolls the screen by itself. |
| Cursor advance after a placement (not `C=1`) | Like kitty: the cursor goes to `row + rows - 1`. If that is past the bottom margin, the region scrolls up by the overshoot (`region_scroll_up`, so DECSTBM is respected). `C=1` skips this, so it never scrolls (SCR-10). |
| ED 2 | Removes placements whose shown rows reach the screen. Placements entirely in scrollback survive (kitty's `clear_filter_func`, checked on kitty-ref). ED 3 removes all. |
| Resize | Each screen's placements are re-anchored so they keep their screen row (the reflow renumbers line ids), and those that now start past the bottom or right edge are dropped. The box is never trimmed. Making images follow reflowed text is SCR-7 (A2). |
| Alt screen | Unchanged: the primary `ImageGrid` is stashed on entry and restored on exit. Alt placements use the alt grid's line ids, and the alt grid has no history, so they are evicted as soon as they scroll off the top. |
| Cell-addressed deletes (`d=p/c/q/y/a`) | Translate the cell through `top_line` and test the shown rows. `d=x` tests columns only (kitty does too), so it also reaches placements in scrollback. |

### kitty 0.49.2 behaviour verified on kitty-ref

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
- **A2 (SCR-6/7/11) and A4 (relative placements).** Relative children resolve
  as `parent.line + V` with no clamp at screen row 0, so a child keeps its
  offset while its parent scrolls into history (most of SCR-8). The column
  still clamps at 0.
- **Unicode placeholders (stack F).** Per-cell placeholder placements are
  still created in `finalize_placeholder_run`, now via `Placement::new` and
  anchored at `top_line + cell.row`. They are left for stack F to move to
  render time.
