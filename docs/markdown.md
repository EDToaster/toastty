# Markdown rich blocks

Status: v1 implemented. This is the design for rendering Markdown documents,
including maths and Mermaid diagrams, inline in the terminal.

## User-facing behaviour

- `toastty md README.md` (or `cat notes.md | toastty md`) prints the document as
  a rendered block at the cursor, like `cat` but formatted. The shell prompt
  comes back below it.
- The block is part of the scrollback. It scrolls, and it is kept in history,
  like any other output.
- When the window width changes, the block is laid out again for the new
  width. It can get taller or shorter.
- Selecting text inside the block copies the rendered text. Hovering the block
  shows a small "Copy Markdown" chip that copies the original source.
- Links are clickable with Ctrl+click (Cmd+click on macOS), like OSC 8 links.
- Anything the renderer can't handle falls back to plain text. A maths
  expression that fails to parse shows as inline code, and a broken Mermaid
  diagram shows as its source in a code block.

### v1 scope

| Element | v1 |
| --- | --- |
| Headings (ATX and setext), with real sizes | yes |
| Bold, italic, strikethrough, inline code | yes |
| Ordered, unordered and nested lists; task lists | yes |
| Block quotes (nested) | yes |
| Fenced and indented code, with syntax highlighting (syntect) | yes |
| Tables (GFM), with column alignment | yes |
| Links (inline, reference, autolinks), clickable | yes |
| Horizontal rules | yes |
| Images: `data:` URIs | yes |
| Images: local files | opt in (`[security] markdown_local_files`) |
| Images: remote URLs | never fetched (alt text is shown) |
| Inline maths `$…$` and display maths `$$…$$` (RaTeX, KaTeX-compatible) | yes |
| Fenced ```` ```math ```` blocks | yes |
| Fenced ```` ```mermaid ```` diagrams (mermaid-svg) | yes |
| Raw HTML | ignored (shown as text) |
| Footnotes | rendered as text |

## Protocol

```
OSC 7700 ; md ; <keys> ; <base64 payload> ST
```

- `7700` is toastty's private OSC number for rich blocks. The second field
  names the block type; only `md` exists in v1.
- `<keys>` is a `:`-separated list of `key=value` pairs and may be empty:
  - `cwd=<base64 path>`: the directory that relative image paths resolve
    against. Used only when local files are allowed.
  - Unknown keys are ignored, so later versions can add keys.
- The payload is the UTF-8 Markdown, standard base64 encoded, with or without
  padding. Invalid base64 or invalid UTF-8 drops the sequence.
- Either BEL or ST can terminate the sequence. The existing 8 MiB OSC cap
  applies, so the source can be up to about 6 MiB.
- The block is placed at column 0 of the cursor's row. If the cursor isn't at
  column 0, a newline is inserted first. Afterwards the cursor sits at column 0
  of the row below the block.
- In the alternate screen the sequence is ignored, because full-screen apps own
  that screen.
- **Multiplexers:** inside tmux, herdr, screen or zellij (`$TMUX`,
  `$HERDR_PANE_ID` / `$HERDR_ENV`, `$STY`, `$ZELLIJ`) the helper doesn't try
  passthrough. It prints the raw Markdown, like the non-toastty fallback, and,
  only when stderr is a terminal, a one-line note on stderr ("toastty md: rich
  rendering isn't available inside tmux yet; showing plain text"). Multiplexer
  support is a follow-up, probably via kitty-style Unicode placeholders so the
  multiplexer owns the cells. toastty does not unwrap `DCS tmux;` passthrough.
- **Detection:** the helper only emits the sequence when `TERM_PROGRAM=toastty`.
  Otherwise it prints the raw Markdown. Documents too large for the OSC cap
  are printed as plain text as well.
- **`cwd=`:** the helper sends the file's directory (the current directory for
  stdin), so `![](img.png)` next to the file works when local files are
  allowed.

## Architecture

```
PTY bytes ─▶ toastty-parser ─▶ Term::osc_dispatch(7700)
                                   │  decode, security gate
                                   ▼
                     RichBlockLayout (trait, injected by the binary)
                                   │  toastty-markdown::Renderer::layout
                                   ▼
          BlockLayout { rows, text per row, links, display list }
            (the image is a painter: nothing is rasterized yet)
                                   │
     Term writes `rows` grid rows: plain text into the cells, links as
     hyperlink ids, and a per-row tag `RichRow { block, index }`.
     The image goes into a RichBlockStore (not the kitty ImageRegistry).
                                   │
Renderer: for each visible row with a RichRow tag, paint (once) the
16-row tile holding it, upload it, and draw the row's cell_h-tall slice
instead of the row's text glyphs. If the image is gone, the plain text
in the cells is drawn as normal.
```

- **`toastty-markdown`** (new crate) is pure CPU and knows nothing about the
  terminal: Markdown → `RenderedBlock`.
  - Parsing: `pulldown-cmark` with GFM tables, task lists, strikethrough and
    maths enabled.
  - Text shaping: cosmic-text, with a proportional body font and the
    terminal's monospace font for code.
  - Drawing: tiny-skia.
  - Code highlighting: syntect, coloured from the terminal's ANSI palette.
  - Maths: RaTeX (`ratex-parser` / `ratex-layout`, rasterized with the KaTeX
    fonts).
  - Mermaid: `mermaid-svg` produces SVG, which resvg rasterizes.
  - Text: the layout reports, for every row, the plain text runs that sit on
    it, mapped to columns by x position. The terminal writes them into the
    cells.
- **Rows tagged in the grid, not placements:** kitty image placements live
  only in the visible grid and are dropped when they scroll off. A rich block
  must survive in scrollback and through reflow, so each `Row` carries an
  `Option<RichRow>` tag (block id plus the index of the row within the block).
  Every grid operation that moves rows (scrolling, scrollback, erase, reflow)
  then carries the tag along. Erasing a row or overwriting its cells clears the
  tag, so an app that draws over a block wins.
- **Reflow:** when the column count changes, `Grid::resize_reflow` treats each
  contiguous run of rows with the same block as one unit. It asks the layout
  callback to render the block again at the new width and replaces the run
  with the new rows. Runs whose rows have left the scrollback are just dropped.
- **Storage:** `RichBlockStore` keeps, per block id:
  - the source;
  - the parameters it was rendered with;
  - the latest layout's painter (`RichImage`).

  Blocks with no tagged rows left in the grid or scrollback are freed. The
  store has a byte budget like the image registry (now counting layouts, not
  pixels); evicted images fall back to drawing their text.
- **Theme and zoom:** a palette change (config reload, OSC 4/10/11) or a
  change of font size or DPI renders the visible blocks again. The row count
  is kept when only colours change.

### Resize performance

A window resize re-lays out every block, on screen or not, at every step of
the drag, so layout has to cost about as much as text reflow. Two changes
make it so:

- **Layout and painting are separate.** `Renderer::layout` produces the
  rows, text, links and a display list; `Renderer::paint(layout, y0, h)`
  rasterizes any band of it. The terminal's `RichImage` is a lazy painter,
  and the GPU cache paints only the 16-row tiles holding visible rows. An
  offscreen block costs no pixels at all; an on-screen one costs a screenful.
  Bands are bit-exact with a full paint (vector ops are drawn in block
  coordinates and translated exactly, with an 8 px margin cropped off, and
  images are pre-scaled and blitted at whole pixels), except for 1 px
  hairlines, which may move by a few levels; `tests/lazy_paint.rs` checks this.
- **Width-independent work is cached** in the renderer across layouts:
  shaped paragraphs (keyed by a 128-bit hash of their content), highlighted
  code, maths rasters, decoded images, and Mermaid diagrams. Mermaid is split
  into `prepare_mermaid` (parse, lay out, SVG tree: once) and a rasterization
  at the fitted width that happens only when a tile showing it is painted,
  and is reused by the other tiles.

Release build, demo document (376 rows), sweeping 120 → 40 columns
(`cargo run --release -p toastty-markdown --example resize_bench -- doc.md`):
layout 0.8 ms per width (was 54 ms for layout plus full paint), plus about
2.4 ms to paint one screen of 50 rows.

### Implementation notes (v1)

- The trait is `toastty_term::RichBlockLayout`; the binary installs
  `toastty::markdown::MarkdownLayout`, which builds the
  `toastty_markdown::Renderer` (and its font database) on the first block.
  `toastty-term` has no dependency on `toastty-markdown`.
- Layout parameters: `cols` = grid width, the renderer's integral cell size,
  `font_px` = font size × display scale (including zoom), `mono_family` =
  the terminal's `font.family`, the theme palette in sRGB with OSC 4
  overrides for colours 0–15, `max_rows` = 2000 (`RICH_MAX_ROWS`).
- Code blocks (fenced and indented) sit on the terminal's column grid,
  so selecting them highlights exactly the drawn glyphs. They are shaped
  in `mono_family` (the engine's default monospace face when its font
  database lacks that family), and every cluster advances exactly
  `cell_w` per cell the terminal gives its characters (1 or 2, 0 for
  zero-width marks), like the terminal's own cluster-width snap. The text
  starts on the cell boundary nearest the panel's padding, inside list
  items and quotes too, and long lines wrap at a whole number of cells.
  Inline code in paragraphs keeps its natural advances. A font family
  change re-lays blocks out like a font size change does.
- Re-layout is deferred: geometry and palette changes mark blocks dirty and
  the binary calls `Term::sync_rich_blocks()` once per frame, which re-lays
  out (geometry change) or re-colours (palette only, keeping the row count:
  the new image is cropped or padded to the old height) and frees unreferenced
  blocks. Reflow on a column change re-lays blocks out synchronously inside
  `Grid::resize_reflow_with`.
- A block that an app partly overwrote is frozen: its remaining tagged rows
  keep their slices but are no longer re-laid out.
- The store budget is 256 MiB (`RICH_DEFAULT_BUDGET`), least recently drawn
  first; evicted blocks draw their text. On the GPU, blocks live as tiles of
  16 cell rows (fewer if the device's max texture size demands), painted and
  uploaded only when one of their rows is visible; up to 64 tiles that are no
  longer visible are kept. A block wider than the max texture size falls back
  to text.
- Selected cells over a block get a translucent selection-colour tint drawn
  over the image. OSC 8 underlines are not drawn on block rows (the image
  styles its links); the Ctrl/Cmd hover underline still is.
- Frames that show block rows are always full redraws (the slices are
  alpha-blended over the cell backgrounds).
- "Copy Markdown" is a round chip with a copy icon (the scroll-to-bottom
  chip's SDF pipeline), at the top right of the hovered block, pinned to the
  top of the window when the block starts above it. Clicking copies the
  source to the clipboard; the click never reaches the app.

## Security

- The block arrives over the PTY and may come from any program, including one
  on a remote host. Rendering is gated by `[security] markdown = true`, which
  is on by default; when it's off, the sequence is ignored.
- Local image files are read only when `[security] markdown_local_files` is
  on (off by default). Remote URLs are never fetched.
- **Resource limits:**
  - Source: the OSC cap, 8 MiB.
  - Rows: `max_rows`, 2,000 by default. Past that the block is cut and
    marked as truncated. This is the only size cap on a block: it is
    painted lazily, one visible 16-row tile at a time, so its full
    pixel size is never held in memory.
  - Decoded images: 64 MiB.
  - Mermaid diagrams: 4,096 × 8,192 px, scaled down to fit.
  - Maths expressions: 4 KiB of source and 256 levels of nesting. Earlier
    RaTeX versions were open to a stack overflow (CVE-2026-53531); we
    require ≥ 0.1.11 and apply our own limit as well.
- Rendering runs on the thread that parses PTY output. Large documents are
  bounded by the limits above.

## Tests

- **`toastty-markdown`:** unit tests for each element and golden PNG snapshots
  rendered with bundled fonts only (never system fonts), so CI on Linux and
  macOS matches. The text and link outputs are checked separately from the
  pixels.
- **Term:** unit tests cover the protocol, row tagging, scrolling into
  scrollback, reflow re-layout, overwrite clearing, the alternate screen, the
  security gates and copy.
- **End to end:** GIFs recorded with real `toastty md` runs. Inside tmux the
  helper's plain-text fallback is checked instead.
