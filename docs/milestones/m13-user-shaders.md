# M13 — User shaders and post-process

**Goal.** Let users run ShaderToy/Ghostty-style fragment shaders over the
terminal image. Ghostty's shaders should paste in unchanged, and a shader
that doesn't animate must cost nothing at idle.

**Status.** Spec agreed 2026-10-22. It supersedes parts of
[decision #8](../decisions/shader-pipeline.md): GLSL is the main
language in practice, the y axis points down, and there is no
`iCursor`. The rest of #8 (naga, the shim, last-good on error) stands.

## Config

```toml
[shader]
# Post-process shaders, run in order. Each one's output is the next one's
# iChannel0. Empty (the default) turns post-processing off.
# `~` expands; relative paths are relative to the config file's directory.
# The extension picks the language: .glsl / .frag = GLSL, .wgsl = WGSL.
paths = []
# "auto"   detect from the shader (below)
# "always" redraw at ~60 Hz all the time, even unfocused
# "never"  redraw only on terminal updates and shader-input events
animation = "auto"
```

Shader files are watched. On a change they are recompiled. If any shader
in the chain fails, the error goes to the in-window error banner
(`path:line:col: message`, line numbers in the user's file, not the
prelude-wrapped source) and the previous working chain stays live. A
config reload that changes `[shader]` behaves the same way.

## Languages

### GLSL (ShaderToy / Ghostty dialect)

The user file contains `void mainImage(out vec4 fragColor, in vec2
fragCoord)` plus any helpers, and nothing else. We prepend a prelude and
append a `main()`; naga's GLSL frontend parses the result. The prelude:

- `#version 450 core`. A `#version` line in the user file is blanked
  (kept as an empty line so line numbers stay right).
- An unnamed std140 uniform block (`set = 0, binding = 0`) whose members
  are the uniforms below, so the user writes `iTime` directly. naga turns
  each access into `AccessIndex(GlobalVariable(block), i)`.
- `texture2D iChannel0_tex` and `sampler iChannel0_smp` at bindings 1 and
  2, plus `#define iChannel0 sampler2D(iChannel0_tex, iChannel0_smp)`.
- `#define texture2D texture` and the `CURSORSTYLE_*` macros (Ghostty
  compatibility).
- `main()` passes `gl_FragCoord.xy` straight through as `fragCoord`,
  without flipping it (see Coordinates).

`iChannel1..3` are not declared. Using one is a compile error that names
the line.

### WGSL

The user file contains `fn mainImage(fragCoord: vec2f) -> vec4f`. The
prelude declares `var<uniform> toastty: ToasttyUniforms` (same member
names as GLSL, so `toastty.iTime`), `iChannel0: texture_2d<f32>` and
`iChannel0Sampler: sampler` at the same bindings, the
`CURSORSTYLE_*` constants, and an entry point that calls `mainImage`.

## Coordinates and colour

- **The y axis points down** with the origin at the top left: wgpu's
  native convention, and the one Ghostty uses on macOS. So `fragCoord` is
  `gl_FragCoord.xy` (pixel centres at .5), and
  `texture(iChannel0, fragCoord / iResolution.xy)` samples the same pixel.
  Nothing is flipped anywhere. ShaderToy shaders that draw relative to
  the bottom of the screen come out upside down, as they do in Ghostty on
  macOS.
- `iResolution` is the full surface, padding included.
- **Colours are sRGB-encoded** (display values in 0–1), as on ShaderToy.
  `iChannel0` is sampled through a non-sRGB view of the scratch texture,
  and the last pass writes through a non-sRGB view of the swapchain
  image. A pass-through shader therefore reproduces the frame exactly.
  Colour uniforms (`iPalette`, `iBackgroundColor`, …) are sRGB-encoded
  too, converted from the renderer's linear theme values.
- **Alpha is premultiplied.** `iChannel0` holds what the cell pass wrote
  (premultiplied; alpha 1 unless the theme bg is transparent). The output
  is presented as is, so a shader that writes `alpha = 1` makes a
  transparent window opaque. This is documented, not handled for v1.
- The sampler is linear-filtered and clamps to the edge.

## Uniforms

Names and types match Ghostty's `shadertoy_prefix.glsl`
(ghostty-org/ghostty `f54340ad`) so its shaders compile unchanged.

| Uniform | Type | Value |
|---|---|---|
| `iResolution` | `vec3` | `(width, height, 1)` px |
| `iTime` | `float` | Seconds since the first post-processed frame |
| `iTimeDelta` | `float` | Seconds since the previous frame |
| `iFrameRate` | `float` | `1 / iTimeDelta`, smoothed |
| `iFrame` | `int` | Frames rendered with post-processing |
| `iChannelTime[4]` | `float[4]` | 0 |
| `iChannelResolution[4]` | `vec3[4]` | `[0]` = `iResolution`, the rest 0 |
| `iMouse` | `vec4` | ShaderToy semantics, y-down px: `xy` = pointer position while button 1 is held (last held position otherwise); `zw` = position of the last press, `z` negative once released, `w` negative after the press frame |
| `iDate` | `vec4` | `(year, month - 1, day, seconds since local midnight)` |
| `iSampleRate` | `float` | 0 |
| `iCurrentCursor` | `vec4` | Drawn cursor rect (a bar cursor is thin): `xy` = `(left, top + height)`, the left and **bottom** edge (Ghostty's y-down convention: the −x, +y corner), `zw` = width, height |
| `iPreviousCursor` | `vec4` | Value before the last change |
| `iCurrentCursorColor`, `iPreviousCursorColor` | `vec4` | sRGB RGBA |
| `iCurrentCursorStyle`, `iPreviousCursorStyle` | `int` | `CURSORSTYLE_BLOCK` 0, `_BLOCK_HOLLOW` 1, `_BAR` 2, `_UNDERLINE` 3, `_LOCK` 4 |
| `iCursorVisible` | `int` | 0 / 1 |
| `iTimeCursorChange` | `float` | `iTime` when the cursor rect or colour last changed; `-1e6` until the first change |
| `iTimeFocus` | `float` | `iTime` when the window last gained focus (0 at start) |
| `iFocus` | `int` | 0 / 1 |
| `iPalette[256]` | `vec3[256]` | Live palette (OSC 4 included) |
| `iBackgroundColor`, `iForegroundColor` | `vec3` | Current theme (after reverse video) |
| `iCursorColor`, `iCursorText` | `vec3` | |
| `iSelectionBackgroundColor`, `iSelectionForegroundColor` | `vec3` | |

Toastty extensions:

| Uniform | Type | Value |
|---|---|---|
| `iTimeBell` | `float` | `iTime` at the last BEL; `-1e6` before the first |
| `iBellCursor` | `vec4` | The cell the cursor was in when the BEL arrived, same layout as `iCurrentCursor` |
| `iBellCount` | `int` | BELs since start |
| `iBells[4]` | `vec4[4]` | The last four BELs, newest first: `xy` = centre of the bell cell (px), `z` = `iTime` at the bell, `w` = seconds since the bell, computed on the CPU in f64 (exact even after days of uptime). Unused slots: `(0, 0, -1e6, 1e6)` |
| `iCellSize` | `vec2` | Cell width, height (px) |
| `iGridRect` | `vec4` | The cell grid: `xy` = top-left (px), `zw` = size (px) |

The bell position comes from `Term::last_bell_cell()` (screen row/col
captured when BEL is parsed) and `Term::bell_revision()`, mapped to
pixels the way `cursor_pixel_rect` maps the cursor (view offset
included). A bell in an OSC terminator is not a bell.

## Animation

After compiling, walk the naga module: collect the uniform-block members
read by `mainImage` and everything it calls (the `AccessIndex` on the
block global, through the `Call` graph). The chain's needs are the union
over its shaders.

- **Time inputs:** `iTime`, `iTimeDelta`, `iFrame`, `iDate`, `iBells`
  (its `w`). A shader that reads none of them can't change between events,
  so it never needs time-driven frames.
- **Event inputs:** `iMouse` (pointer move / press), `iFocus` and
  `iTimeFocus` (focus change), the bell uniforms (BEL). When one of these
  changes and the shader reads it, render a frame even if nothing is
  damaged. Cursor and colour uniforms change only with damage or a full
  clear, which already render.
- **Event duration hint:** a shader may declare
  `const float TOASTTY_EVENT_DURATION = 1.5;` (WGSL:
  `const TOASTTY_EVENT_DURATION: f32 = 1.5;`), read from the module's
  named constants. It means "my time-driven animation only lasts this long
  after an event". It is valid but unused in Ghostty.

Per `[shader] animation`:

| Mode | Shader reads time, declares a duration | Reads time, no duration | No time inputs |
|---|---|---|---|
| `auto` | ~60 Hz for `duration` after an event the shader reads (below), focused or not; else idle | ~60 Hz while focused; idle while unfocused | Idle |
| `always` | ~60 Hz | ~60 Hz | ~60 Hz |
| `never` | Event frames only | Event frames only | Event frames only |

The events that open the `duration` window are a bell, a cursor change
and a focus gain, but only those whose uniforms the chain reads (bell:
`iTimeBell`, `iBellCursor`, `iBellCount`, `iBells`; cursor: the
`iCurrentCursor*` / `iPreviousCursor*` / `iTimeCursorChange` family; focus:
`iFocus`, `iTimeFocus`). A chain that reads none of them is opened by all
three. So a bell-only shader stays idle while you type.

"Idle" means the existing skip-submit gate applies. The chain's
duration is the max over its shaders. Time-driven frames are scheduled
through `Renderer::next_redraw_deadline`, like cursor blink.

## Render path

With no shaders configured, the frame path is **unchanged and
pixel-identical** (the existing snapshot tests guard this).

With a chain:

1. **Content pass** into `scratch_texture`, never directly into the
   swapchain (`render_direct` is off). It draws everything it draws today
   except the UI overlays, and keeps the partial-redraw and `LoadOp` logic.
   The IME preedit stays here, because it is part of the content.
2. **Post passes**, one fullscreen triangle per shader. Pass 0 reads
   scratch. Each later pass reads the previous pass's output (two
   ping-pong textures in the non-sRGB surface format, created only when
   the chain has more than one shader). The last pass writes the
   swapchain image through its non-sRGB view. This replaces the
   `copy_texture_to_texture` blit.
3. **UI pass** on the swapchain image (sRGB view, `LoadOp::Load`), drawn
   every post-processed frame and never shaded: debug overlay, error
   banner, close dialog with its scrim, scroll-to-bottom chip,
   "Copy Markdown" chip.

Scratch needs `TEXTURE_BINDING` and a non-sRGB entry in `view_formats`.
The surface config needs a non-sRGB `view_formats` entry. Switching the
chain on or off forces a full clear.

A frame with no damage but a due post animation (or an event) skips the
content pass's draws and runs only steps 2 and 3.

## Renderer API (`toastty-render`)

```rust
pub struct PostShaderSource { pub path: PathBuf, pub source: String }
pub struct ShaderError { pub path: PathBuf, pub line: Option<u32>, pub column: Option<u32>, pub message: String }  // Display: "path:line:col: message"
pub enum PostAnimation { Auto, Always, Never }

impl Renderer {
    /// Compile and install a chain. All-or-nothing: if any shader fails,
    /// returns every error and keeps the current chain. An empty slice
    /// turns post-processing off.
    pub fn set_post_shaders(&mut self, shaders: &[PostShaderSource]) -> Result<(), Vec<ShaderError>>;
    pub fn set_post_animation(&mut self, mode: PostAnimation);
    pub fn set_focused(&mut self, focused: bool);
    /// Pointer position in physical px (surface space) and button-1 state.
    pub fn set_pointer(&mut self, pos: (f32, f32), button1_down: bool);
}
```

The renderer reads the bell from `Term` itself in `render_term`, so a
BEL with no other output must still produce a `render_term` call (the
binary requests a redraw after any PTY input).

## Testing

- Unit: the prelude keeps user line numbers; `#version` is blanked; error
  locations map back to the user file; analysis finds time inputs through
  helper calls; a static shader is static; the duration constant is read
  in both languages; `iChannel1` gives a clean error.
- GPU (via `test_gpu`): a pass-through shader reproduces the no-shader
  frame; an invert shader inverts; a two-shader chain applies both; with
  no shader, existing snapshots are unchanged.
- App, on Xvfb: screenshots of the demo shader, static and mid-ripple.

## Demo

`examples/shaders/crt-splitflap.glsl`: a static CRT look (barrel
curvature, scanlines, aperture mask, vignette) plus, on each BEL, a
split-flap wave. Starting at the bell's cell, cells flip like a
departure-board tile in a ring that spreads outward. Overlapping bells
come from `iBells`, and `TOASTTY_EVENT_DURATION` keeps it idle between
bells.

## Out of scope

UI for choosing shaders, multi-buffer ShaderToys (`iChannel1+`, feedback),
compute effects, HLSL/Slang/SPIR-V input, visual/audible bell features.
