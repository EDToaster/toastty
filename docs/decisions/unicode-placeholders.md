# Decision: Unicode placeholders are resolved from the cells at render time

Date: 2026-10-21. Status: **implemented** (#28, checklist §3 PH-1 to PH-11).

## TL;DR

A kitty Unicode-placeholder image is text. Each `U+10EEEE` cell stays in
the grid as an ordinary character. Its foreground colour carries the low
bits of the image id. A `CellExtra::Placeholder` carries the rest: the
row, column and id-MSB diacritics, and the placement id from the SGR 58
underline colour. The `U=1` virtual placements go in a per-screen
`VirtualPlacements` table. Every frame, the renderer splits the rows it
draws into runs of placeholder cells and looks each run up in the table.
It then draws the run's slice of the fitted image.

Nothing ties an image to a screen position, so there is nothing to keep
in sync. Text, EL / ECH / ICH / DCH, scrolling, scroll regions,
scrollback, reflow and tmux redraws all move or erase the image because
they move or erase the cells.

## What it replaced

`finalize_placeholder_run` turned each placeholder cell into its own 1×1
`ImageGrid` placement at native pixel scale when the cell was printed,
and threw the `U=1` placement away. After that, nothing linked the tile
to its cell:

- the image stayed when its text was overwritten;
- it didn't scroll into scrollback;
- reflow truncated it;
- positional deletes punched holes in it;
- placeholders printed before the image arrived never showed;
- each cell cost a draw call.

## Pieces

| Piece | Where |
|---|---|
| Diacritic table, `VirtualPlacement(s)`, fit geometry `placeholder_slice` | `toastty-graphics/src/kitty/placeholder.rs` |
| Per-cell data `CellExtra::Placeholder(PlaceholderExtra)` | `toastty-term/src/extra.rs` |
| Printing, diacritic attachment, run resolution, accessors | `toastty-term/src/term/placeholder.rs` |
| Instance building, dirty-cell clipping, vertical merging | `toastty-render/src/image/placeholder.rs` |

The handler gained one sink hook, `KittySink::place_virtual`. It's called
for `U=1` on `a=T` and `a=p`.

## Semantics (checked live against kitty 0.49.2)

- **Runs.** These follow kitty's `screen_render_line_graphics`. A cell
  continues the run to its left when its colours match, and every
  diacritic it gives agrees with what it would inherit: the same row,
  the next column, and the same MSB. On a run's first cell, a missing
  column defaults to 0 and a missing MSB to 0. A missing row blanks the
  whole run, so bare cells show nothing (PH-11).
- **Ids.** The image id is `msb << 24 | color_to_id(fg)`. `color_to_id`
  is the palette index for SGR 30–37 / 90–97 / `38;5`, the 24-bit value
  for `38;2`, and 0 for the default colour (PH-10). The placement id is
  `color_to_id(underline)`. Placement id 0 picks the image's oldest
  virtual placement, as kitty does.
- **Fit.** The box is `c × r` cells. A 0 dimension falls back to
  `ceil(img / cell)` for that axis on its own, not to the aspect-derived
  value (verified: `c=10` alone puts a 200×100 image 27.5 px down in
  kitty). The image is fitted with its aspect ratio kept and centred.
  A run shows the intersection of its cells with the fitted image, so
  cells outside the image stay blank (PH-2, PH-3).
- **Lifetime.** Virtual placements are removed only by:
  - `d=i`/`I` (scoped by `p=`), `d=n`/`N` and `d=r`/`R`;
  - re-transmitting the image, which per the spec deletes its
    placements (kitty then shows nothing until a new `U=1`);
  - eviction.

  ED 2/3, RIS and every positional delete (`a c p q x y z` and their
  capitals) leave them alone (PH-7). An image with a virtual placement
  counts as placed: it is pinned against quota eviction, and the
  capital positional deletes don't free its bytes.
- **Scope.** There is one table per screen, stashed with the image grid
  on alt-screen switches. In kitty, main's virtual placements don't show
  on alt and alt's are gone after exit. The image *registry* is still
  shared between screens (a separate item). So whenever an image's bytes
  are replaced or freed (re-transmit, eviction, a byte-freeing delete),
  its virtual placements are dropped from *both* tables. Otherwise a
  stashed placement's old `c`/`r` would apply to whatever image later
  reuses the id. Invariant: every virtual placement names a registered
  image.
- **Copy.** A placeholder cell copies as `U+10EEEE` followed by the
  diacritics it was printed with, as kitty does, so pasting reproduces
  the image cell.

## Extras-table budget

Each distinct `(placement id, row, col, MSB)` is one `ExtraTable` entry,
and `kitten icat` uses a random 32-bit id with three diacritics per cell.
The table is capped at 65,535 ids and is shared with grapheme tails and
tab markers, so a long session used to fill it: after about 100 icat
runs, new images stayed blank and combining marks were dropped.

The table now recycles ids, and placeholders get a quota:

- **Reserve.** Placeholder entries are capped at 3/4 of the table
  (`MAX_PLACEHOLDER_ENTRIES`). Grapheme tails and tab markers always keep
  the remaining quarter, so images can't starve combining marks.
- **Reclamation.** When an intern would be refused, `Term::intern_extra`
  first runs a mark pass over every ring slot of both grids, plus the
  spare blank row. `ExtraTable::retain` frees every id no cell refers to
  onto a free list, and later interns reuse those ids. Images that have
  scrolled out of history cost nothing after that.
- **Pressure.** Scrollback can hold more distinct live placeholder
  cells than the quota, for example a long `icat *.png` session.
  `Term::drop_old_placeholder_extras` then drops the placeholder data
  of the oldest scrollback rows until half the quota is in use, and
  reclaims again. Those cells stay `U+10EEEE` text but draw nothing,
  much like images evicted by kitty's storage quota. New images, the
  visible screen and the alt grid always keep theirs.
- **Throttle.** If a pass frees less than 1/16 of the table, what's
  there is live, and the next 4,096 refused interns skip the scan. A
  full table therefore doesn't cost a scan per printed character.

All print-path callers (graphemes, tab markers, placeholders) write the
returned id straight into the grid. The rich-block layout still interns
directly and never triggers a scan, because it holds ids in rows it
hasn't written yet.

## Rendering and cost

- When the active screen has no virtual placements, the builder returns
  immediately, so terminals that never use placeholders pay nothing.
  Otherwise it scans the rendered rows (≤ rows + 2), which is a linear
  pass over the cells.
- **Lookups.** `VirtualPlacements` is indexed. Each image has its
  placements in creation order, so `p=0` takes the first, and a map
  covers named `(image, p)` pairs. Each run then costs one or two hash
  lookups, however many placements an image has.
  - `cargo bench -p toastty-render --bench placeholder` measures a
    hostile frame: 4,096 placements of one image, and 200×60 one-cell
    runs naming a placement id that doesn't exist.
  - That frame takes ~0.5 ms. The earlier linear scan took ~75 ms.
  - A full-screen image takes ~55 µs.
- **Draw calls.** The image pipeline issues one draw per instance.
  Each run is one instance, and runs that stack exactly on consecutive
  rows (same texture, z, x extent in screen and image space, and
  abutting in y) are merged. An undisturbed image is one draw call.
- **Partial frames.** Placeholder images are drawn only over the dirty
  cells. The clean cells keep last frame's pixels, and redrawing a
  translucent image over them would blend it twice. An image or table
  change bumps `image_revision`, which forces a full frame.

## Interface for relative placements (REL-5)

`Term::placeholder_origin(image_id, placement_id)` returns the minimum
row and the minimum column over the visible cells that draw that virtual
placement: the spec's position for a virtual parent. It uses cell
positions, not the fitted image's pixel edge.
