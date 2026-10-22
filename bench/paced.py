#!/usr/bin/env python3
"""Paced full-screen repaints, run *inside* toastty by `run.py`.

Emits one full-screen synchronized-output frame every `--interval-ms`
(20 ms by default), sized to the terminal, then exits. Unlike the
`dense_cells` flood, the frames are paced, so toastty renders (nearly)
every one of them and the per-frame render cost shows up in
`TOASTTY_STATS` `frame_us`.

Modes:

- `text`: every frame is new text (seeded random words, a 256-colour fg
  per word, a few coloured bg runs). Worst case for row shaping: no row
  repeats between frames.
- `cjk`: like `text`, with CJK words and emoji mixed in, so every row
  needs cosmic-text (the shaper's slow path).
- `tui`: an htop/btop-like screen. The layout and most of the text stay
  put; colours cycle and a few numbers change every frame, so every row
  is rewritten (and dirty) but most rows' text is unchanged.

    paced.py --mode text --frames 300 [--interval-ms 20] [--seed 1]
"""

from __future__ import annotations

import argparse
import os
import random
import sys
import time

WORDS = (
    "lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod "
    "tempor incididunt ut labore et dolore magna aliqua enim ad minim veniam "
    "quis nostrud exercitation ullamco laboris nisi aliquip ex ea commodo "
    "consequat duis aute irure in reprehenderit voluptate velit esse cillum "
    "fugiat nulla pariatur excepteur sint occaecat cupidatat non proident"
).split()


CJK_WORDS = "你好 世界 日本語 テキスト 한글 漢字 中文 进度 🚀 👍 café naïve".split()


def width(word: str) -> int:
    # Every non-Latin char in CJK_WORDS is a wide (2-cell) char.
    return sum(2 if ord(ch) > 0x2FFF else 1 for ch in word)


def text_frame(rng: random.Random, cols: int, rows: int, cjk: bool = False) -> str:
    out = []
    for r in range(rows):
        out.append(f"\x1b[{r + 1};1H")
        w = 0
        while w < cols:
            word = rng.choice(CJK_WORDS if cjk and rng.random() < 0.3 else WORDS)
            if width(word) > cols - w:
                # Truncate (ASCII) or pad out (a wide word that won't fit).
                word = word[: cols - w] if word.isascii() else "x" * (cols - w)
            fg = rng.randrange(256)
            if rng.random() < 0.1:
                out.append(f"\x1b[38;5;{fg};48;5;{rng.randrange(256)}m{word}\x1b[49m")
            else:
                out.append(f"\x1b[38;5;{fg}m{word}")
            w += width(word)
            if w < cols:
                out.append(" ")
                w += 1
        out.append("\x1b[0m")
    return "".join(out)


def tui_frame(rng: random.Random, cols: int, rows: int, f: int) -> str:
    out = []
    for r in range(rows):
        out.append(f"\x1b[{r + 1};1H")
        if r == 0:
            line = f" uptime 12:{f % 60:02d}  load {f % 7}.{f % 10}{(f * 3) % 10}"
            out.append(f"\x1b[1;37;44m{line[:cols].ljust(cols)}\x1b[0m")
            continue
        # A process-table row: fixed name / command text, a meter that
        # moves on a few rows each frame.
        pid = 1000 + r * 37
        name = WORDS[r % len(WORDS)]
        cmd = " ".join(WORDS[(r + k) % len(WORDS)] for k in range(8))
        busy = (r + f // 10) % 9 == 0
        cpu = (r * 13 + (f if busy else 0)) % 100
        bar_w = 20
        fill = cpu * bar_w // 100
        fg = 16 + (r * 5 + f) % 216
        cells = f"{pid:>7} {name:<12} {cpu:>3}% "
        rest = f" {cmd}"
        tail_w = max(0, cols - len(cells) - bar_w - 2)
        out.append(
            f"\x1b[38;5;{fg}m{cells}\x1b[0m"
            f"[\x1b[38;5;{46 + fill}m{'|' * fill}{' ' * (bar_w - fill)}\x1b[0m]"
            f"\x1b[38;5;{250 - r % 6}m{rest[:tail_w].ljust(tail_w)}\x1b[0m"
        )
    return "".join(out)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--mode", choices=("text", "cjk", "tui"), default="text")
    ap.add_argument("--frames", type=int, default=300)
    ap.add_argument("--interval-ms", type=float, default=20.0)
    ap.add_argument("--seed", type=int, default=1)
    args = ap.parse_args()

    cols, rows = os.get_terminal_size(sys.stdout.fileno())
    rng = random.Random(args.seed)
    w = sys.stdout.buffer
    w.write(b"\x1b[?25l\x1b[2J")
    w.flush()
    # Let the window settle (first frame, any resize) before timing.
    time.sleep(0.5)
    t0 = time.monotonic()
    for f in range(args.frames):
        if args.mode == "tui":
            body = tui_frame(rng, cols, rows, f)
        else:
            body = text_frame(rng, cols, rows, cjk=args.mode == "cjk")
        w.write(("\x1b[?2026h\x1b[H" + body + "\x1b[?2026l").encode())
        w.flush()
        next_t = t0 + (f + 1) * args.interval_ms / 1000
        dt = next_t - time.monotonic()
        if dt > 0:
            time.sleep(dt)
    w.write(b"\x1b[0m\x1b[?25h")
    w.flush()
    time.sleep(0.2)
    return 0


if __name__ == "__main__":
    sys.exit(main())
