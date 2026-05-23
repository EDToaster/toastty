# M6 — Color and chrome

**Goal.** Light up nearly every TUI by lifting SGR to truecolor and adding the common window-chrome controls.

**Scope.** Extend SGR handling in `toastty-term` to cover 256-color (`CSI 38;5;N m` and `48;5;N m`) and 24-bit truecolor (`CSI 38;2;R;G;B m` and the bg variant). `toastty-render`'s `Theme` already carries a 16-entry palette — extend `Color` to a richer enum (`Default | Indexed16(u8) | Indexed256(u8) | Rgb(u8, u8, u8)`) and resolve at render time. Add OSC 0/1/2 handling to set the window title; `toastty-window` grows a `Window::set_title(&str)` that the dispatcher calls. DECSCUSR (`CSI N q`) switches the cursor shape per-app — the `[cursor]` config section already stores a default; this milestone wires the runtime override.

Also ship a real `terminfo` entry. Create `terminfo/toastty.terminfo` declaring the capabilities we actually support (256-color, alt screen, bracketed paste once M7 lands, etc.). Document `tic -x terminfo/toastty.terminfo` as the install step in the README. Until then most apps will fall back to `xterm-256color` which is close-enough but imprecise.

**Why first.** Each piece is small (~200 LoC each), independent, and breaks 90% of "this looks weird in toastty" reports. After M6, vim/tmux/htop/btop all look the way they're supposed to.

**Out of scope.** Hyperlinks (M10), palette query/set (M10), cursor blink (the config flag is stored but rendering blink needs an animation tick that fits more naturally alongside M9's damage-tracking work).
