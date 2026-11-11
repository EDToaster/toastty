# Experimental smooth scrolling in alternate-screen TUIs

Toastty can keep a TUI's logical screen on its existing cell grid while drawing
selected rectangles at fractional cell offsets. This is an opt-in experiment,
not a new terminal standard and not automatic smoothing for unmodified apps.

The implementation uses the existing experimental `APC smooth` vocabulary from
[Ghostty Smooth protocol 3](https://github.com/tlpinney/ghostty/blob/smooth/SMOOTH.md).
That fork explicitly describes the protocol as an experiment, not an upstream
Ghostty proposal. Its [pixelscroll.nvim client](https://github.com/tlpinney/pixelscroll.nvim)
already demonstrates the intended split: the terminal owns pixel movement,
while the app owns content and acknowledges whole-row changes.

## Why not just use scrolling margins?

The alternatives solve different problems:

| Approach | What it provides | Why it is not sufficient on its own |
| --- | --- | --- |
| DECSTBM + DECSLRM + DECSCLM | Existing scroll boundaries and a smooth/jump output mode ([xterm controls](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html), [DEC manual](https://vt100.net/docs/vt510-rm/DECSCLM.html)). | Good for animating semantic scroll operations. A TUI that redraws changed cells instead of emitting SU/SD supplies no scroll intent, and these controls do not carry fractional trackpad displacement. |
| SGR pixel mouse mode 1016 | Pointer positions expressed in pixels, with SGR mouse-event framing ([xterm controls](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html)). | The documented wheel representation is still buttons 64/65, not a continuous delta. Pixel coordinates are not pixel scrolling. |
| Detect translated rows | Could work without app changes. | Ambiguous for repeated lines, blank cells, folds, independently changing status columns, and partial redraws. Cannot establish the app's content bounds. |
| Send arbitrary pixel deltas to apps | Lets apps interpret precision gestures themselves. | Requires a new input protocol and clients, and still needs renderer offsets, edge content, and a transaction boundary. |
| App-declared regions + acknowledgements | Gives the terminal bounds, scroll position, spare content and intent. | Requires app integration, an overscan row, and explicit handling of latency. This is the selected experiment. |

DECSSCLS also exists, but controls historical line-output rates rather than a
direct manipulation gesture ([DEC scroll-speed documentation](https://vt100.net/docs/vt510-rm/DECSSCLS.html)).
This change does not repurpose or implement DECSCLM/DECSSCLS.

## Wire contract

All examples use seven-bit `ESC` framing. `ST` is `ESC \`; numbers are unsigned
decimal ASCII, without signs, whitespace or empty fields.

```text
ESC _ smooth;? ST
    query -> ESC _ smooth;ok;3 ST

ESC _ smooth;id;top;bottom;left;right;pos;max ST
    declare/update a vertical scroll region

ESC _ smooth;id;top;bottom;left;right;pos;max;ox;oy ST
    declare/update with an application-controlled fractional shift

ESC _ smooth;top;bottom;pos;max ST
    legacy declaration: id 0, full terminal width

ESC _ smooth;off;id ST
    remove one region

ESC _ smooth;off ST
    remove all regions
```

- Bounds are zero-based, inclusive, viewport-relative cells.
- `bottom` is a spare content row. It is hidden at rest and supplies pixels as
  the region moves up. The visible height is `bottom - top`, not `+ 1`.
- The extended form also makes `right` a spare column, even when `ox` is zero.
  Its visible width is `right - left`.
- `pos` is the app's first displayed content-row position; `max` is the largest
  allowed position. One wheel event must move exactly one displayed row.
- `ox` and `oy` are 0–999 thousandths of a cell, moving content left/up.
  The vertical app shift adds to the terminal's trackpad offset, capped at 0.999.
- `max=0` is useful for app-driven movement only. It does not capture precision
  wheel input, so a map can use wheel events for zoom.
- Eight non-overlapping rectangles are allowed. The hidden row/column count
  toward overlap checks. An invalid, overlapping, ninth, or oversized declaration
  is ignored atomically without replacing a valid region.
- In Toastty, declarations are accepted only on the alternate screen. Queries
  work on either screen. Ordinary primary-screen scrollback is unchanged.

### Application integration

1. Enter raw input mode and the alternate screen. Query support with a bounded
   timeout before reserving extra cells; unknown terminals silently ignoring APC
   is not enough if the client has already hidden a content row.
2. Render the visible rows plus the next row into the declared rectangle.
   Enable SGR mouse reporting (`CSI ? 1000 ; 1006 h`) for terminal-owned scrolling.
3. Set the app's wheel action to one displayed row per event.
4. Put the text update and its declaration in the same synchronized-output
   transaction (`CSI ? 2026 h` … `CSI ? 2026 l`). A declaration acknowledges
   the content painted in that transaction, not a future redraw.
5. Re-declare after keyboard jumps, content-bound changes and resize. The terminal
   removes regions on resize, cell-size changes, screen switches, RIS and DECSTR.
6. Send `smooth;off` and restore mouse/screen modes on exit, including exception
   and signal cleanup.

Example: a pane with 18 visible rows and 38 visible columns, plus bottom/right
overscan, shifted half a row upwards:

```python
sys.stdout.write(
    "\x1b[?2026h"
    + render_cells()  # rows 2..20 and columns 1..39, inclusive
    + "\x1b_smooth;1;2;20;1;39;10;500;0;500\x1b\\"
    + "\x1b[?2026l"
)
sys.stdout.flush()
```

The bundled demo is a stdlib-only client that deliberately forces declarations
without probing, so the same bytes can be compared on an unsupported baseline:

```sh
python3 scripts/smooth-scroll-demo.py
python3 scripts/smooth-scroll-demo.py --auto
python3 scripts/smooth-scroll-demo.py --hold 10.5
```

Use the wheel or `j`/`k` to scroll, Space to animate, `s` to toggle declarations
and `q` to exit. Automatic mode sends application-driven fractional shifts,
which makes renderer behavior reproducible even on an X11 virtual display
without precision touchpad events. It is not a hardware-trackpad test.

## Input, acknowledgement and rendering model

For each region, Toastty retains an offset in content-row units relative to its
last acknowledged `pos`. A quarter-row downward gesture changes this offset by
0.25 without writing anything to the PTY. Crossing a row boundary queues an SGR
wheel event; after the app paints `pos + 1`, Toastty subtracts that row from the
offset. The new cell content and reduced offset become visible together.

Only one wheel request per region is in flight. Further input accumulates up to
32 rows; each acknowledgement can release the next request. This avoids flooding
an unresponsive process and makes direction reversals unambiguous while a request
is outstanding. The offset is also clamped to the app's `[0, max]` range.

There is only one spare bottom row. Until an acknowledgement arrives, the displayed
offset is clamped to `[0, 0.999]`: it pauses at the edge rather than drawing missing
content. Upward motion from an integer position needs the previous row from the
app before any pixels can move. This is a deliberate limitation, not zero-latency
scrolling under arbitrary network or application delay.

An unchanged `pos` is not an acknowledgement and does not refresh the 500 ms
request timeout. A matching one-row change acknowledges the request; any other
position change cancels outstanding motion as an application jump. Timeout drops
motion, redraws the current grid and removes its wake-up deadline. Requests are
drained only after the parser batch, so a screen exit later in the same batch
cannot leak a queued wheel event into the shell.

Precision input is intercepted only with SGR mouse tracking and
`scrollback.smooth_scrolling=true`. Ordinary wheel notches and horizontal wheel
input keep their existing paths. Apps remain responsible for keyboard scrolling.
Mouse hit-testing applies the same fractional transform as rendering.

The renderer clips and translates existing text and Kitty/Sixel image quads,
including their texture coordinates. It does not resample or replace the logical
grid. Neighboring cells, the terminal's overlays and fixed headers/footers stay
put. Full frame reconstruction is used on changed frames with regions, avoiding
stale pixels from the partial-redraw cache; an idle region schedules no animation.
The existing synchronized-output gate still applies.

## Deliberate limits and next protocol questions

- This is experimental wire compatibility, not a claim of validation against
  every Ghostty Smooth client. Neovim 0.12/pixelscroll integration and hardware
  touchpads on macOS/Wayland need additional platform qualification.
- One spare row imposes an acknowledgement-latency ceiling. Two-sided overscan,
  a viewport generation, explicit request IDs and a bounded batch-ack format are
  candidates for a future version, not extra fields silently added to v3.
- A one-row keyboard move coincident with a pending wheel request cannot be
  distinguished from its acknowledgement by this wire format.
- Position counts displayed rows, not buffer lines. Wrapped lines, folds, pinned
  gutters, overlays and independently moving objects require client cooperation.
  Put fixed gutters outside the rectangle; do not declare overlapping regions.
- RGP 3D scenes and rich Markdown blocks are not region content in this version.
  Text and raster-image layers are supported.
- A multiplexer must translate pane coordinates, manage region IDs and lifetime,
  and preserve frame ordering. Blind passthrough is unsafe for split panes.
  No tmux/screen/zellij compatibility is claimed.
- The feature adds no dependencies or always-on frame timer. The bounded region
  table and full-redraw choice favor correctness over fine-grained damage
  optimization; performance should be measured on real GPUs before broad rollout.

## Reproducible checks

```sh
cargo test -p toastty-term
cargo test -p toastty-render --lib smooth
cargo test -p toastty --bin toastty
cargo clippy -p toastty-term -p toastty-render -p toastty --all-targets -- -D warnings
```

The focused tests exercise split APC input, strict bounds, overlap and region
quotas, fractional input, both directions, slow/absent acknowledgement, backlog
draining, application jumps, screen/resize/font reset, logical-grid invariance,
transformed hit-testing, texture-coordinate clipping, and GPU frame isolation.
The GPU test uses the real `Renderer::render_term`, verifies synchronized metadata
publication, compares every pixel outside the region, and verifies exact restoration
after removal. PR captures run the real Toastty app with isolated Xvfb state, not
a web mock-up.
