# Milestones

Short sketches of the planned milestones beyond the current work. Each doc is a couple of paragraphs covering goal, scope, dependencies on earlier milestones, and what's deferred.

| # | File | Theme |
| --- | --- | --- |
| M6 | [m06-color-and-chrome.md](./m06-color-and-chrome.md) | 256-color + truecolor SGR, window title, cursor shape, terminfo |
| M7 | [m07-modern-input.md](./m07-modern-input.md) | Bracketed paste, mouse, focus, kitty keyboard, Caps/Num Lock |
| M8 | [m08-synchronized-and-grapheme.md](./m08-synchronized-and-grapheme.md) | Mode 2026 / 2027 / 2048 |
| M9 | [m09-damage-tracking.md](./m09-damage-tracking.md) | Damage set + skip-submit (30× cheaper idle) |
| M10 | [m10-shell-integration.md](./m10-shell-integration.md) | OSC 7 / 8 / 52 / 133 / 4 + shell snippets |
| M11 | [m11-image-protocols.md](./m11-image-protocols.md) | Kitty graphics + Sixel |
| M12 | [m12-rgp-3d.md](./m12-rgp-3d.md) | RGP — inline 3D objects |
| M13 | [m13-user-shaders.md](./m13-user-shaders.md) | WGSL + GLSL post-process |

Earlier milestones (M1 — toastty-pty, M2 — toastty-parser, M3 — toastty-term, M4a/M4b — window + render, M4.5 — config, M5 — binary integration) live in the git history rather than as separate docs; their rationale is in `architecture.md` and the decision records.
