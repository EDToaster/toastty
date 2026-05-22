# toastty

A lightweight, GPU-accelerated terminal emulator in Rust.

See [`docs/architecture.md`](./docs/architecture.md) for high-level design,
[`docs/protocols.md`](./docs/protocols.md) for the protocol support matrix,
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
