# Decision: Window + Input Layer

**Slug:** `window-input`
**Date:** 2026-05-22
**Status:** Recommendation — adopt `winit 0.30.13` behind a thin internal seam.

## Verdict

**Adopt `winit = 0.30.13` (latest stable) — but wrap it.**

Build a small `toastty-window` crate that owns the `winit::application::ApplicationHandler` impl and emits a toastty-specific event enum the rest of the codebase consumes. Reasons:

1. winit covers ~90% of what a terminal needs (window, focus, modifiers, IME, hi-res scroll, scale factor, redraw-on-demand) and is the same layer Alacritty ships on. We do not need to hand-roll a per-platform window stack like WezTerm does.
2. The 10% gap is sharp and protocol-relevant (Caps Lock / Num Lock not exposed; macOS IME couples dead keys to preedit; Wayland redraw cadence is fiddly). A wrapper lets us paper over those gaps and lets us swap backends later (raw AppKit/Wayland) without churning the parser/dispatcher.
3. winit `0.31.0-beta.2` (Nov 2025) changes the API materially (`Box<dyn Window>`, `can_create_surfaces`, `PointerButton`, `SurfaceResized`). Pin to 0.30.x and plan a migration; do not adopt the beta.

The prototype in `prototypes/window-input/` exercises the surface area and was smoke-tested on macOS (Retina, scale_factor=2). Build is clean against `winit = "=0.30.13"` with `rwh_06`, `x11`, `wayland`, `wayland-dlopen`, `wayland-csd-adwaita`.

## What the prototype proved

| Concern | Outcome |
| --- | --- |
| Open a window + log keys with modifiers | OK. `WindowEvent::ModifiersChanged(Modifiers)` carries both unified `ModifiersState` and L/R-disambiguated `lshift_state()` / `rshift_state()` / etc. |
| Key press / repeat / release | OK. `KeyEvent { state, repeat, .. }` — `repeat: bool` is delivered alongside `ElementState::Pressed`. Release fires as `ElementState::Released`. All three required by kitty progressive-enhancement flag `Report event types` are present. |
| Physical + logical + text | OK. `KeyEvent.physical_key: PhysicalKey` (W3C `KeyCode`), `logical_key: Key`, `text: Option<SmolStr>`. Plus the platform extension trait `KeyEventExtModifierSupplement` exposes `text_with_all_modifiers()` and `key_without_modifiers()` on macOS, Windows, X11, Wayland, Orbital. |
| Ctrl+a vs Ctrl+Shift+a disambiguation | OK. `key_without_modifiers()` returns `"a"` in both, but `ModifiersState::shift_key()` differs — exactly the bit kitty protocol needs in the modifier mask. The prototype prints a `kitty-demo` line proving this. |
| IME composition | Partial. `Ime::{Enabled, Preedit(text, cursor), Commit(text), Disabled}` are delivered after `Window::set_ime_allowed(true)`. Per-window only; no per-cell input. See sharp edges. |
| Dead keys | Platform-dependent. `Key::Dead(Option<char>)` exists as a logical_key variant, but on macOS dead keys route through IME (see sharp edges). |
| Scale factor / DPI | OK. `ScaleFactorChanged { scale_factor: f64, inner_size_writer }` plus `Resized(PhysicalSize<u32>)`. Window-level uses `wp-fractional-scale` on Wayland since 0.29.3 (fixes #3183). |
| Hi-res scroll | OK. `MouseScrollDelta::PixelDelta(PhysicalPosition<f64>)` for trackpads, `LineDelta(f32, f32)` for notched wheels. Both with `TouchPhase`. |
| Focus in/out | OK. `WindowEvent::Focused(bool)`. Verified on macOS. |
| Repaint on demand | OK. With `ControlFlow::Wait` + `Window::request_redraw()`, only `RedrawRequested` fires on real events. The prototype was idle for >1s with zero redraws after startup. Caveat: Wayland has known gotchas (see sharp edges). |

## Sharp edges (ranked by pain)

### 1. Caps Lock and Num Lock are NOT exposed as modifiers — kitty protocol gap

`winit::keyboard::ModifiersState` only carries `SHIFT | CONTROL | ALT | SUPER`. The kitty keyboard protocol modifier mask requires `caps_lock` (bit 6) and `num_lock` (bit 7).

Direct evidence — `alacritty/src/input/keyboard.rs:689`:

```rust
// NOTE: Kitty protocol defines additional modifiers to what is present here, like
// Capslock, but it's not a modifier as per winit.
```

Alacritty ships incomplete kitty modifier reporting because of this. WezTerm avoids the issue by reading platform LED state directly (`window/src/os/x11/keyboard.rs`, `window/src/os/macos/window.rs`, `window/src/os/windows/window.rs`).

**Recommendation for toastty:** in `toastty-window`, augment winit's events with a per-platform LED reader:

- Linux X11: `XkbGetIndicatorState`
- Linux Wayland: read XKB state via `xkbcommon` (winit already links it for keymap handling — we may be able to query the existing state).
- macOS: `IOHIDManager` LED element, or `CGEventSourceFlagsState(.combinedSessionState).contains(.maskAlphaShift)` for caps lock (no public num-lock API on Mac, but Mac keyboards rarely have a physical Num Lock).
- Windows (future): `GetKeyState(VK_CAPITAL)` / `VK_NUMLOCK`.

This is the single most important wrapper-layer responsibility.

### 2. macOS dead keys go through IME, not `Key::Dead`

On macOS, pressing Option+E (or any layout's dead key) does **not** deliver a `KeyEvent { logical_key: Key::Dead(...) }`. Instead, AppKit's `NSTextInputClient` fires, and winit translates that into `Ime::Preedit` followed by `Ime::Commit` once the next key resolves the composition. winit docs state explicitly: *"macOS: IME must be enabled to receive text-input where dead-key sequences are combined."*

Implication: a terminal that disables IME (because the focused TUI does its own input handling, e.g. vim in normal mode) will silently break Option+E typing on macOS. winit issue #2651 documents a worse failure mode with custom layouts where modifier handling gets stuck.

Alacritty's workaround (`alacritty/src/input/keyboard.rs:33`, `display/window.rs:445`): track an `ImeInhibitor` set; only call `set_ime_allowed(false)` when *no* inhibitor wants it off. They keep IME on by default specifically so dead keys work.

**Recommendation for toastty:** mirror Alacritty's "IME on by default, with named inhibitors" pattern. When kitty keyboard protocol's `Report associated text` flag is on, the PTY consumer wants text from `Ime::Commit`, not from raw `KeyEvent.text`. Wire `Ime::Preedit` text into the overlay renderer (Alacritty draws preedit underlined at the cursor) and ship `Ime::Commit` text into the PTY as a chunk. Do not try to feed dead-key composition through `KeyEvent.text` on macOS — it will not be there.

### 3. Wayland: `RedrawRequested` cadence is platform-quirky

winit issue #2609: during interactive resize, Wayland holds back `RedrawRequested` until resizing ends, which makes a `ControlFlow::Wait` app look frozen during resize. Winit issue #1619: with `ControlFlow::Wait`, `request_redraw()` sometimes does not produce a `RedrawRequested` until *some other* event arrives. Wayland also bundles a frame-callback model that conflicts with winit's redraw abstraction in subtle ways.

**Recommendation for toastty:** on Wayland, request a redraw on `Resized` *and* on `ScaleFactorChanged`, and additionally drive a short-lived `ControlFlow::WaitUntil` (50–100ms after a resize event) to force a paint even if winit doesn't deliver `RedrawRequested`. Alacritty (`event.rs:486-489`) already uses `ControlFlow::WaitUntil` with a scheduler — adopt the same pattern.

### 4. Wayland fractional scaling — fine for window, careful with monitor handles

`Window::scale_factor()` uses `wp-fractional-scale-v1` since winit 0.29.3 and returns the correct fractional value (e.g. 1.25, 1.5). However, `MonitorHandle::scale_factor()` historically reported the integer fallback (winit issue #3183, fixed in 0.29.3 but worth re-verifying on the Linux box). For text crispness on a 1.5x display, we must:

1. Read `Window::scale_factor()` (NOT `MonitorHandle::scale_factor()`) when sizing surfaces.
2. Create the wgpu surface at `physical_size` and rasterize glyphs at the actual physical scale — no rounding to nearest integer.
3. On `ScaleFactorChanged`, recreate the swapchain and re-rasterize atlases at the new factor.

The prototype's `scale` and `resize` log lines show what we have to plumb through.

### 5. macOS Character Viewer / emoji palette is silently broken when IME is off

winit issue #3342: the macOS Character Viewer (fn+space) inserts via `insertText:replacementRange:`, which winit only forwards to the app when IME is currently active. Result: if a user opens emoji picker while IME has not been triggered this session, nothing gets typed. Workaround on the winit side is to type a diacritic first.

**Recommendation:** if IME is always allowed (per #2 above), this works. Make sure we never globally disable IME, only inhibit it transiently when a TUI explicitly opts out via a known sequence (e.g. cursor keys mode + alternate screen). Add a known-issue note in user docs.

### 6. macOS IME ignores `_selected_range` / `_replacement_range`

winit issue #3617: nested-preedit IMEs like Traditional Chinese Zhuyin won't fully work because winit's `NSTextInputClient` impl drops these range parameters. For Japanese/simple Chinese this is fine; Zhuyin users will see degraded composition. Outside our control short of patching winit.

### 7. `LineDelta` magnitude varies wildly across platforms

`MouseScrollDelta::LineDelta` is "lines" but the scale is platform-defined. Native Linux X11 typically gives 1.0–3.0 per notch, macOS gives small fractional values (it's actually a smoothed pixel delta exposed as lines), Web/winit-on-wasm gives 100+. There is no portable "one notch = one tick". Alacritty multiplies by a configurable `scroll.multiplier` and quantizes.

**Recommendation for toastty:** treat `LineDelta` as an opaque rate, apply our own multiplier, and *prefer* `PixelDelta` when both are available (Wayland exposes both via `wl_pointer.axis_value120` in some compositors). Send `PixelDelta` through to SGR mouse with our own line-quantization step.

### 8. Synthetic key events on focus changes

`WindowEvent::KeyboardInput { is_synthetic, .. }` — on focus loss winit synthesizes `Released` events for keys it believes were held. Useful, but if we also report key releases to a kitty-protocol app, we must NOT report synthetic releases (the app didn't see a real release; reporting one will desync its modifier model). Filter on `is_synthetic` at the wrapper seam.

### 9. winit 0.31 beta is a moving target

Context7 docs already show the beta API. Pinning `=0.30.13` (exact) is mandatory; a `^0.30` resolver could pick up 0.30.14 with breaking changes (winit has been known to ship non-strict semver minor bumps). Plan a 0.31 migration once it stabilizes, behind the same wrapper.

## Surprises

1. **WezTerm does not use winit at all.** It hand-rolls per-platform window code (`window/src/os/{macos,wayland,x11,windows}/`) precisely so it can reach keyboard LEDs, control IME timing, and own the Wayland frame callback. That's a real-world signal about how far winit gets you. We can stay on winit by accepting Alacritty-class compromises (no caps-lock bit in kitty mods is fine for now), and we keep the option to follow WezTerm later.
2. **`text_with_all_modifiers()` is the right field for terminal output, not `text`.** `KeyEvent.text` deliberately omits Ctrl, so Ctrl+A would give text `"a"`, not `"\x01"`. The extension trait method gives the OS-cooked text including Ctrl. Most winit examples use `text`; do not copy them blindly. Alacritty uses `text_with_all_modifiers()` everywhere (`keyboard.rs:39`, `:262`, `:312`).
3. **No KeyboardInput during IME preedit.** While an IME has an active composition, winit suppresses `WindowEvent::KeyboardInput` and only delivers `Ime::Preedit`/`Ime::Commit`. This is correct behaviour but it means our kitty-keyboard encoder needs a "what's the IME doing" gate, and we can't shortcut by listening to KeyboardInput alone.

## Recommended wrapping seam

```rust
// crates/toastty-window/src/lib.rs
pub enum InputEvent {
    Key(ToasttyKey),         // physical + logical + text + repeat + state + LED-augmented mods
    ImePreedit { text: String, cursor: Option<(usize, usize)> },
    ImeCommit  { text: String },
    Mouse(MouseEvent),       // includes pixel-quantized scroll
    Focus(bool),
    Resize { physical: (u32, u32), scale: f64 },
    RedrawRequested,
}

pub struct ToasttyKey {
    pub physical: winit::keyboard::PhysicalKey,
    pub logical:  winit::keyboard::Key,
    pub unmodded: winit::keyboard::Key,   // from KeyEventExtModifierSupplement
    pub text:     Option<String>,          // text_with_all_modifiers()
    pub mods:     ToasttyMods,             // includes caps_lock, num_lock from platform LED
    pub state:    ElementState,
    pub repeat:   bool,
    pub is_synthetic: bool,
}
```

Everything above the seam (the kitty keyboard handler, the SGR mouse handler, the cell/preedit overlay) becomes winit-independent. If we ever swap to raw AppKit / `smithay-client-toolkit`, the dispatcher does not care.

## References

- winit 0.30.13 docs: <https://docs.rs/winit/0.30.13/winit/>
- Alacritty keyboard impl: `alacritty/src/input/keyboard.rs` (`build_sequence`, `SequenceModifiers::from`)
- WezTerm window crate (non-winit, for contrast): `wezterm/window/src/os/`
- Sharp-edge issues referenced: rust-windowing/winit #2651, #3342, #3617, #3183, #1619, #2609, #3493, #3551
- Kitty keyboard protocol: <https://sw.kovidgoyal.net/kitty/keyboard-protocol/>
