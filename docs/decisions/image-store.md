# Decision: Per-Screen Image Store

**Date:** 2026-10-21
**Status:** Accepted (kitty graphics fix programme, PR A5)
**Slug:** `image-store`

## Problem

Until now one `ImageRegistry` held every image, keyed by the client's id
(`i=`), for both screens. The id-to-number map lived beside it in `Term`.
That caused several bugs (checklist items PLC-2, PLC-4, PLC-6 and the #27
review):

- **Deletes crossed screens.** `d=I` (and `d=F`, `d=N`, `d=R`) on the alt
  screen freed images that only the main screen used. A full-screen
  previewer deleting its own ids could wipe the shell's image.
- **Same-id replacement crossed screens.** Transmitting `i=7` on one
  screen replaced the bytes behind the other screen's `i=7` placements.
- **Quota eviction crossed screens.** Eviction only protected the active
  screen's placements, so it could drop the bytes behind the stashed
  screen's placements.
- **Re-transmitting an id kept its placements (PLC-2),** which then showed
  the new pixels. Kitty deletes them.
- **Anonymous images had addressable ids (PLC-4).** They took the lowest
  free id, so another program's `a=t,i=1` or `a=d,d=I,i=1..` overwrote or
  deleted them.

## Model

kitty gives each screen its own graphics manager: its own images, ids,
numbers and storage quota. toastty now mirrors that with
`toastty_graphics::ImageStore`, one per screen. A screen's store and its
placements (`ImageGrid`) live together in `term::ScreenImages`:

- **`Term::images`** is the active screen's `ScreenImages`.
- **`Term::stashed_images`** is the inactive screen's.
  - `Term::swap_screen_images` swaps the two as one value. That is the
    only place screen switches touch images, and per-screen state added
    later (#28's virtual placements) goes in the same struct, so it can't
    be forgotten in the swap.
  - Every id-, number- and quota-related operation goes through the
    active store, so it can't reach the other screen.

Each store holds:

| | |
|---|---|
| `registry: ImageRegistry<ImageKey>` | Decoded bytes keyed by **key**. It owns the LRU and the byte quota. |
| `by_client` / `client_of` | Map between client id (`i=`) and key, in both directions. Anonymous images have client id `0` and are absent from `by_client`. |
| `numbers` | Map from image number (`I=`) to the client ids of the live images transmitted with that number, oldest first. The newest live one answers; once it is freed the next newest does, and an image re-sent by `i=` keeps its number (#35, #37; matches kitty 0.49.2). |

### Client ids and keys

- **The client id** is what programs address: `i=` keys, `d=i/I/r/R`,
  `a=p`, `a=q`, and parent references (`P=`). `Placement::image_id` holds
  it, and deletes, parents and the equal-`z` tie-break use it.
- **The key** (`toastty_graphics::ImageKey`, a `u32` newtype) is where
  the bytes live.
  - It is a distinct type so that a client id can't be used to look up
    bytes or textures: that would compile and silently draw another
    image. The registry (`ImageRegistry<ImageKey>`), `Placement::key`,
    the store's maps and the renderer's `ImageTextureCache` all take
    `ImageKey`. Code written against the old `u32` keys fails to compile
    instead of misbehaving.
  - `Term::alloc_image_key` hands keys out from one counter shared by both
    screens. A key is never reused while it is still stored.
  - Every transmission gets a fresh key, including a re-transmission under
    the same id.
  - `Placement::key` and the renderer (registry lookup, texture cache) use
    keys. So the GPU can never confuse a re-sent id, or the other screen's
    image with the same id, with stale content.
  - `Placement::new` leaves `key` as `ImageKey::NONE`, which is never
    stored, so a placement nobody resolved draws nothing.
    `Term::place_image` / `place_relative` (and the placeholder path) set
    the real key.

### Rules (verified on kitty-ref 0.49.2)

- **Re-transmission (PLC-2).** A transmission with an `i=` that is already
  in use deletes the old image *and all its placements*. The new data
  appears only once it is placed.
  - `a=T,i=1` red then `a=t,i=1` blue shows nothing.
  - `a=T,i=1` red then `a=T,i=1` blue elsewhere shows one blue image.
  - `ImageStore::insert` reports the old key as `Stored::replaced`, and
    `register_image` removes its placements.
- **Anonymous images (PLC-4, PLC-8).** With no `i=` and no `I=`, the
  image has no client id at all.
  - It survives `a=d,d=R,x=1,y=4294967295`, `a=t,i=1` and `a=d,d=I,i=1`.
  - Positional deletes (`d=a/A/p/P/…`) still remove it, and uppercase
    ones free its bytes, because they work by key.
  - The handler gets client id `0` back from `register_image`, so it
    doesn't reply. `place_image` uses the key it just stored
    (`last_registered_key`).
  - Nothing can place it again, so its bytes go when its last placement
    does, by whatever path (a lowercase delete, scrolling out of the
    history, a clear), as in kitty. Sixel images are anonymous too.
- **Clears (PLC-8).**
  - `ESC[2J` removes the on-screen placements and then frees every image
    left with no placement, including ones that were never placed.
    Placements in the history (and their images) survive. kitty-ref:
    after `ESC[2J`, `a=p` on a placed, a never-placed and a re-placed id
    all get ENOENT, `I=6` is handed `i=1` again, and an image whose
    placement had scrolled into the history still answers `a=p` with OK.
  - `ESC[3J` removes every placement and so frees every image (kitty-ref:
    ENOENT for both a placed and a never-placed id).
  - RIS (`ESC c`) frees the whole store: `a=p,i=1` gets ENOENT after it.
- **`I=` without `i=`.** kitty assigns the lowest unused client id
  (`get_free_client_id`). Measured: `I=5` → `i=1`, `I=6` → `i=2`, and
  after an explicit `i=1` replaced the first, `I=7` → `i=3`.
- **Quota.** Each screen has its own quota, as kitty gives each graphics
  manager its own storage limit. The default is 320 MiB per screen.
  `Term::set_image_cap` sets both.
  - Eviction prefers images without placements *on that screen*. The
    other screen's store is never consulted, because it is never evicted
    from here.
  - Each image costs its pixel bytes plus `IMAGE_OVERHEAD_BYTES` (256)
    against the quota. A quota that counted only pixels would let a
    client store millions of 1×1 images, each of which really costs a few
    hundred bytes of map entries and bookkeeping. With the overhead the
    quota bounds the image count too (at most ~1.3 M per screen at
    320 MiB, all of them 1×1).
- **Screen switches.**
  - Entering 1049 stashes the primary's grid and store and gives the alt
    screen empty ones.
  - Leaving restores the primary's and drops the alt screen's.
  - The renderer's texture cache mirrors the *active* registry, so a
    switch re-uploads the textures of the screen being entered. That is
    cheap for the usual few images; #24 can keep both screens' textures
    resident, since keys never collide across screens.
  - #27 (47/1047 keep the alt screen) only needs `swap_screen_images(false)`
    on entry, so the alt store comes back with the alt grid.

## Where other work hooks in

`ImageStore` is meant to be the single place for per-image state, and
`ScreenImages` for per-screen state:

- **Removing placements.** Every path that removes placements from the
  active grid (deletes, scrolling out of the history, margin scrolls,
  ED, re-transmission, eviction, a cap change) ends in
  `Term::drop_placements`, which calls `ScreenImages::release`. The
  stashed screen's removals (reflow, cap change) call `release` directly.
  That is where a removal's consequences go: freeing anonymous and
  (for uppercase deletes) orphaned bytes today, REL-2's child cascade
  (#40) and #28's virtual-placement cleanup next, so they reach every
  path, re-transmission and eviction included.
- **#24 (texture cache, generation counter).** Key the GPU textures on
  `Placement::key`. Keys are already unique per transmission across both
  screens, so the registry's per-insert generation becomes redundant.
  - If it is kept, it must not be per-registry: each screen's store has
    its own registry, and their counters would collide.
- **#28 (Unicode placeholders, virtual placements).**
  - `VirtualPlacements` belongs in `ImageStore`, so it moves with the
    screen.
  - "Re-send drops virtual placements" hooks on `Stored::replaced`.
  - Eviction cleanup hooks on `Stored::evicted` and `ImageStore::set_cap`'s
    return.
  - Protection from eviction means adding the virtual placements' keys to
    the `pinned` set passed to `ImageStore::insert`.
  - Render-time placeholder lookups go client id → `ImageStore::key_of` →
    `registry().get(key)`.
- **#35 (`d=F` frames, number → ids list).**
  - The number map is `ImageStore::numbers`, a list of ids per number
    (done in the merge train).
  - `Term::delete_image_frames` must work on `self.image_store` only, which
    makes `d=F` on alt unable to free main's images.
- **#37 (replies).** `register_image` returns the client id, which is `0`
  for anonymous images (no reply). `ImageStore::lowest_free_client_id`
  is public, for `next_image_id`.
- **E1 (animation frames).** Frames belong to the stored image, so store
  them per key in `ImageStore`, next to the registry entry. They then
  follow the same replace, evict and screen rules.

## PLC-7

PLC-7 was "Intermittent: an image was missing after re-sending the same
id." I couldn't reproduce it.

- Live in toastty on Xvfb, 20 same-id `a=T` re-sends at the same cell in
  each of two runs: alternating red and blue, then identical red.
- The screen was grabbed 0.25 s after each send. All 40 showed the right
  colour, both on this PR's build and on #39's (before the store change).

With A5 every re-send gets a fresh key, so the renderer sees new content
under a new texture key, and nothing keyed on the id can go stale.
