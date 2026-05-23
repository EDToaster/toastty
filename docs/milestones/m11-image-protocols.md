# M11 — Image protocols

**Goal.** Inline images. The first big graphics work after pure text.

**Scope.** Kitty graphics protocol as primary, Sixel as fallback.

**Kitty graphics protocol.** APC payloads (`\x1b_G...\x1b\\`) carry image data with key=value parameters: format (`f=`), action (`a=t` transmit, `a=p` place, `a=d` delete), size, placement coordinates, z-index, etc. The streaming APC scanner from decision #5 already handles arbitrary payload sizes; this milestone wires (a) the parameter parser, (b) the per-Term image registry that holds decoded textures, (c) renderer support for image cells that sample from the registry instead of the glyph atlas. Chunked uploads (`m=1` continuation chunks) are reassembled in the Kitty handler, not the parser.

Support the unicode placeholder extension at this stage too — used by tmux to pass images through; without it tmux strips kitty graphics.

**Sixel.** Older protocol, broader legacy reach. DCS-framed (`\x1bP...\x1b\\`). Decode hand-rolled or via `sixel-rs`; the output is a per-image bitmap that becomes a texture in the same image registry the kitty path uses. Renderer doesn't care which protocol filled the registry.

**Atlas eviction.** The "panic when full" policy from M4b stops being acceptable once images churn through the atlas. Add LRU eviction — `Atlas::reserve` returns `Err(::Full)` once we can't evict, and the rasterizer/image-uploader gracefully degrades (skip that cell rather than crash).

**Image cells.** Add an image-cell variant to `Cell` (or a parallel grid layer). Cell holds an image ID + sub-rect within the image. Renderer draws image instances after text instances in the cell pass.

**Out of scope.** iTerm2 inline images — explicitly deferred (decision matrix: Kitty + Sixel cover the same use cases). Animated images (kitty supports frame sequences; defer to a later pass).
