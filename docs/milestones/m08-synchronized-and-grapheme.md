# M8 — Synchronized output and grapheme clusters

**Goal.** Flicker-free neovim / tmux, correct emoji widths, reliable resize.

**Scope.** Three modes that compose into the modern terminal core:

- **Mode 2026 — synchronized output (BSU/ESU).** Apps wrap a batch of cell updates in `CSI ? 2026 h` / `CSI ? 2026 l`. The renderer must not display the partial state between them. Implementation: a `pause_rendering` flag in `toastty-term::Modes` set by the handler; the renderer checks it at frame start and skips the frame. **Critical subtlety from decision #7**: if 1s elapses without ESU, force a flush and set `timeout_force_flushed: true` so the next post-ESU frame marks the entire grid dirty for a corrective full redraw — otherwise damage tracking would emit a tiny dirty list and the partial-state flash would persist for one frame. Tested empirically in the original prototype.
- **Mode 2027 — grapheme cluster processing.** Apps declare cluster widths and the terminal honors them rather than `wcwidth()`-ing every codepoint. The pure-function `cluster_width.rs` module already has the snap math; this milestone wires (a) the mode-2027 opt-in in `toastty-protocols`, (b) the multi-cell-cluster case (`cluster_cells > 1`), and (c) cell-grid bookkeeping for clusters that span two cells (one cell holds the cluster, the next is marked as a continuation).
- **Mode 2048 — in-band resize notifications.** Stream-based resize reports replace the SIGWINCH race. When the kernel reports SIGWINCH (or our own resize handler fires), emit the report sequence on the PTY's read side so the app sees it in order with everything else. Currently SIGWINCH races with PTY reads on some setups; mode 2048 fixes that.

**Out of scope.** BiDi (Arabic/Hebrew) layout — cosmic-text supports it, but our cell-grid model needs work to express right-to-left rendering correctly. Reserve for a later milestone once enough users surface it.
