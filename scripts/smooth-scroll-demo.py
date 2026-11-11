#!/usr/bin/env python3
"""Two-pane APC smooth demo. Run inside Toastty; --auto gives a repeatable capture."""

import argparse
import os
import re
import select
import sys
import termios
import time
import tty

ESC = "\x1b"


def apc(body):
    return f"{ESC}_smooth;{body}{ESC}\\"


def draw(position, enabled, moving, caption):
    cols, rows = os.get_terminal_size()
    if cols < 60 or rows < 15:
        sys.stdout.write(f"{ESC}[H{ESC}[2JResize to at least 60 columns by 15 rows.")
        sys.stdout.flush()
        return
    split = cols // 2
    top, bottom = 5, rows - 4
    left, right = 2, split - 3
    line = int(position)
    fraction = int((position - line) * 1000)
    output = [f"{ESC}[?2026h{ESC}[H{ESC}[2J", "" if enabled else apc("off")]

    def text(row, col, value, color=250):
        output.append(f"{ESC}[{row + 1};{col + 1}H{ESC}[38;5;{color}m{value}{ESC}[0m")

    text(1, 2, "TOASTTY / SMOOTH REGIONS", 81)
    text(2, 2, caption[: cols - 4], 245)
    text(4, left, "SCROLLING PANE", 117)
    text(4, split + 2, "FIXED PANE", 150)
    for y in range(top, bottom + 1):
        n = line + y - top
        color = 81 if n % 3 == 0 else 250
        label = f"{n + 1:04}  " + [
            "A region, not the whole screen",
            "Text stays on the logical grid",
            "Pixels move in the renderer",
            "Headers remain stationary",
            "One spare row fills the edge",
            "Redraw and offset commit together",
        ][n % 6]
        text(y, left, label[: right - left], color)
        text(y, split, "|", 238)
        if y < bottom:
            text(y, split + 2, [
                "This pane does not move.",
                "",
                "Protocol: APC smooth / v3",
                "Transport: PTY escape sequences",
                "Frame boundary: mode 2026",
                "",
                "Wheel or j/k: scroll",
                "Space: animate / pause",
                "s: smooth / stepped",
                "q: quit",
            ][(y - top) % 10][: cols - split - 4], 245)
    # The extended form reserves the bottom row and rightmost column.
    if enabled:
        output.append(apc(f"1;{top};{bottom};{left};{right};{line};500;0;{fraction if moving else 0}"))
    text(rows - 2, 2, "FIXED FOOTER  |  APP REQUEST: " + ("SMOOTH" if enabled else "STEPPED") + "  |  one pane moves", 81)
    output.append(f"{ESC}[?2026l")
    sys.stdout.write("".join(output))
    sys.stdout.flush()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--auto", action="store_true")
    parser.add_argument("--caption", default="Precision scrolling without moving the surrounding UI")
    parser.add_argument("--hold", type=float, help="Render a fixed fractional position for inspection")
    args = parser.parse_args()
    fd = sys.stdin.fileno()
    saved = termios.tcgetattr(fd)
    position, enabled, moving = 10.0, True, args.auto
    last = time.monotonic()
    pending = ""
    try:
        tty.setraw(fd)
        sys.stdout.write(f"{ESC}[?1049h{ESC}[?25l{ESC}[?1000h{ESC}[?1006h")
        while True:
            now = time.monotonic()
            if args.hold is not None:
                position = args.hold
            elif moving:
                # Application-driven fractional movement exercises the same
                # clipping path on X11, where XTest only injects wheel notches.
                position = min(500.0, position + (now - last) * 2.0)
            last = now
            draw(position, enabled, moving or args.hold is not None, args.caption)
            ready, _, _ = select.select([fd], [], [], 1 / 60 if moving else 0.1)
            if not ready:
                continue
            pending += os.read(fd, 4096).decode("utf-8", errors="ignore")
            # Replies and mouse packets can be split across reads.
            while pending:
                if pending.startswith(ESC + "[<"):
                    match = re.match(r"\x1b\[<(\d+);(\d+);(\d+)([Mm])", pending)
                    if not match:
                        break
                    button = int(match[1])
                    if button in (64, 65):
                        moving = False
                        position = max(0, min(500, int(position) + (1 if button == 65 else -1)))
                    pending = pending[match.end():]
                    continue
                key, pending = pending[0], pending[1:]
                if key in ("q", "\x03"):
                    return
                if key == " ":
                    moving = not moving
                elif key == "s":
                    enabled = not enabled
                elif key in ("j", "k"):
                    moving = False
                    position = max(0, min(500, int(position) + (1 if key == "j" else -1)))
    finally:
        sys.stdout.write(apc("off") + f"{ESC}[?2026l{ESC}[?1000l{ESC}[?1006l{ESC}[?25h{ESC}[?1049l")
        sys.stdout.flush()
        termios.tcsetattr(fd, termios.TCSADRAIN, saved)


if __name__ == "__main__":
    main()
