# M9 — Damage tracking and skip-submit

**Goal.** 30× cheaper CPU at idle (the measurement from decision #7).

**Scope.** Move from full-frame redraw to damage-tracked partial redraw with submit suppression.

`toastty-term` grows a damage set: every `Perform` callback that mutates a cell marks `(row, col)` dirty. After applying a batch of bytes, the dispatcher returns the dirty set to the renderer. The renderer reads `term.damage()` instead of the full grid each frame; rebuilds the instance buffer from just the dirty cells; renders with `LoadOp::Load` so the previous framebuffer is preserved.

The hot path: per frame, if `term.damage().is_empty()` and no animation is due (cursor blink), **skip the surface submit entirely**. No GPU work at all. Decision #7 measured this at ~0.3% CPU idle vs 9.3% for full vsync — 30× cheaper, with the win coming from "did we submit?" rather than "how many instances?" (the per-submit overhead dominates on modern GPUs, so a hybrid policy that switches to full redraw above some threshold has nothing to amortize against).

The mode 2026 `pause_rendering` flag from M8 lives one layer above the damage set: when set, the renderer skips submits regardless. The corrective-dirty flag for timeout flushes is also part of M8.

This is also a good time to make cursor blink work — the renderer keeps a timestamp, and if `now - last_cursor_toggle > blink_interval`, it flips the cursor and marks the cursor's cell dirty.

**Out of scope.** Scroll-region damage optimization (whole-region scroll instead of per-cell). Defer until profiling shows it matters; the per-cell version will already be dramatically faster than full redraw.
