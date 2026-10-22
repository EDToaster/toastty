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
| IL / DL | Ordinary placements: nothing (SCR-6). kitty 0.49.2 moves only the text, and keeps an image whole even when DL deletes the rows under it (checked on kitty-ref). Unicode-placeholder images are text: they render from the placeholder cells (#28), so they move with their cells and need no image-grid shift. |
| Scroll inside DECSLRM left/right margins | The text moves only inside the margin rectangle, and so do the images whose columns lie entirely inside the left/right margins, clipped at the top/bottom margins (SCR-11). Images outside a side margin, or straddling one, stay with the text that stays. kitty has no left/right margins, so there the whole region (text and images) moves: inside the rectangle the two agree. |
| Placing an image (`a=T`, `a=p`, sixel) | The box is anchored at the cursor line and **never** scrolls the screen by itself. |
| Cursor advance after a placement (not `C=1`) | Like kitty: the cursor goes to `row + rows - 1`. If that is past the bottom margin, the region scrolls up by the overshoot (`region_scroll_up`, so DECSTBM is respected). `C=1` skips this, so it never scrolls (SCR-10). |
| ED 2 | Removes placements whose shown rows reach the screen. Placements entirely in scrollback survive (kitty's `clear_filter_func`, checked on kitty-ref). ED 3 removes all. |
| Resize | Placements follow their text (SCR-7). The primary reflow (`Grid::resize_reflow_mapped`) returns a `LineRemap`: for each old retained row, the new line id its first cell landed on. Every primary placement is mapped through it, including the stashed primary grid while the alt screen is up and the same-size reflow a rich-block re-layout does. The alt grid's `Grid::resize` keeps its top rows' ids, so alt placements stay with their rows. Nothing is dropped for hanging past the new bottom or right edge, and the box is never trimmed, so shrinking then growing restores the image. Only placements whose text left the history are evicted. Each placement's top-left cell is mapped as a content offset into its logical line (`LineRemap::map_cell`), so it follows its text across rows when a line rewraps (a `C=1` image at column 70 of a 90-cell line moves to the next row at 58 columns) and comes back on widening; width round trips and drag-resizes are exact. Placeholder images render from their cells (#28), so they follow a rewrap exactly like text. |
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

## Size

The cell box is only a **reservation**. It drives cursor motion, cell
hit-tests and deletes. What is drawn is decided at render time, with the
current cell size, by `Placement::draw_rect` (A3). The rules below were
measured on kitty 0.49.2 with PIL bounding boxes (9×20 px cells):

| Given | Drawn size | Reserved cells |
|---|---|---|
| neither `c=` nor `r=` | the source rect's native pixels (205×105 → 205×105; 3×50 → 3×50) | `ceil((w + X) / cell_w)` × `ceil((h + Y) / cell_h)` |
| only `c=` | width `c·cell_w − X`, height from the source aspect, rounded (400×200 `c=10` → 90×45; 333×100 `c=10` → 90×27) | `c` × `ceil((c·cell_w + X) · h / w / cell_h)`: the given span plus its own offset, at the aspect; `Y` doesn't count (`c=10,Y=19` → 10×3; `c=4,X=8` → 4×2) |
| only `r=` | height `r·cell_h − Y`, width from the aspect (400×200 `r=3` → 120×60) | `ceil((r·cell_h + Y) · w / h / cell_w)` × `r`; `X` doesn't count (`r=3,X=9` → 14×3; `r=3,Y=19` → 18×3) |
| both | the box less the offsets, stretched (200×100 `c=4,r=8` → 36×160) | `c` × `r` |

- The spec says to letterbox when both are given. kitty stretches, and so
  does toastty (SIZE-3).
- `X=`/`Y=` are clamped to `cell − 1` (`X=25,Y=40` → +8,+19). For a
  natural-size image both widen the reservation (`X=8,Y=19` on 200×100
  reserves 24×6 cells). With one side given, only that side's offset
  counts, through the derived side (table above). The reservation is
  never rounded before the ceiling: `c=10` on 1000×449 takes 3 rows for
  40.41 px. All of this was measured with DSR after `a=T` on kitty-ref
  and is locked by `Placement::reserved_cells` tests (review of #39).
- The source rect (`x`/`y`/`w`/`h`) is kept as given. `SrcRect::resolve`
  applies the spec's defaults and intersects it with the image:
  - unset `w`/`h` run to the edge (`x=150` alone → 50×100);
  - a rect partly outside shows the inside part (`x=150,w=100` → 50×100);
  - a rect fully outside shows nothing;
  - an oversized rect shows the full image.

  The size and aspect come from the resolved rect (`w=60,h=40,c=12` →
  108×72).
- **Font zoom** keeps native-size images at their pixels, and
  `c=`/`r=` images at their cell span. kitty-ref after two zoom steps:
  205×105 stays 205×105, and `c=10` on 400×200 becomes 120×60 at 12 px
  cells. This falls out of computing the size at render time (SIZE-11).
  The **reservation** is recomputed too, as kitty does
  (`Term::set_cell_pixel_size` → `ImageGrid::reserve_all`, both the
  active and the stashed grid). On kitty-ref, after zooming in a `d=p`
  outside the drawn image but inside the old reservation misses it, and
  after zooming out one inside the drawn image but outside the old
  reservation hits it. The cursor isn't moved.
- **Pixel exactness:** the quad's origin is snapped to whole window
  pixels, including a centered grid's half-pixel lead
  (`ImageView::origin`). A native-size image then maps texels 1:1 under
  the linear sampler. The scroll offset stays exact, as it does for text.
- **Edges:**
  - A box wider or taller than the space left keeps its size and is
    cropped at the grid edge at native scale (SIZE-7; 1500×100 at column
    20 shows its first 800 px).
  - In the pending-wrap state, kitty puts the image past the right edge,
    so it is invisible, and toastty does the same.
  - After a placement that reaches the right edge, the cursor wraps to
    column 1 of the next line: kitty's `if (x >= columns) { x = 0; y++; }`
    (PLC-5).
- Placeholder tiles and the `Placement::new` default have `fit_cols` /
  `fit_rows` equal to the box, so they stretch to their cells as before.
  Sixel images draw at native size.

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

- **Native pixel sizing (A3, done).** See "Size" above. A future
  letterbox option would only change `Placement::draw_rect`.
- **A5, per-screen image stores.** `ImageGrid` is already per screen (active
  plus stashed). Line ids are per `Grid`, so a per-screen registry can sit
  beside each `ImageGrid` without touching the anchoring.
- **Prompt-aware resize (OSC 133).** A future "reflow everything except
  the prompt" path (`resize_reflow_at_prompt` / `layout_into_ring` on the
  unmerged `prompt-aware-resize` branch) must also produce a `LineRemap`
  (with `ReflowMap` positions) and feed it to `Term::remap_primary_images`,
  or images will stop following their text when resizing at a prompt.
- **A4 (relative placements), done in #40.** Every rule below was
  checked on kitty-ref (kitty 0.49.2):
  - **Position.** Children resolve as `parent.line + V` and
    `parent.col + H`, with no clamp on either axis. `Placement::col` is
    signed, so a negative `H=` puts the child partly left of the screen,
    where the renderer crops it (REL-6).
  - **Validation before change.** `ImageGrid::add_relative` checks the
    whole parent reference before it touches anything, so a rejected
    re-placement leaves the old one alone (REL-1).
  - **Replace in place.** A named `(i, p)` is replaced in place by
    `ImageGrid::upsert`, for relative and absolute placements alike. It
    keeps its draw order, and its children stay attached and are
    re-resolved, so they follow it (REL-3).
  - **Lifetime.** A relative placement lives as long as its parent. Every
    `ImageGrid` removal (`remove_where` and everything built on it:
    deletes, ED 2, history eviction, margin scrolls) also drops orphaned
    descendants, recursively. That includes a parent scrolling out of
    the history while the child's rows are still retained (SCR-8; kitty
    does the same).
  - **Binding.** `add_relative` resolves the parent reference to one
    concrete placement when the child is created, and stores its handle
    in `Placement::parent_ref`. From then on, positions, the self and
    cycle checks, the depth cap and the delete cascade all follow
    handles, never the ids.
    - This matters for `P=` without `Q=`. kitty binds that to the parent
      image's oldest placement *at that moment*, so a later delete of
      that placement takes the child with it, instead of re-anchoring
      the child on another placement of the image.
    - Replacing a placement in place (`upsert`) keeps its handle, so its
      children stay bound.
    - A child of a Unicode-placeholder parent (a group of tiles) is still
      found by ids, until #28.
    - The orphan sweep builds a set of live handles once per pass and
      removes with one drain, so it is O(n) per pass.
  - **Freeing child images.** The cascaded children are returned
    separately (`ImageGrid::remove_where_split`). Whether a child removed
    *because its parent was* also loses its image depends on the delete,
    as on kitty-ref 0.49.2:
    - **Image freed** once it has no placement left: `d=i/I`, `d=n/N`,
      `d=r/R`, `d=z/Z` and `d=q/Q` (lowercase too), plus every uppercase
      delete.
    - **Image kept:** lowercase `d=a`, `d=p`, `d=x`, `d=y` and `d=c`, and a
      parent leaving the history.
    - **Also freed** when the parent's image is re-transmitted or evicted
      by the quota (kitty-ref, ENOENT for the child afterwards).
    - All of this is decided in one place, `ScreenImages::release` (see
      `image-store.md`), from the `Free` rule each removal path passes:
      `orphans` for uppercase deletes, `children` for the id-like
      deletes, re-transmission, quota eviction and cap changes. Every
      path, on either screen, goes through it.
  - **`ESC[2J`.** Relative children aren't cleared themselves; they go
    with their parent. kitty-ref: a child on screen whose parent is in
    the history survives `ESC[2J`, image and all, while a child in the
    history whose parent is on screen goes with it (and PLC-8 frees both
    images).
    - A child deleted directly keeps its image on a lowercase delete
      (REL-2).
  - **Margin scrolls.** Relative children don't scroll on their own. They
    follow their parent through the resolve (#31), and they take on the
    rows their parent lost to a margin (`inherited_clip`). kitty-ref:
    - a child inside the region whose parent is outside stays whole;
    - with both inside, two LFs cut the parent's top row at the margin,
      and the child loses its top row too.
  - **Parent references.** `P=` without `Q=` is bound to the parent
    image's oldest placement (see "Binding"), and `Q=` without `P=` is an
    ordinary placement. Self-reference gives EINVAL if the placement exists and
    ENOPARENT if not. `a=p,U=1` with `P=` gives EINVAL, while
    `a=T,U=1,P=…` is accepted and makes the virtual placement (REL-7).
    `a=T` with `P=` is relative like `a=p` and never moves the cursor
    (REL-4).
  - **Placeholder parents.** A child of a Unicode-placeholder parent
    measures from the minimum line and the minimum column over the
    parent's placeholder images (`ImageGrid::parent_origin`, REL-5).
    This uses placeholder tiles until #28 lands; after that it asks
    `Term::placeholder_origin`.
- **Unicode placeholders (#28).** Placeholder images are not in the image
  grid. They are resolved from the placeholder cells at render time, so
  everything in this document that moves text moves them too.
