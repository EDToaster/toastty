# Markdown rich blocks

Status: in progress (v1). This is the design for rendering Markdown documents,
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
| Fenced ```` ```mermaid ```` diagrams (mermaid-rs-renderer) | yes |
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
- **Through tmux:** the helper wraps the sequence in tmux passthrough
  (`DCS tmux; <ESC doubled> ST`) when `$TMUX` is set. toastty unwraps it.
  tmux needs `set -g allow-passthrough on`.
- **Detection:** the helper only emits the sequence when `TERM_PROGRAM=toastty`.
  Otherwise it prints the raw Markdown.

## Architecture

```
PTY bytes ─▶ toastty-parser ─▶ Term::osc_dispatch(7700)
                                   │  decode, security gate
                                   ▼
                     RichBlockLayout (trait, injected by the binary)
                                   │  toastty-markdown::Renderer::render
                                   ▼
                RenderedBlock { rows, image, text per row, links }
                                   │
     Term writes `rows` grid rows: plain text into the cells, links as
     hyperlink ids, and a per-row tag `RichRow { block, index }`.
     The image goes into a RichBlockStore (not the kitty ImageRegistry).
                                   │
Renderer: for each visible row with a RichRow tag, draw the matching
cell_h-tall slice of the block's image instead of the row's text glyphs.
If the image is gone, the plain text in the cells is drawn as normal.
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
  - Mermaid: `mermaid-rs-renderer` produces SVG, which resvg rasterizes.
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
  - the latest `RenderedBlock` image.

  Blocks with no tagged rows left in the grid or scrollback are freed. The
  store has a byte budget like the image registry; evicted images fall back to
  drawing their text.
- **Theme and zoom:** a palette change (config reload, OSC 4/10/11) or a
  change of font size or DPI renders the visible blocks again. The row count
  is kept when only colours change.

## Security

- The block arrives over the PTY and may come from any program, including one
  on a remote host. Rendering is gated by `[security] markdown = true`, which
  is on by default; when it's off, the sequence is ignored.
- Local image files are read only when `[security] markdown_local_files` is
  on (off by default). Remote URLs are never fetched.
- **Resource limits:**
  - Source: the OSC cap, 8 MiB.
  - Rows: `max_rows`, 2,000 by default. Past that the block is cut and
    marked as truncated.
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
- **End to end:** GIFs recorded with real `toastty md` runs, including inside
  tmux.
