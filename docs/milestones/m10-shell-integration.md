# M10 — Shell integration

**Goal.** Shell snippets light up. Clickable URLs. Hot-swappable themes. The terminal feels like part of the shell, not just a dumb tty.

**Scope.** Five OSCs and the platform glue around them:

- **OSC 7 — current working directory.** Apps emit `\x1b]7;file://host/path\x1b\\` after every `cd`. Store the cwd in `Term`; expose via `Term::cwd()`. A future "open new tab" feature inherits the cwd from this getter.
- **OSC 8 — hyperlinks.** Apps emit `\x1b]8;;URL\x1b\\text\x1b]8;;\x1b\\` to mark a range of cells as a clickable hyperlink. Store per-cell hyperlink IDs (interned in a `Term`-owned table — architecture.md flags this as the right approach since the URL is repeated across cells). The renderer underlines hyperlinked cells; the binary opens the URL when the user clicks (via `webbrowser` crate or `open` on macOS).
- **OSC 52 — clipboard read/write.** Wire to the platform clipboard via `arboard`. Gate write-from-PTY behind a `[security] osc_52_write = false` config flag by default — apps writing to your clipboard without consent is a known attack surface. Read remains opt-in too.
- **OSC 133 — semantic prompts.** Apps mark prompt boundaries (A=start of prompt, B=end of prompt, C=start of command, D=command finished). Store boundaries in `Term`; expose them for a future jump-to-prev-prompt keybind, command-status indicators in the tab strip, and "rerun last command" features.
- **OSC 4 — palette query/set.** Apps query (e.g. `\x1b]4;1;?\x1b\\`) or override individual palette entries at runtime. Useful for theme-swap-without-restart.

Ship shell integration snippets in `share/shell-integration/`: `bash.sh`, `zsh.sh`, `fish.fish` that emit OSC 133 + OSC 7 from prompt hooks. README documents how to source them.

**Out of scope.** The jump-to-prev-prompt UX itself (that's a keybind, comes naturally with M9's redraw policy and the eventual binding system). OSC 9;4 ConEmu progress reports — useful but lower priority; queue for M12+.
