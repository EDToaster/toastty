# toastty

A lightweight, GPU-accelerated terminal emulator in Rust.

See [`docs/architecture.md`](./docs/architecture.md) for high-level design,
[`docs/protocols.md`](./docs/protocols.md) for the protocol support matrix,
[`docs/selection.md`](./docs/selection.md) for mouse selection behaviour,
[`docs/compat-checklist.md`](./docs/compat-checklist.md) for known gaps,
and [`docs/decisions/`](./docs/decisions/) for individual decision records
with measurements.

## Layout

Cargo workspace under `crates/`:

| Crate | Role |
| --- | --- |
| `toastty` | Binary entry point; wires everything together |
| `toastty-pty` | PTY open/read/write (Unix; ConPTY in v2) |
| `toastty-io` | mio + winit bridge; PTY readiness + frame deadline |
| `toastty-window` | Thin winit wrapper (Caps/Num Lock, IME, dead keys) |
| `toastty-parser` | vte wrapper + our own streaming APC scanner |
| `toastty-term` | Grid (ring buffer), modes, cursor, scrollback, damage tracking |
| `toastty-protocols` | One module per protocol handler |
| `toastty-graphics` | Kitty graphics, Sixel, RGP decoders |
| `toastty-render` | wgpu renderer; cosmic-text shaping; user shader pipeline |
| `toastty-config` | TOML config loading |

## Keyboard shortcuts

| Action | Linux / Windows | macOS |
| --- | --- | --- |
| Copy selection | Ctrl+Shift+C | Cmd+C |
| Paste clipboard | Ctrl+Shift+V | Cmd+V |
| Paste PRIMARY selection | Shift+Insert | Shift+Insert (clipboard) |
| Open hyperlink | Ctrl+click | Cmd+click |
| Copy a Markdown block's source | click the copy chip shown at the block's top right on hover | same |
| Zoom in | Ctrl+= (also Ctrl++, Ctrl+Shift+=, Ctrl+keypad +) | Cmd+= (same variants) |
| Zoom out | Ctrl+- (also Ctrl+Shift+-, Ctrl+keypad -) | Cmd+- |
| Reset zoom | Ctrl+0 | Cmd+0 |

### Markdown

`toastty md README.md` (or `cat notes.md | toastty md`) prints a Markdown
document as a rendered block: headings, tables, code, maths and Mermaid
diagrams, laid out for the window width and re-laid out when it changes.
Selecting text copies the rendered text; the copy chip copies the source;
links open with Ctrl+click (Cmd+click on macOS). Outside toastty, and inside
tmux / screen / zellij / herdr for now, it prints the plain Markdown. See
[docs/markdown.md](docs/markdown.md) and the `[security] markdown` /
`markdown_local_files` config keys.

### Font zoom

- Each step changes the font by 1 logical px, clamped to 6–72 px. A
  `font.size_px` outside that range is kept as it is: the range is widened to
  include it, so zoom never jumps away from it. Reset goes back to
  `font.size_px`.
- The zoomed size is logical, like `font.size_px`, so it is multiplied by the
  monitor scale factor (17 px on a 2× display is rasterised at 34 px).
- The window keeps its size. The grid is recomputed and goes through the same
  path as a window resize: scrollback reflow, PTY winsize (including pixel
  size, so `stty size` / `tput cols` and SIGWINCH update), and a mode 2048
  in-band resize report for apps that enabled it. The selection is cleared,
  just as it is on a window resize.
- Config hot-reload: if `font.size_px` changes, the zoom resets to the new
  size. Any other edit keeps the current zoom.
- Zoom keys are always taken by toastty and never reach the app, whatever the
  kitty keyboard protocol flags are. kitty, Ghostty and WezTerm do the same.
  This differs from copy: copy only forwards Ctrl+Shift+C under the kitty
  protocol when nothing is selected, because "no selection" leaves copy with
  nothing to do, which never happens for zoom. Shift is ignored (`+` is
  Shift+= on US layouts) and so are Caps Lock / Num Lock. Alt, or the other
  platform's modifier, turns the binding off, so Ctrl+Alt+= still reaches
  the app. With Num Lock off, keypad 0 is Insert and does not reset.
- Until configurable keybindings exist this means an app can't receive
  Ctrl+= / Ctrl+- / Ctrl+0 (Cmd on macOS).

## Development

```sh
make check         # cargo check --workspace --all-targets
make lint          # clippy with -D warnings
make fmt           # rustfmt
make test          # cargo test --workspace
make cover         # coverage summary
make cover-gate    # fail if <95% on gated crates
make cover-html    # generate browsable HTML coverage report
```

Install the coverage tool once with `cargo install cargo-llvm-cov`.

## Testing conventions

- **Logic crates** (`parser`, `term`, `protocols`, `config`, `graphics`,
  pure-function submodules of `render`): **95% line coverage gate** enforced
  by `make cover-gate`.
- **I/O crates** (`pty`, `io`): covered by integration tests using real PTYs
  and child processes. Excluded from the line gate.
- **Window crate**: covered by integration tests that script winit's
  `EventLoop`. Excluded from the line gate.
- **Render crate**: pure-function submodules (`layout`, `atlas`, `viewport`,
  `damage`, `cluster_width`) are gated. Pipeline/device code is exercised by:
  - **`build.rs`** validates every `.wgsl` shader via `naga` at build time.
  - **Headless snapshot tests** under `crates/toastty-render/tests/` use
    a hidden wgpu device, render canonical scenarios, and diff against
    golden images with `image-compare` (SSIM threshold).
  - **wgpu validation** is enabled in tests via `cfg(test)` flags.
  - **Property tests** (`proptest`) exercise grid → instance buffer
    transformations.
