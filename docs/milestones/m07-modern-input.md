# M7 — Modern input

**Goal.** Make tmux, neovim, and helix feel right.

**Scope.** Four protocols, ordered by impact:

- **Bracketed paste** (DECSET 2004). When the user pastes text from the system clipboard, the binary wraps it in `\x1b[200~ ... \x1b[201~` before writing to the PTY. Shells and editors then know "this is paste, not typing" and don't trigger their normal text-completion / auto-indent behavior. Handler in `toastty-protocols`; clipboard read in the binary via `arboard` or a thin platform shim.
- **Mouse reporting** (SGR mode 1006, plus 1000 for clicks-only and 1002 for click+drag). Translate winit's `MouseInput` and `Wheel` events into the corresponding CSI sequences and write them to the PTY. Apps opt in via the mode bits — toastty just emits when an app has enabled mouse mode.
- **Focus events** (mode 1004). Convert winit's `Focus(true/false)` into `\x1b[I` / `\x1b[O`. Used by prompt themes that dim when blurred.
- **Kitty keyboard protocol.** The big one. `CSI u` with progressive enhancement flags (disambiguate, report event types, alt keys, all keys as escape codes, associated text), pushed/popped on a stack via `CSI > flags u` / `CSI < u`. Replaces the basic VT encoding from M5 when an app opts in. Required for proper `Ctrl+Shift+a` ≠ `Ctrl+A` disambiguation, separate press/release events, and modifier-aware function keys.

Also fix the Caps/Num Lock LED-reading TODO from M4a — required for kitty keyboard correctness. Read OS LED state per platform (macOS via `IOKit`, Linux via `evdev`).

**Out of scope.** Mouse over OSC 1006 in full (just clicks/drags/scroll, not motion-without-buttons). System-clipboard reads triggered by OSC 52 (that's M10). Mode 2048 in-band resize (M8 — pairs more naturally with synchronized output).
