# Slide shows: `toastty present`

`toastty present talk.md` shows a Markdown file as a deck of full-screen
slides, each rendered by the Markdown block renderer (see
[markdown.md](markdown.md)): headings, tables, highlighted code, maths and
Mermaid diagrams all work. Try `toastty present examples/present/demo.md`.

## Writing a deck

- Slides are separated by a line holding only `---` (up to three spaces of
  indent), as in Marp and reveal.js. `---` lines inside fenced code blocks
  don't count. Use `***` or `___` for a horizontal rule on a slide.
- Because `---` always splits, a setext heading (`Title` over `---`) isn't
  available; use `## Title`.
- A front-matter block at the top (`---`, YAML-looking lines, `---`) is
  skipped, so Marp decks open as they are.
- HTML comments (`<!-- … -->`) outside code are removed. Use them for
  speaker notes.
- Slides that are empty after that are dropped.
- Relative image paths resolve against the deck's directory when
  `[security] markdown_local_files` is on.

## Keys

| Key | Action |
| --- | --- |
| Space, →, ↓, l, j, n, PgDn, Enter | next slide |
| ←, ↑, h, k, p, PgUp, Backspace | previous slide |
| g, Home | first slide |
| G, End | last slide |
| digits, then Enter | go to that slide |
| r | reload the file |
| q, Esc, Ctrl+C | quit |

The deck also reloads when the file changes on disk, so you can edit a
slide in another pane and watch it update. The current slide is redrawn
when the window is resized.

## How it works

`present` is a plain client of the terminal: it only writes escape
sequences to its tty and reads keys and replies back. It doesn't reach into
toastty's internals, so the same binary works over ssh.

1. On start it pushes the visible screen into the scrollback (a screenful of
   newlines from the last row), hides the cursor and puts the tty in raw mode.
2. Each slide is drawn on the **primary** screen, since OSC 7700 blocks are
   ignored on the alternate screen. Inside synchronized output
   (`CSI ? 2026 h` … `l`), it erases the display, sends the slide as one
   `OSC 7700 ; md ; cwd=… ; <base64> ST` block, then asks for the cursor
   position (`CSI 6 n`). The block leaves the cursor on the row below it, so
   the reply gives the block's height in rows.
3. If the slide fits above the last row, it erases again, moves the cursor
   down to centre the block vertically, sends it again, and writes the page
   counter (`3 / 12`, dimmed) on the last row. Only this final frame is
   shown. A slide taller than the window is left drawn from the top (it
   scrolls, and the top is in the scrollback) and gets no counter, since
   writing on its rows would overwrite the block.
4. On quit it clears the screen, shows the cursor and restores the tty mode.

Outside toastty, and inside tmux / screen / zellij / herdr, the slides are
shown as plain Markdown text (the `toastty md` fallback), top-aligned.

## Limits

- Slide text size is the terminal's font size. Zoom with toastty's font
  zoom keys; the slide is laid out again for the new grid.
- Rich-block width is the full window width; there's no per-slide layout
  or theme control.
- Each slide is one block, so the 8 MiB OSC cap and 2,000-row block limit
  apply per slide.
