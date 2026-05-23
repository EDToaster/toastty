# M13 — User shaders and post-process

**Goal.** Power-user differentiator. ShaderToy port compatibility was measured at 100% on a 14-shader corpus in decision #8 — bring that to users.

**Scope.** Post-process fragment shader stage after the cell pass, slotted into the existing render graph as pass 4. User supplies a shader file path in config (or via a CLI arg for live experimentation).

**Dual-path shader pipeline.** WGSL primary — written by the user as `fn main(...) -> @location(0) vec4<f32>`. GLSL secondary via `naga`'s GLSL frontend — accepts ShaderToy-style `void mainImage(out vec4 fragColor, in vec2 fragCoord)`, runs through a ~50-LoC shim (`glsl_shim.rs` from decision #8's prototype is the reference) that synthesizes an entry point, translates via `naga` at runtime to whatever the backend wants (SPIR-V/MSL/HLSL).

**Uniforms.** Mirror ShaderToy conventions where sensible: `iTime` (seconds), `iFrame` (counter), `iResolution` (`vec3` with width/height/aspect), `iCursor` (cell coords). Plus a previous-framebuffer texture binding so effects can read the pre-shader output.

**Hot reload.** `notify` crate watches the shader file. On change, recompile via `naga`. On compile failure, surface diagnostics in the status line (or stderr) and **keep the last-good pipeline live** so the renderer never crashes from a bad edit. The Ghostty/Kitty contract: a typo should never take the terminal down.

**Sandbox.** None needed — fragment shaders can't escape the GPU. Resource limits via wgpu's validation.

**The shim's known gotcha.** From decision #8: `#define`-based ShaderToy uniform aliases break naga's swizzle parsing (`#define iMouse (vec4(...))` then `iMouse.zw` fails). The shim declares `iMouse` as a top-level `vec4` and copies from the UBO inside the synthesized `main()` — already worked out in the prototype.

**Out of scope.** A UI for shader selection (just a file path in config). Multi-pass shaders. Compute-shader effects.
