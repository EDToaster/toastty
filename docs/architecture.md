# toastty — Architecture

High-level design for toastty, a lightweight GPU-accelerated terminal emulator in Rust. Companion to [`protocols.md`](./protocols.md). Decision records that back the choices on this page live in [`decisions/`](./decisions/).

## Goals

- **Lightweight + performant.** Small binary, low idle RAM, minimal dependencies. Alacritty-class baseline; not Bevy-class.
- **Modern protocol support.** First-class for the 2024–2026 wave (mode 2026 synchronized output, mode 2027 grapheme clusters, mode 2048 in-band resize, kitty keyboard, OSC 133).
- **Inline graphics.** Kitty graphics primary, Sixel fallback.
- **Experimental 3D.** Ship Ratty Graphics Protocol (RGP) as a first-class citizen to help seed the ecosystem.
- **User-customizable shaders.** ShaderToy-style fragment shader post-process; hot reload.
- **Cross-platform.** macOS, Linux, Windows (Windows/ConPTY is v2).

## Non-goals (for now)

- ECS / game-engine framing. RGP is rendered, not simulated.
- A built-in multiplexer. Compose with tmux/zellij.
- A Lua/JS scripting runtime. Config file + custom shaders only.
- iTerm2 inline images. Kitty + Sixel already cover the use case.

## Decisions index

Each row links to its decision record with measurements and code excerpts.

| # | Area | Decision | Record |
| --- | --- | --- | --- |
| 1 | PTY + event loop | `mio::Poll` integrated with winit; `Poll(timeout)` doubles as frame timer | [pty-event-loop](./decisions/pty-event-loop.md) |
| 2 | Window + input | `winit 0.30.13` pinned, behind a `toastty-window` wrapper | [window-input](./decisions/window-input.md) |
| 3 | Text stack | `cosmic-text 0.19` + custom wgpu renderer + cluster-width override | [text-stack](./decisions/text-stack.md) |
| 4 | 3D / RGP | Hand-rolled wgpu scene; `rend3` is unmaintained | [rgp-3d-path](./decisions/rgp-3d-path.md) |
| 5 | APC parsing | Stream payloads via `ApcHandler`; ship our own scanner (vte drops APC) | [streaming-apc](./decisions/streaming-apc.md) |
| 6 | Scrollback | Ring buffer of `Row { cells: SmallVec<[Cell; 16]>, soft_wrap }` | [scrollback](./decisions/scrollback.md) |
| 7 | Redraw | Damage-tracked with submit suppression; skip submit when clean | [redraw-policy](./decisions/redraw-policy.md) |
| 8 | Shaders | Dual-path WGSL + GLSL-via-naga; 100% ShaderToy port rate observed | [shader-pipeline](./decisions/shader-pipeline.md) |

## Pinned versions

Exact versions validated by the decision prototypes. Bump deliberately, not casually.

| Crate | Version | Role |
| --- | --- | --- |
| `winit` | `=0.30.13` | Window + input (avoid 0.31.0-beta API churn) |
| `wgpu` | `=29.0.3` | GPU backend |
| `naga` | `29.x` | WGSL + GLSL → SPIR-V/Metal/etc.; GLSL frontend is now solid |
| `vte` | `=0.15.0` | Parser for CSI/OSC/DCS only — does **not** handle APC |
| `cosmic-text` | `=0.19.0` | Text shaping (harfrust under the hood) + layout |
| `swash` | `0.2.x` | Glyph rasterization (transitive via cosmic-text) |
| `mio` | latest | PTY readiness + frame deadline poll |
| `memchr` | `=2.8.0` | Fast scanning in the APC parser |

## Protocol communication model

Everything flows through the PTY byte stream — bidirectional pipe between toastty and the child process.

- **App → toastty** (stdout): cursor moves, SGR, OSC titles, Kitty graphics uploads, RGP object placement.
- **toastty → App** (stdin): keyboard input, mouse reports, query responses, resize notifications (mode 2048), focus events.

All "protocols" are framings inside that byte stream — no side channels.

### Sequence framings

| Introducer | Name | Used for | Parser |
| --- | --- | --- | --- |
| `ESC` + final | Single-char | Small commands (save cursor, keypad mode) | `vte` |
| `CSI` (`ESC [`) | Control Sequence Introducer | Cursor, SGR, modes, mouse, kitty keyboard | `vte` |
| `OSC` (`ESC ]`) | Operating System Command | Titles, hyperlinks, clipboard, semantic prompts | `vte` |
| `DCS` (`ESC P`) | Device Control String | Sixel, XTGETTCAP responses | `vte` |
| `APC` (`ESC _`) | Application Program Command | Kitty graphics, RGP | **toastty** (vte drops these) |
| C0 controls | Non-escape | BEL, BS, HT, LF, CR | `vte` |

Variable-length sequences (OSC/DCS/APC) end with `ST` (`ESC \`).

> **APC caveat (decision #5):** `vte 0.15.0` routes APC bytes into its `anywhere()` state and discards them — there is no `apc_dispatch` hook. We ship our own APC scanner alongside vte's CSI/OSC/DCS handling. The scanner is ~90 logical LoC and exposes a streaming `ApcHandler { start, chunk, end }` trait; a `BufferingApcHandler` adapter wraps it for handlers that prefer the whole-payload form.

## Architectural layers

```
┌──────────────────────────────────────────────────────────┐
│  Renderer (wgpu 29)                                      │
│    Background → 3D (RGP) → Cells (text+image) → Post     │
│    Damage-tracked; skips submit when clean               │
└──────────────────────────────────────────────────────────┘
                          ▲
┌─────────────────────────┴────────────────────────────────┐
│  Terminal State                                          │
│    grid (ring buffer), cursor, modes, image registry,    │
│    RGP scene, keyboard protocol flag stack, dirty set    │
└──────────────────────────────────────────────────────────┘
                          ▲
┌─────────────────────────┴────────────────────────────────┐
│  Dispatcher                                              │
│    one method per parsed event; routes to handlers       │
│    sets dirty cells, pause_rendering, query replies      │
└──────────────────────────────────────────────────────────┘
                          ▲
┌─────────────────────────┴────────────────────────────────┐
│  Parser                                                  │
│    vte (CSI/OSC/DCS) + toastty APC scanner               │
└──────────────────────────────────────────────────────────┘
                          ▲
┌─────────────────────────┴────────────────────────────────┐
│  PTY I/O                                                 │
│    mio::Poll on master fd; Poll(timeout) is frame deadline│
│    Background thread → winit UserEvent::PtyReady         │
└──────────────────────────────────────────────────────────┘
                          ▲
                          │
                       Shell / TUI app
```

The renderer pulls from terminal state every frame; it does not drive the parser. This decoupling is what makes synchronized output (mode 2026) clean — the dispatcher flips a `pause_rendering` flag, the renderer skips frames until ESU.

## Event loop & PTY I/O (decision #1)

A single render thread owns: the PTY master `OwnedFd`, the parser, terminal state, and the wgpu queue. A background **mio thread** runs `Poll::poll(timeout)` over the PTY fd and posts `UserEvent::PtyReady` to winit via `EventLoopProxy::send_event` when bytes are ready.

The `Poll(timeout)` value is set to `next_frame_deadline - now`. This collapses two event sources into one — no separate timerfd, no `tokio::interval`. It maps 1:1 onto winit's own `ControlFlow::WaitUntil(deadline)`.

Why not the alternatives:
- **tokio:** the bounded mpsc between kernel reads and the parser adds a scheduler hop per byte — 5× worse p99 latency. Don't put a channel between kernel and parser.
- **Dedicated blocking read thread:** 115–130% CPU under firehose vs 99% for mio. Park/unpark per pipe-fill cycle doesn't coalesce the way kqueue/epoll waiters do.

Throughput on `yes` firehose: **1.66 GiB/10s, p99 latency 4 µs, 63 LoC of glue.**

Because the render thread owns the master fd, query/response (`CSI ? 2026 $ p`) and mode 2048 (in-band resize) are synchronous `write()` calls from inside the dispatcher — no mutex, no channel hop.

## Window + input (decision #2)

`winit 0.30.13` pinned, behind a thin `toastty-window` wrapper crate. Pin exact: 0.30 minor releases ship breaking changes; 0.31-beta swaps to `Box<dyn Window>` and renames events.

Why a wrapper rather than using winit directly: a handful of platform realities that bleed into the kitty keyboard protocol's correctness.

**Three sharp edges the wrapper handles:**

1. **Caps Lock / Num Lock are not in `winit::keyboard::ModifiersState`.** The kitty protocol requires them. The wrapper reads OS LED state (Alacritty does this; WezTerm goes further and rolls its own input stack to avoid the problem entirely).
2. **macOS dead keys route through IME, not `Key::Dead`.** Option+E only works when `set_ime_allowed(true)`. The wrapper keeps IME on by default and exposes named inhibitors for apps that need raw keys.
3. **Wayland `RedrawRequested` cadence is unreliable** (winit issues #1619, #2609). The wrapper schedules redraws with `ControlFlow::WaitUntil` rather than relying on the event itself.

**Critical detail for the kitty keyboard handler:** use `key.text_with_all_modifiers()` from the `KeyEventExtModifierSupplement` trait, not `KeyEvent.text`. The latter omits Ctrl by design and would produce `"a"` for `Ctrl+A` instead of `"\x01"`. Most winit examples use the wrong field.

## Renderer

### Backend: wgpu

Chosen over raw Vulkan, OpenGL, or native APIs.

| Reason | Detail |
| --- | --- |
| Same mental model as Vulkan | Command buffers, render passes, pipelines, bind groups |
| Cross-platform output | Vulkan/Linux, Metal/macOS, DX12/Windows, GL fallback |
| No MoltenVK detour on macOS | Raw Vulkan would force MoltenVK; wgpu emits native Metal |
| Mature ecosystem | Bevy, WezTerm, Zed all ship on it |
| Shader toolchain free | WGSL native; GLSL via `naga` |

Raw Vulkan was rejected: ~1000 LoC boilerplate before drawing a triangle, no native macOS, manual barriers/fences/layout transitions, no shader compiler included.

> **wgpu 29 API papercuts** (logged from decision #8): `InstanceDescriptor::default()` is gone (use `new_without_display_handle_from_env`), `on_uncaptured_error` takes `Arc<dyn ...>` not `Box`, `pop_error_scope` is now a guard pattern, `multiview_mask: None` is required in `RenderPassDescriptor`, `mipmap_filter` is `MipmapFilterMode` not `FilterMode`. Tutorials lag behind.

### Render passes

```
1. Background       solid fill + optional user background shader
2. 3D (RGP)         depth-tested scene → color + depth attachments
3. Cells            instanced quads for text + images; reads depth
                    so 3D can occlude text or sit beneath it
4. Post-process     user-supplied fragment shader; built-in effects
                    (CRT, scanline, film grain) optional
```

### Redraw policy (decision #7)

**Damage-tracked partial redraw with submit suppression.** Per frame:

1. If `pause_rendering` is set (mode 2026 BSU active), skip immediately.
2. Else if `dirty.is_empty()` and no animation is due, **skip submit entirely** — no GPU work at all.
3. Else build instances from the dirty set, render with `LoadOp::Load` to preserve the previous framebuffer, overdraw only dirty cells.

Idle workload: 0.3% CPU vs 9.3% for full-vsync redraw — **30× cheaper at idle.**

Why not hybrid (full redraw when damage is large): on modern GPUs the submit overhead dominates the per-instance work. 12000 instances and 1 instance both submit in ~1.55 ms on Apple Silicon. There's nothing to amortize. Skip-or-submit is the only knob.

### Synchronized output contract (mode 2026)

`Modes` exposes `pause_rendering: bool` — set by the BSU/ESU handler. The renderer reads it at frame start; if set, skip the frame.

Timeout: **~1 s** (matches tmux). If ESU never arrives, the renderer force-flushes.

> **Subtle bug surfaced by decision #7:** when the timeout force-flushes, set `timeout_force_flushed: true` in the BSU state. The next post-ESU frame must mark the entire grid dirty for a corrective full redraw — otherwise damage tracking will emit a tiny dirty list, leaving the partial-state flash visible for one frame.

### 3D layer / RGP (decision #4)

**Hand-rolled wgpu scene graph.** ~700 LoC owns the entire 3D pass.

| Option | Stripped binary | Crates | Idle RAM | Active RAM | Frame |
| --- | ---: | ---: | ---: | ---: | ---: |
| **Hand-rolled wgpu** | **5.4 MiB** | **125** | **92 MiB** | **103 MiB** | **6.7 ms** |
| rend3 | 5.5 MiB | 168 | 109 MiB | 110 MiB | 8.6 ms |
| Bevy headless | 32.6 MiB | 258 | 135 MiB | 184 MiB | 14.4 ms |

> **`rend3` is effectively unmaintained.** Last crates.io release `0.3.0` (March 2022, pinned to `wgpu 0.12`). Trunk last touched May 2024 targeting `wgpu 0.19`. Earlier drafts of this doc listed rend3 as the upgrade path for shadows/PBR — that path doesn't exist today.
>
> If RGP grows past what hand-rolled can ergonomically support, the real escape hatch is **Bevy headless** (render graph only), at the cost of the binary-size hit shown above. Re-evaluate when we have a concrete need (skinned animation, PBR materials, shadow maps), not preemptively.

**Z-ordering:** RGP renders to color + depth in pass 2. The cell pass reads that depth buffer, so 3D objects can occlude text or sit underneath depending on their depth. The grid never moves; 3D is overlaid in the same screen space.

### User custom shaders (decision #8)

**Dual-path: WGSL primary, GLSL via `naga`.** Post-process fragment shader after the cell pass.

| Input | Form |
| --- | --- |
| WGSL | Direct compile to backend |
| GLSL (`mainImage` style) | Wrapped in a ~50 LoC shim that synthesizes `main()`, translated by `naga` to WGSL/SPIR-V/MSL/HLSL |

Out-of-the-box ShaderToy port rate measured at **14/14 = 100%** across mat2 rotation, palette functions, voronoi, mandelbrot, smin SDFs, raymarched normals, `dFdx`/`dFdy`, `discard`, `textureLod`. Compile time sub-millisecond.

**Inputs the user gets** (matches ShaderToy conventions where sensible):
- `iTime`, `iFrame`, `iResolution`
- `iCursor` (cell coords)
- Previous framebuffer texture

**Hot reload:** `notify` crate watches the shader file → recompile → swap pipeline. Compile errors are surfaced in the status line; the renderer keeps the last-good pipeline live so a typo never crashes the terminal.

> **`#define`-based uniform aliases break swizzle parsing.** Don't translate ShaderToy's `iMouse` via `#define`; declare it as a top-level `vec4` in the shim and copy from the UBO inside the synthesized `main()`.

### Text rendering (decision #3)

**`cosmic-text 0.19` + custom wgpu renderer + cluster-width override pass.**

| Concern | Choice |
| --- | --- |
| Shaping | `cosmic-text` (uses harfrust under the hood; bidi, complex scripts, fallback chain) |
| Glyph rasterization | `swash` (transitive via cosmic-text) |
| Atlas packing | Custom; **two atlases** (mask + color) like glyphon, so emoji churn never evicts text glyphs |
| Image decode | `image` crate; consider `zune-png` for hot paths |
| Sixel decode | hand-roll or `sixel-rs` |

Rejected:
- **glyphon** — pins `cosmic-text 0.18.2`, owns too much of the pipeline; starting one major version behind on the shaping engine is a non-starter.
- **swash direct** — 3–5× the LoC at production quality, no measured perf win.

> **Mode 2027 requires a cluster-width override pass.** `Buffer::set_monospace_width` rounds *per-glyph advance*, not *per-cluster advance*. CJK ideographs, ZWJ emoji, and VS16 emoji all come out as ~1.71 cells (13.71 px) with cell=8, font=14. After shaping, group glyphs by `(LayoutGlyph.start..end)` and re-snap each cluster to its declared cell count. Required for both mode-2027 apps and any text that contains wide characters.

### Scrollback (decision #6)

**Ring buffer of fixed-capacity rows.** `Box<[Row; CAP]>` where:

```rust
struct Row {
    cells: SmallVec<[Cell; 16]>,
    soft_wrap: bool,   // load-bearing for reflow
}
```

Why ring, not flat `Vec`: ring's by-index read is `arr[(head + idx) % CAP]` — a single masked load. The renderer pulls the visible viewport every frame, so cheap random access matters more than mid-buffer edits (which a terminal never does).

Rejected:
- **Flat `Vec<Row>`** — equivalent at the macro level, but the ring's once-allocated discipline is cleaner for the steady-state append path.
- **Rope of cells** — 7× slower on random scroll; the renderer would burn ~70 µs/frame just on text fetch.

> **`SmallVec` inline size is a trap.** `SmallVec<[Cell; 80]>` (so 80-col rows are zero-alloc) reserves 3216 B per *empty* ring slot — 643 MB at 200k slots. **Inline=16** is the sweet spot.
>
> **Memory dominator is `Cell` layout, not the container.** Pack `Style` into a u32 stylesheet ID; intern hyperlink URLs as `NonZeroU16` IDs. That's a larger win than any data-structure change.

**Smooth scrolling** is renderer-side, not scrollback-side. The renderer holds `top_line: u64` + `pixel_offset: f32`; per frame it asks the ring for `top..top+visible+1` rows. Ring measured 83 ns/row random access — 2 µs/frame for a 25-row viewport.

**Reflow on resize** walks soft-wrapped row runs, coalesces them into logical lines, re-shapes at the new width, writes back into the ring. Ring eviction handles row-count changes for free. Measured 66 µs median for 100k lines at 80↔120 cols.

**Alt screen does NOT reflow.** Mode 1049 apps redraw themselves on `SIGWINCH` / mode 2048. Discard the alt-screen grid, re-allocate at new size.

## Workspace layout

Single Cargo workspace; multiple crates so protocol modules and renderer stages stay decoupled and individually testable.

```
toastty/
├── Cargo.toml                      ─ workspace
├── README.md
├── docs/
│   ├── architecture.md             ─ this file
│   ├── protocols.md
│   └── decisions/                  ─ 8 decision records
├── crates/
│   ├── toastty-pty/                ─ PTY open/read/write; ConPTY later (v2)
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── unix.rs
│   │       └── windows.rs
│   │
│   ├── toastty-io/                 ─ mio loop, frame deadline, winit bridge
│   │   └── src/
│   │       ├── lib.rs
│   │       └── proxy.rs            ─ EventLoopProxy::send_event glue
│   │
│   ├── toastty-window/             ─ thin winit wrapper
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── input.rs            ─ key.text_with_all_modifiers()
│   │       ├── modifiers.rs        ─ Caps/Num lock LED state
│   │       └── ime.rs              ─ macOS dead keys, preedit
│   │
│   ├── toastty-parser/             ─ vte wrapper + APC scanner
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── vte_bridge.rs       ─ CSI/OSC/DCS via vte
│   │       ├── apc.rs              ─ our APC state machine
│   │       └── events.rs           ─ Csi, Osc, Dcs, Apc structs
│   │
│   ├── toastty-term/               ─ state: grid, scrollback, modes, cursor
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── grid.rs             ─ ring buffer
│   │       ├── cell.rs             ─ packed Style ID, hyperlink ID
│   │       ├── reflow.rs           ─ soft-wrap coalesce + re-shape
│   │       ├── modes.rs            ─ DECSET/DECRST registry
│   │       ├── cursor.rs
│   │       ├── damage.rs           ─ dirty cell set
│   │       └── charset.rs
│   │
│   ├── toastty-protocols/          ─ one module per protocol
│   │   └── src/
│   │       ├── lib.rs              ─ Protocol trait + dispatcher
│   │       ├── sgr.rs
│   │       ├── mouse.rs            ─ 1000/1002/1003/1006
│   │       ├── osc.rs              ─ 0/1/2/7/8/10/11/12/52
│   │       ├── semantic_prompt.rs  ─ OSC 133
│   │       ├── progress.rs         ─ OSC 9;4
│   │       ├── kitty_keyboard.rs   ─ CSI u + flag stack
│   │       ├── synchronized.rs     ─ mode 2026 + timeout flag
│   │       ├── unicode_core.rs     ─ mode 2027
│   │       ├── resize_inband.rs    ─ mode 2048
│   │       ├── bracketed_paste.rs  ─ mode 2004
│   │       └── focus.rs            ─ mode 1004
│   │
│   ├── toastty-graphics/           ─ image + 3D protocol handlers
│   │   └── src/
│   │       ├── lib.rs              ─ GraphicsBackend trait
│   │       ├── kitty.rs            ─ primary image protocol; chunked reassembly
│   │       ├── sixel.rs            ─ fallback image protocol
│   │       └── rgp.rs              ─ experimental 3D
│   │
│   ├── toastty-render/             ─ wgpu renderer
│   │   ├── src/
│   │   │   ├── lib.rs              ─ Renderer, frame loop
│   │   │   ├── device.rs           ─ adapter/device/queue setup
│   │   │   ├── pipelines/
│   │   │   │   ├── background.rs
│   │   │   │   ├── text.rs         ─ instanced quad + glyph atlas
│   │   │   │   ├── image.rs        ─ kitty/sixel decoded textures
│   │   │   │   ├── rgp.rs          ─ hand-rolled 3D pass
│   │   │   │   └── post.rs         ─ user shader post-process
│   │   │   ├── atlas.rs            ─ mask + color, dual atlases
│   │   │   ├── cluster_width.rs    ─ mode 2027 cluster snap
│   │   │   ├── viewport.rs         ─ smooth scroll state
│   │   │   ├── shader_loader.rs    ─ WGSL + GLSL→naga, hot reload
│   │   │   └── uniforms.rs
│   │   └── shaders/                ─ built-in WGSL
│   │
│   ├── toastty-config/             ─ config file, themes
│   │   └── src/lib.rs
│   │
│   └── toastty/                    ─ the binary
│       └── src/
│           ├── main.rs
│           └── app.rs              ─ wires everything together
│
└── terminfo/
    └── toastty.terminfo
```

## Open questions

Decisions that remain genuinely open.

- **ConPTY support timeline.** Windows-native shipping is a meaningful chunk of work — defer to a v2 milestone. mio has Windows named-pipe support, no architectural blocker.
- **Shell integration delivery.** Ship our own shell snippets for OSC 133 emission, or rely on existing kitty/iTerm2 integrations users already have configured.
- **Cell layout finalization.** The decision #6 finding (memory dominated by `Cell` size) points at packing `Style` and `hyperlink_id` into a single u64, but the exact bit layout depends on how much SGR state we expose (256-color vs truecolor, dim/strikethrough/underline-style coverage). Will fall out of implementing SGR handling.
- **Bevy-headless escape hatch.** Pre-decide a feature flag boundary now (so `toastty-graphics::rgp` doesn't bake hand-rolled assumptions everywhere), or wait until we hit a concrete RGP fidelity ceiling.
