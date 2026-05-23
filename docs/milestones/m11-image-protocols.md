# M11 — Image protocols

**Goal.** Inline images. The first big graphics work after pure text.

**Scope.** Kitty graphics protocol as primary, Sixel as fallback.

**Kitty graphics protocol.** APC payloads (`\x1b_G...\x1b\\`) carry image data with key=value parameters: format (`f=`), action (`a=t` transmit, `a=p` place, `a=d` delete), size, placement coordinates, z-index, etc. The streaming APC scanner from decision #5 already handles arbitrary payload sizes; this milestone wires (a) the parameter parser, (b) the per-Term image registry that holds decoded textures, (c) renderer support for image cells that sample from the registry instead of the glyph atlas. Chunked uploads (`m=1` continuation chunks) are reassembled in the Kitty handler, not the parser.

Support the unicode placeholder extension at this stage too — used by tmux to pass images through; without it tmux strips kitty graphics.

**Sixel.** Older protocol, broader legacy reach. DCS-framed (`\x1bP...\x1b\\`). Decode hand-rolled or via `sixel-rs`; the output is a per-image bitmap that becomes a texture in the same image registry the kitty path uses. Renderer doesn't care which protocol filled the registry.

**Atlas eviction.** The "panic when full" policy from M4b stops being acceptable once images churn through the atlas. Add LRU eviction — `Atlas::reserve` returns `Err(::Full)` once we can't evict, and the rasterizer/image-uploader gracefully degrades (skip that cell rather than crash).

**Image cells.** Add an image-cell variant to `Cell` (or a parallel grid layer). Cell holds an image ID + sub-rect within the image. Renderer draws image instances after text instances in the cell pass.

**Out of scope.** iTerm2 inline images — explicitly deferred (decision matrix: Kitty + Sixel cover the same use cases). Animated images (kitty supports frame sequences; defer to a later pass).

## M11a — Kitty graphics shipped

Delivered the Kitty path end-to-end:

- `toastty-graphics::kitty` module with header parser, image decoder
  (PNG / raw RGB / raw RGBA), `KittyHandler` + `KittySink` trait,
  Unicode placeholder + diacritic table, and reply encoder for
  `OK` / `EINVAL` / `EBADF` / `ENOENT` / `ENOTSUP` / `EFBIG`.
- `ImageRegistry` (CPU-side, LRU-bounded by bytes, 256 MiB default cap)
  and `ImageGrid` (parallel placement layer over the cell grid).
- `Term::apc_start/chunk/end` dispatch into the Kitty handler with
  `Term` as the `KittySink`. Chunked uploads (`m=1` → `m=0`)
  reassemble correctly; `S=` size enforces against the budget.
- Render path: per-image GPU texture cache
  (`image::atlas::ImageTextureCache`, 14-slot LRU), single shader
  pipeline (`shaders/image.wgsl`), one draw per instance with
  per-draw texture rebind. Below-text (`z < 0`) and above-text
  (`z >= 0`) splits sit around the text pass; all draws target the
  M9 scratch texture.
- `Term::image_revision` bumps on every registry/grid mutation; the
  renderer detects the bump, syncs textures, and forces
  `needs_full_clear = true` for that frame (pragmatic shortcut —
  partial-redraw with images is a follow-up).
- Unicode placeholder support: SGR 58 stored on
  `cursor_underline_color`; the placeholder run state machine
  collects `U+10EEEE` + 0..3 diacritics and materializes `Placement`s
  whose `src_rect` slices the source image into a uniform tile
  grid.
- `text::atlas::Atlas::reserve` returns `Result<AtlasSlot, AtlasFull>`
  with LRU shelf invalidation. Glyph rasterizer degrades to `None`
  on full + best-effort retries after a shelf eviction.

Pre-approved trade-offs honored:
- Texture-per-image (not packed atlas).
- Parallel `ImageGrid` on Term (not a Cell field).
- `image` crate `0.25` with `default-features = false, features = ["png"]`.
- Separate image pipeline + shader.
- Image revision change → `needs_full_clear` (no partial redraw with
  images in M11a).
- `I=` image number accepted on transmit; subsequent ops require
  `i=`.
- Animated frames (`a=a`) reply `ENOTSUP`.

Known limitations carried into follow-ups:
- Sixel: not implemented.
- iTerm2 inline images: not implemented.
- Animation: not implemented.
- Per-image-number→id mapping for delete `n=N` is a no-op.
- Placeholder run finalize is best-effort tiling — a future pass can
  coalesce adjacent cells into a single placement.
