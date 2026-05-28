# Toastty kitty graphics protocol — compliance audit

Audit of the toastty implementation against the kitty terminal graphics protocol
spec (<https://sw.kovidgoyal.net/kitty/graphics-protocol/>) and the reference
kitty C implementation. Five parallel audits covered: wire format & transport,
display & placement, deletion & lifecycle, animation, and control-keys &
interaction with other terminal actions.

Findings are grouped into three categories below: **Blocker**, **Major**,
**Minor**. Inside each category, items are listed in roughly fix-order with
file:line references.

---

## 1. Blocker

User-visible spec violations. Daily-use clients (`kitten icat`, `yazi`,
`helix`, `less` with images, `image.nvim`) will hit these.

### B1. `CSI 2J` / `CSI 3J` does not clear images
- **Spec**: "The clear screen escape code (usually `ESC [ 2J`) should also
  clear all images. This is so that the clear command works."
- **Location**: `crates/toastty-term/src/term.rs:2189-2193`
- **Behavior**: the `2`/`3` arm of `erase_display` calls
  `grid.clear_visible(style)` + `damage.mark_all()`, but never
  `image_grid.clear()`. Images visibly survive a `clear`.
- **Fix**: in the `2`/`3` arm, call `self.image_grid.clear()` and bump
  `image_revision`.

### B2. Alt-screen 1049 enter/exit permanently destroys primary images
- **Spec**: primary and alt screens must maintain independent image lists.
- **Location**: `crates/toastty-term/src/term.rs:2606-2656`
  (`enter_alt_screen` / `exit_alt_screen`)
- **Behavior**: there is only one `ImageGrid` per `Term`. Entry clears it and
  exit clears it again. Anything that toggles the alt screen (helix, yazi,
  less, vim) wipes primary-screen images permanently.
- **Fix**: mirror the existing primary/alt `Grid` duplication — keep
  `primary_image_grid` / `alt_image_grid` and swap on 1049 enter/exit.

### B3. DECSTBM partial-region scroll doesn't move images
- **Spec**: "only images that are entirely within the page area must be
  scrolled. When scrolling them would cause them to extend outside the page
  area, they must be clipped."
- **Location**: `crates/toastty-term/src/term.rs:1491-1515, 1535-1564`
  (partial branches of `region_scroll_up` / `region_scroll_down`)
- **Behavior**: `image_grid.shift_rows_{up,down}` is only called on the
  full-region path. With a scroll region set, images stay frozen while text
  scrolls beneath them. The `ImageGrid::shift_rows_up(n, scroll_top)` API
  already takes a `scroll_top` parameter for exactly this case.
- **Fix**: invoke `shift_rows_{up,down}` with the region bounds on the
  partial branch too.

### B4. `d=i,i=N,p=M` ignores the placement id and deletes all placements of N
- **Spec**: "If you specify a `p` key for the placement id as well, then only
  the placement with the specified image id and placement id will be
  deleted."
- **Location**: `crates/toastty-term/src/term.rs:3292-3298`
- **Behavior**: the arm calls `image_grid.remove_image(image_id)`, which
  matches solely on `image_id`. `header.placement_id` is parsed but never
  consulted.
- **Fix**: when `header.placement_id != 0`, scope the predicate to
  `p.image_id == img && p.placement_id == pid`.

### B5. `d=I` frees image bytes unconditionally
- **Spec**: uppercase variants free image data "provided that the image is
  not referenced elsewhere" — i.e. only when no placements of that image
  remain.
- **Location**: `crates/toastty-term/src/term.rs:3292-3298`
- **Behavior**: `if drop_bytes { self.image_registry.remove(...) }` runs
  regardless of whether other placements still reference the image.
  Combined with B4, `d=I,i=N,p=M` wipes the image *and* every other
  placement.
- **Fix**: only free bytes when no placements of `image_id` remain in any
  grid after the placement removal.

### B6. `d=p` / `d=P` has the wrong selector entirely
- **Spec**: `p/P` deletes "all placements that intersect a specific cell, the
  cell is specified using the `x` and `y` keys" (lowercase, cell coords).
- **Location**: `crates/toastty-term/src/term.rs:3305-3311`
- **Behavior**: implemented as filter by `(image_id, placement_id)` using
  `i=`/`p=` headers. Wrong selector key letters; wrong semantic; and
  `drop_bytes` is never consulted on this arm (uppercase `P` cannot free).
- **Fix**: read lowercase `x=` / `y=` (new header fields, distinct from
  `src_x` / `src_y` which are display-context pixel coords) and remove
  placements whose `(col_range, row_range)` contain the cell.

### B7. `d=r` / `d=R` has the wrong selector entirely
- **Spec**: `r/R` deletes "all images whose id is greater than or equal to
  the value of the `x` key and less than or equal to the value of the `y`
  key" (id-range delete; kitty 0.33+).
- **Location**: `crates/toastty-term/src/term.rs:3313-3318`
- **Behavior**: implemented as "clear row at `cell_y`" using the `Y=` header
  key. That is closer to the `y/Y` selector ("intersect a row"), but using
  the wrong key letter and with wrong scope.
- **Fix**: implement `r/R` as a registry-level range delete; route the
  current "clear row" code to the (still missing) `y/Y` selector.

### B8. Sending both `i=` and `I=` is silently accepted
- **Spec**: "Specifying both `i` and `I` keys in any command is an error.
  The terminal must reply with an EINVAL error message, unless silenced."
- **Location**: `crates/toastty-graphics/src/kitty/header.rs:289-290`;
  `crates/toastty-graphics/src/kitty/handler.rs:178-215`
- **Behavior**: the parser stores both values and `dispatch` proceeds. The
  reference C implementation emits EINVAL.
- **Fix**: in `dispatch`, reject `image_id != 0 && image_number != 0` with
  EINVAL (subject to the quiet level).

### B9. Malformed-header errors are swallowed instead of returning EINVAL
- **Spec**: malformed input should return EINVAL when the client wants a
  reply.
- **Location**: `crates/toastty-term/src/term.rs:3130-3134` (the
  `_ = handler.process(...)` discards `HandlerError::BadHeader`). An
  existing comment in the code already calls out this gap.
- **Behavior**: bad enum / bad int / unknown action all produce silence
  instead of an EINVAL reply.
- **Fix**: thread the error back into a reply path; if `i=`/`I=` were
  parseable, emit `EINVAL` echoing them.

### B10. Unicode placeholder image-id decoding is wrong
- **Spec**: image_id is composed from `(foreground color, 3rd diacritic)` —
  with indexed fg providing bits 0..8, RGB fg providing bits 0..24, and the
  3rd diacritic providing bits 24..32. The underline color encodes
  `placement_id`.
- **Location**: `crates/toastty-term/src/term.rs:3557-3575, 1781-1808`
- **Behavior**: `placeholder_image_id_from_sgr` treats the underline color
  as the *high byte of the image_id* (`(high << 8) | low`) and never reads
  the 3rd diacritic. Apps that use placement ids via underline color get
  the wrong image lookup; apps with image ids > 255 in indexed mode cannot
  address them.
- **Fix**: compute `image_id = (third_diacritic << 24) | fg_color_bits`;
  treat underline color as `placement_id`; plumb that placement_id into the
  emitted `Placement`.

### B11. Unicode placeholder diacritic inheritance from left neighbor is missing
- **Spec**: a cell with 0 diacritics inherits `(row, col+1, id_msb)` from
  the cell to its left if fg and underline match; 1 diacritic inherits
  `(col+1, id_msb)`; 2 diacritics inherit `id_msb`.
- **Location**: `crates/toastty-term/src/term.rs:1785-1808`
- **Behavior**: `finalize_placeholder_run` reads `diacritics.first()` as
  row and `.get(1)` as col with no left-neighbor inheritance. Multiplexer
  pass-through that emits diacritics only on cell boundaries paints every
  bare cell at source `(0, 0)`.
- **Fix**: track the previous cell's resolved `(row, col, id_msb)` when fg
  and underline match, and fill in missing components.

### B12. `U=1` virtual placement still emits a visible placement
- **Spec**: `U=1` creates a *virtual* placement — the image is registered
  but not displayed; subsequent `U+10EEEE` cells provide visible
  references.
- **Location**: `crates/toastty-graphics/src/kitty/handler.rs:429-447`
- **Behavior**: `a=T,U=1` still calls `place_image` and advances the
  cursor. Apps using the canonical `q=2,U=1` two-step (transmit virtual,
  then paint placeholders) get a duplicate non-virtual placement at the
  cursor.
- **Fix**: when `unicode_placeholder == 1`, skip the visible placement and
  skip cursor advance; just register the image.

---

## 2. Major

Wrong behavior or unimplemented spec features that spec-conformant clients
will exercise, though less common in everyday use than the blockers.

### M1. Cursor end position after `a=T` is wrong on both axes
- **Spec / reference**: after a placement that moves the cursor, the
  reference C impl does `c->x += cols; c->y += rows - 1;` — cursor lands on
  the *last row* of the image, one column past its right edge.
- **Location**: `crates/toastty-term/src/term.rs:3350-3370`
- **Behavior**: toastty advances `cursor.row` by `rows` (off by one) and
  sets `cursor.col = start_col` (drops `cols` advance). Text emitted right
  after an image lands on the wrong row and column.
- **Fix**: `cursor.row += rows.saturating_sub(1); cursor.col += cols;`.

### M2. Standalone `a=p` never moves the cursor regardless of `C=`
- **Spec**: `a=p` with `C=0` (default) should move the cursor by
  `(cols, rows-1)`.
- **Location**: `crates/toastty-graphics/src/kitty/handler.rs:454-472`
  (`handle_place` never calls the cursor-advance path)
- **Fix**: call the same `advance_cursor_after_placement` path used by
  `a=T`, gated on `!cursor_no_move && !unicode_placement`.

### M3. `X=` and `Y=` are pixel offsets within a cell, not cell offsets
- **Spec**: "`X` — The x-offset within the first cell at which to start
  displaying the image" (pixels, must be smaller than the cell). The cell
  position itself comes from the cursor.
- **Location**: `crates/toastty-graphics/src/kitty/header.rs:51-54,
  313-314`; `crates/toastty-graphics/src/kitty/handler.rs:569-572`;
  `crates/toastty-render/src/image/instance.rs:105-108`
- **Behavior**: stored as `cell_x` / `cell_y` (u32) and used to offset the
  starting *cell* of the placement. The doc comment "X cell offset on the
  grid" confirms the misunderstanding. Latent today because no observed
  client sets these to non-zero.
- **Fix**: rename to `pix_x_in_cell` / `pix_y_in_cell`; pass them to the
  renderer as a sub-cell pixel offset on `pos_x` / `pos_y`.

### M4. Aspect ratio is not preserved when only one of `c=` / `r=` is given
- **Spec**: when only `c=` is set, `r` should be derived from
  `cols * cell_pw : rows * cell_ph == img_w : img_h`.
- **Location**: `crates/toastty-graphics/src/kitty/handler.rs:541-560`
- **Behavior**: derives `rows = ceil(img_h / cell_ph)` (natural cell count)
  regardless of `cols`. Example — 200×100 image, 10×20 cell, `c=10` →
  toastty picks `r=5` instead of the aspect-correct `r=3`.
- **Fix**: when one axis is given and the other is 0, compute the missing
  axis from the source aspect ratio in pixel space.

### M5. Re-emitting `(image_id, placement_id)` accumulates instead of replacing
- **Spec**: "If you send two placements with the same image id and
  placement id the second one will replace the first."
- **Location**: `crates/toastty-term/src/term.rs:3189-3262` (`place_image`)
- **Behavior**: `image_grid.add(placement)` is unconditional. Animations
  and moving placements that re-emit the same `i=N,p=M` accumulate
  layers — memory growth plus z-fight artifacts on equal z.
- **Fix**: before `add`, `remove_where(|p| p.image_id == img &&
  p.placement_id == pid)` when both are non-zero.

### M6. `t=f` / `t=t` / `t=s` transmission mediums return ENOTSUP
- **Spec**: terminals claiming kitty graphics support must support direct,
  file, temp-file, and shared-memory transports. `kitten icat
  --transfer-mode=file` for large local images relies on this.
- **Location**: `crates/toastty-graphics/src/kitty/handler.rs:273-275`,
  `224-227`
- **Behavior**: the parser knows the mediums but the handler rejects
  anything other than `t=d` with ENOTSUP.
- **Fix**: implement `t=f` (read file path), `t=t` (read + delete), `t=s`
  (read POSIX shm and unlink).

### M7. RIS (`ESC c`) is a no-op
- **Spec**: "When resetting the terminal, all images that are visible on
  the screen must be cleared."
- **Location**: `crates/toastty-term/src/term.rs:2838-2877`
  (`esc_dispatch` has no `b'c'` arm)
- **Behavior**: `ESC c` doesn't reset cursor, SGR, modes, scrollback, or
  images.
- **Fix**: add a `b'c'` arm calling a new `Term::ris()` that resets all
  state and calls `image_grid.clear()`.

### M8. `CSI S` (SU) and `CSI T` (SD) unimplemented
- **Location**: `crates/toastty-term/src/term.rs` `handle_csi` (~1817+);
  `'S'` and `'T'` fall through to the unhandled-CSI warning.
- **Behavior**: explicit scroll commands move neither text nor images.
- **Fix**: add match arms calling `region_scroll_up`/`down` `n` times.

### M9. `Term::resize` doesn't touch placements
- **Location**: `crates/toastty-term/src/term.rs:1357-1385`
- **Behavior**: after shrinking, `Placement.row_range.end` can exceed the
  new `self.rows`, leaving phantom images past the bottom edge. Reference
  kitty clips on resize.
- **Fix**: walk `image_grid.placements`, clamp ranges to the new
  geometry, `remove_where` placements that collapse to empty.

### M10. `d=a` / `d=A` ignore the "visible on screen" qualifier
- **Spec**: `a/A` deletes "all placements visible on screen". Off-screen
  placements (alt screen, scrollback) should be untouched.
- **Location**: `crates/toastty-term/src/term.rs:3282-3289`
- **Behavior**: `image_grid.clear()` wipes everything. `d=A` further
  drains the entire `image_registry`, including images whose placements
  were not visible (or whose bytes were intentionally kept for re-use).
- **Fix**: scope deletion to placements whose row_range intersects
  `0..self.rows` on the active screen; for `d=A` only free bytes for
  images with no surviving placement anywhere.

### M11. `d=n` / `d=N` (delete by image number) unimplemented
- **Location**: `crates/toastty-term/src/term.rs:3300-3304` (the comment
  explicitly acknowledges no `I=` → id mapping exists)
- **Fix**: maintain an image-number → most-recent-id index in the
  registry; resolve `d=n,I=N` to the newest id with that number.

### M12. `d=c/C, q/Q, x/X, y/Y, z/Z` are silent no-ops
- **Location**: `crates/toastty-term/src/term.rs:3319-3321` (catch-all
  `_ => {}`). Note: the real `y/Y` "delete by row" is what toastty
  incorrectly wired to `r/R` (B7).
- **Fix**: implement each selector. Most need only a `remove_where`
  predicate over the existing `image_grid.placements`.

### M13. Relative placements (`P=`, `Q=`, `H=`, `V=`) not implemented
- **Spec**: child placements anchored to a parent by image+placement id at
  `(H, V)` cell offsets; cursor never moves; new error codes
  `ENOPARENT` / `ECYCLE` / `ETOODEEP`.
- **Location**: `crates/toastty-graphics/src/kitty/header.rs:72-73,
  365-366` (`P`/`Q` parsed and ignored; `H`/`V` not parsed at all and
  silently swallowed by the tolerant key parser).
- **Fix**: parse `H` / `V`; resolve parent on placement; reject cycles and
  excess depth with the appropriate error codes.

### M14. Placement id not plumbed through unicode placeholder pipeline
- **Location**: `crates/toastty-term/src/term.rs:1796-1808`
  (`finalize_placeholder_run` builds the placement with hardcoded
  `placement_id: 0`)
- **Behavior**: apps with multiple placements per image, addressed via
  underline color (the spec encoding), can only ever reach `p=0`.
- **Fix**: read the underline color from the run's anchor cell, decode it
  per spec, and pass it to the emitted `Placement`.

### M15. `IL` (`CSI L`) / `DL` (`CSI M`) don't shift images
- **Location**: `crates/toastty-term/src/term.rs:2310-2392`
- **Behavior**: cells shift, placements stay anchored to absolute rows.
  Spec doesn't explicitly say images move, but reference kitty does shift
  them.
- **Fix**: invoke `image_grid.shift_rows_{up,down}` on the affected
  range.

### M16. Quiet-level `q=2` reference behavior vs literal spec
- **Note**: the literal spec text says `q=2` suppresses *failures* (which
  would leave OK through). The reference kitty C code, and almost all
  client expectations, treat `q=2` as "silence everything". Toastty
  matches the reference. Recommendation: keep the current behavior; add a
  code comment citing the reference C code (mirror the existing block at
  `handler.rs:613-625` that documents `client_wants_reply`).

---

## 3. Minor

Lower-impact divergences and cosmetic issues.

### m1. `p=` (placement id) not echoed in OK/error replies
- **Spec example**: `ESC_Gi=<id>,p=<placement>;OK ESC\`
- **Location**: `crates/toastty-graphics/src/kitty/reply.rs:69-87`;
  `crates/toastty-graphics/src/kitty/handler.rs:471`
- **Fix**: include `p=` in the reply encoder when the client supplied a
  placement id.

### m2. Negative z sub-layer (below cell background) not honored
- **Spec**: z < INT32_MIN/2 renders under text *and* under cell
  background; INT32_MIN/2 ≤ z < 0 renders under text but above cell
  background.
- **Location**: `crates/toastty-render/src/image/instance.rs:138-145`;
  `crates/toastty-render/src/lib.rs:1518-1585`
- **Behavior**: only sign of z is checked; all negative z draws above the
  text background pass.
- **Fix**: split negative z into two layers in the renderer pipeline.

### m3. z-index tie-break uses insertion order, not image id
- **Spec**: "If two images with the same z-index overlap then the image
  with the lower id is considered to have the lower z-index."
- **Location**: `crates/toastty-render/src/image/instance.rs:130-132`
- **Fix**: stable sort key = `(z, image_id)`.

### m4. DECSET 47 / 1047 / 1048 (legacy alt-screen variants) unhandled
- **Location**: `crates/toastty-term/src/term.rs:2524-2603`
  (`apply_decset`)
- **Behavior**: only 1049 is honored; 47 / 1047 / 1048 hit the catch-all
  warning. Modern apps prefer 1049 so practical impact is low.

### m5. `ImageRegistry::touch()` is dead code; LRU is effectively LRI
- **Location**: `crates/toastty-graphics/src/registry.rs:117-123` —
  `touch()` exists and is tested in isolation, but no production code path
  calls it on placement creation or render.
- **Fix**: call `touch(id)` from `place_image` so the LRU reflects actual
  use.

### m6. Image-quota eviction can evict in-use images
- **Spec note**: "running out of quota space ... existing images without
  placements will be preferentially deleted."
- **Location**: `crates/toastty-graphics/src/registry.rs:162-175`
- **Behavior**: the LRU pops the front regardless of placement state.
- **Fix**: prefer to evict images with no live placements before
  evicting any image that has them.

### m7. Default image quota is 256 MiB; spec recommends 320 MB
- **Location**: `crates/toastty-term/src/term.rs:457-459`
- **Fix**: bump default to 320 MiB (or 320 MB, per spec wording).

### m8. Continuation-chunk validation is stricter than reference kitty
- **Spec**: continuation chunks "must have only the `m` and optionally `q`
  keys" — terminals should ignore extras, not validate them.
- **Location**: `crates/toastty-graphics/src/kitty/handler.rs:303-318`
  (`headers_continuation_compatible` compares `format`, `compression`,
  `action`, `source_width`, `source_height`)
- **Fix**: relax to identity match on `i=`/`I=`/`p=` only.

### m9. `d=f` / `d=F` (animation frame delete) falls through to OK
- **Location**: `crates/toastty-term/src/term.rs:3279-3322`
- **Behavior**: animation is ENOTSUP overall, but `d=f` hits the
  catch-all `_ => {}` and the handler then replies `;OK`. Either reply
  `ENOTSUP` or treat as a guaranteed-no-op without misleading OK.

### m10. Header field overload risk for animation
- **Note**: when animation lands, `c`, `r`, `X`, `Y`, `z`, `s`, `v` carry
  different types/meanings depending on `a=`. The current `Header` struct
  collapses each into one display-shaped field, so a future animation
  implementation will need a context-aware parser (or per-action header
  variants). Not a bug today (all animation actions return ENOTSUP), but
  worth flagging as a structural pitfall.
- **Location**: `crates/toastty-graphics/src/kitty/header.rs:51-62`

### m11. Test coverage gap for deletion semantics
- The only delete-related test
  (`crates/toastty-graphics/src/kitty/handler.rs:980-988`) just confirms
  the dispatch byte reaches the sink. No `term.rs`-level test exercises
  any `d=` variant against a real registry+grid pair.
- **Recommendation**: add round-trip tests covering at minimum
  `d=i,i=N,p=M` (B4), `d=I,i=N,p=M` bytes-retention (B5),
  `d=p,x=C,y=R` (B6), `d=r,x=A,y=B` (B7), and `d=a` / `d=A`
  visibility scope (M10). Without these, any rewrite of the `d=` table
  is high-risk.

---

## What is working well

So that the fix passes don't accidentally break compliant code:

- APC envelope parsing (8-bit ST, BEL terminators, split-buffer boundaries)
  — `crates/toastty-parser/src/parser.rs:42-115`
- Base64 padded/unpadded with whitespace tolerance —
  `crates/toastty-graphics/src/kitty/handler.rs:501-512`
- Zlib `o=z` decompression —
  `crates/toastty-graphics/src/kitty/handler.rs:514-520`
- RGB/RGBA/PNG decode including `f=24` alpha expansion —
  `crates/toastty-graphics/src/kitty/decode.rs`
- Chunked reassembly with `active_upload_id`, bare-`m=N` continuation,
  anonymous-upload routing —
  `crates/toastty-graphics/src/kitty/handler.rs:286-362`
- Server-allocated IDs (`I=` without `i=`) echoed correctly with both
  `i=` and `I=` — `crates/toastty-graphics/src/kitty/handler.rs:582-590`
- Anonymous-image reply suppression matches reference —
  `crates/toastty-graphics/src/kitty/handler.rs:613-625`
- CSI 14/16/18t window-size reporting —
  `crates/toastty-term/src/term.rs:2003-2017`
- Source rect `x,y,w,h` interpreted in source pixels —
  `crates/toastty-render/src/image/instance.rs:111-119`
- Right-edge truncation, auto-scroll when an image doesn't fit at the
  bottom — `crates/toastty-term/src/term.rs:3227-3257`
- Full 297-entry diacritic table —
  `crates/toastty-graphics/src/kitty/placeholder.rs:58-84`
- Coarse negative-vs-positive z ordering —
  `crates/toastty-render/src/lib.rs:1518-1585`
- Full-region LF / RI image scrolling —
  `crates/toastty-term/src/term.rs:1485, 1530`
- ED 0/1, EL, ICH/DCH/ECH leave images alone (per spec) —
  `crates/toastty-term/src/term.rs:2159-2303`
- Selection over images passes through correctly —
  `crates/toastty-term/src/selection.rs`
- LRU registry cleans up dependent placements on eviction —
  `crates/toastty-term/src/term.rs:3169-3175`
- Animation `a=f` / `a=a` / `a=c` correctly advertised as ENOTSUP so
  clients fall back — `crates/toastty-graphics/src/kitty/handler.rs:207-213`
