#!/usr/bin/env python3
"""toastty benchmark harness.

Runs the release binary on a private Xvfb display (see AGENTS.md,
"Running toastty on a virtual screen") and records, per scenario:

- throughput (MB/s), frames rendered and FPS while catting a fixed file;
- CPU time (user + sys) of the toastty process;
- peak and final RSS;
- frame time (CPU time spent in `render_term`, which includes present);
- output→frame and key→frame latency;
- idle CPU % with and without a blinking cursor;
- startup time to the first frame.

The numbers come from toastty's own `TOASTTY_STATS=<path>` report
(`crates/toastty/src/perf_stats.rs`) plus /proc sampling for the idle
scenarios.

    bench/run.py --bin target/release/toastty --out results.json
    bench/run.py --bin ... --only throughput_ascii,idle_blink
    bench/compare.py base.json new.json

Only the X11 path with software presentation (MESA_VK_WSI_DEBUG=sw) is
exercised, so absolute FPS differs from a real compositor; compare runs
against each other, not against other terminals.
"""

from __future__ import annotations

import argparse
import json
import os
import random
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
CLK_TCK = os.sysconf("SC_CLK_TCK")
SCREEN = (2560, 1440)

# --------------------------------------------------------------------------
# Workloads. Deterministic (seeded) and cached under the work dir.
# --------------------------------------------------------------------------

WORDS = (
    "lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod "
    "tempor incididunt ut labore et dolore magna aliqua enim ad minim veniam "
    "quis nostrud exercitation ullamco laboris nisi aliquip ex ea commodo "
    "consequat duis aute irure in reprehenderit voluptate velit esse cillum "
    "fugiat nulla pariatur excepteur sint occaecat cupidatat non proident"
).split()


def _repeat_to(chunk: bytes, size: int) -> bytes:
    n = size // len(chunk) + 1
    return (chunk * n)[:size]


def gen_ascii(rng: random.Random, size: int) -> bytes:
    lines = []
    total = 0
    while total < 4 << 20:
        width = rng.randint(20, 160)
        line = []
        w = 0
        while w < width:
            word = rng.choice(WORDS)
            line.append(word)
            w += len(word) + 1
        s = " ".join(line) + "\n"
        lines.append(s)
        total += len(s)
    return _repeat_to("".join(lines).encode(), size)


def gen_sgr(rng: random.Random, size: int) -> bytes:
    out = []
    total = 0
    while total < 4 << 20:
        parts = []
        for _ in range(rng.randint(4, 20)):
            word = rng.choice(WORDS)
            k = rng.random()
            if k < 0.4:
                parts.append(f"\x1b[38;5;{rng.randrange(256)}m{word}")
            elif k < 0.7:
                r, g, b = (rng.randrange(256) for _ in range(3))
                parts.append(f"\x1b[38;2;{r};{g};{b}m{word}")
            elif k < 0.85:
                parts.append(f"\x1b[1;4;48;5;{rng.randrange(256)}m{word}\x1b[22;24;49m")
            else:
                parts.append(word)
        s = " ".join(parts) + "\x1b[0m\n"
        out.append(s)
        total += len(s)
    return _repeat_to("".join(out).encode(), size)


def gen_unicode(rng: random.Random, size: int) -> bytes:
    cjk = [chr(c) for c in range(0x4E00, 0x4E00 + 400)]
    greek = [chr(c) for c in range(0x3B1, 0x3C9)]
    emoji = ["😀", "🚀", "🎉", "👍", "🔥", "✨", "🍕", "🐍", "🦀", "👨‍👩‍👧"]
    combining = ["é", "ä", "õ", "ñ"]
    out = []
    total = 0
    while total < 2 << 20:
        parts = []
        for _ in range(rng.randint(5, 25)):
            k = rng.random()
            if k < 0.35:
                parts.append("".join(rng.choice(cjk) for _ in range(rng.randint(1, 6))))
            elif k < 0.5:
                parts.append("".join(rng.choice(greek) for _ in range(rng.randint(2, 8))))
            elif k < 0.6:
                parts.append(rng.choice(emoji))
            elif k < 0.7:
                parts.append(rng.choice(combining))
            else:
                parts.append(rng.choice(WORDS))
        s = " ".join(parts) + "\n"
        out.append(s)
        total += len(s.encode())
    return _repeat_to("".join(out).encode(), size)


def gen_dense(rng: random.Random, cols: int, rows: int, frames: int) -> bytes:
    """vtebench `dense_cells`-like: every cell gets a new fg/bg and glyph,
    one synchronized-output frame at a time."""
    chars = [chr(c) for c in range(0x21, 0x7F)]
    out = []
    for f in range(frames):
        buf = ["\x1b[?2026h\x1b[H"]
        for r in range(rows):
            buf.append(f"\x1b[{r + 1};1H")
            for c in range(cols):
                fg = (r * 7 + c + f) % 256
                bg = (r + c * 3 + f * 5) % 256
                buf.append(f"\x1b[38;5;{fg};48;5;{bg}m{chars[(r + c + f) % len(chars)]}")
        buf.append("\x1b[0m\x1b[?2026l")
        out.append("".join(buf))
    return "".join(out).encode()


def gen_scroll(lines: int) -> bytes:
    return "".join(f"{i}\n" for i in range(1, lines + 1)).encode()


def gen_wide_lines(rng: random.Random, cols: int, lines: int) -> bytes:
    """Full-width coloured lines, to fill the scrollback with real cells."""
    out = []
    for _ in range(lines):
        parts = []
        w = 0
        while w < cols - 12:
            word = rng.choice(WORDS)
            parts.append(f"\x1b[38;5;{rng.randrange(256)}m{word}")
            w += len(word) + 1
        out.append(" ".join(parts) + "\x1b[0m\n")
    return "".join(out).encode()


def workloads(work: Path) -> dict[str, Path]:
    d = work / "data"
    d.mkdir(parents=True, exist_ok=True)
    specs = {
        "ascii.txt": lambda: gen_ascii(random.Random(1), 64 << 20),
        "sgr.txt": lambda: gen_sgr(random.Random(2), 32 << 20),
        "unicode.txt": lambda: gen_unicode(random.Random(3), 16 << 20),
        "dense.txt": lambda: gen_dense(random.Random(4), 128, 42, 400),
        "dense_big.txt": lambda: gen_dense(random.Random(5), 256, 73, 150),
        "scroll.txt": lambda: gen_scroll(3_000_000),
        "scrollback.txt": lambda: gen_wide_lines(random.Random(6), 128, 30_000),
    }
    paths = {}
    for name, make in specs.items():
        p = d / name
        if not p.exists():
            print(f"  generating {name}", file=sys.stderr)
            p.write_bytes(make())
        paths[name] = p
    return paths


# --------------------------------------------------------------------------
# Virtual display + toastty process control.
# --------------------------------------------------------------------------


class Display:
    """A private Xvfb on the first free display number >= 99."""

    def __init__(self, work: Path):
        n = 99
        while Path(f"/tmp/.X{n}-lock").exists() or Path(f"/tmp/.X11-unix/X{n}").exists():
            n += 1
        self.name = f":{n}"
        self.log = open(work / "xvfb.log", "wb")
        self.proc = subprocess.Popen(
            ["Xvfb", self.name, "-screen", "0", f"{SCREEN[0]}x{SCREEN[1]}x24", "-nolisten", "tcp"],
            stdout=self.log,
            stderr=subprocess.STDOUT,
        )
        for _ in range(100):
            if Path(f"/tmp/.X11-unix/X{n}").exists():
                break
            time.sleep(0.05)
        else:
            raise RuntimeError("Xvfb did not start")

    def xdotool(self, *args: str) -> str:
        env = dict(os.environ, DISPLAY=self.name)
        return subprocess.run(
            ["xdotool", *args], env=env, capture_output=True, text=True, check=False
        ).stdout.strip()

    def close(self) -> None:
        if self.proc.poll() is None:
            self.proc.send_signal(signal.SIGTERM)
            try:
                self.proc.wait(5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        self.log.close()


def write_config(work: Path, binary: Path, *, width: int, height: int, blink: bool) -> Path:
    cfg_home = work / f"cfg_{width}x{height}_{'blink' if blink else 'steady'}"
    (cfg_home / "toastty").mkdir(parents=True, exist_ok=True)
    text = subprocess.run(
        [str(binary), "--print-default-config"], capture_output=True, text=True, check=True
    ).stdout
    out = []
    section = None
    for line in text.splitlines():
        s = line.strip()
        if s.startswith("["):
            section = s
        if section == "[window]" and s.startswith("width ="):
            line = f"width = {width}"
        elif section == "[window]" and s.startswith("height ="):
            line = f"height = {height}"
        elif section == "[cursor]" and s.startswith("blink ="):
            line = f"blink = {'true' if blink else 'false'}"
        out.append(line)
    (cfg_home / "toastty" / "config.toml").write_text("\n".join(out) + "\n")
    return cfg_home


class Toastty:
    def __init__(self, display: Display, binary: Path, cfg_home: Path, work: Path, cmd: list[str]):
        self.stats_path = work / f"stats_{time.monotonic_ns()}.json"
        env = {
            k: v
            for k, v in os.environ.items()
            if k not in ("WAYLAND_DISPLAY", "TOASTTY_DEBUG", "TOASTTY_PTY_LOG")
        }
        env.update(
            DISPLAY=display.name,
            MESA_VK_WSI_DEBUG="sw",
            XDG_CONFIG_HOME=str(cfg_home),
            HISTFILE="/dev/null",
            TOASTTY_STATS=str(self.stats_path),
            RUST_LOG="warn",
        )
        self.log = open(work / "toastty.log", "ab")
        self.launch_ns = time.time_ns()
        self.proc = subprocess.Popen(
            [str(binary), "--", *cmd], env=env, stdout=self.log, stderr=subprocess.STDOUT
        )
        self.display = display

    def window(self, timeout: float = 10.0) -> str:
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            w = self.display.xdotool("search", "--class", "toastty")
            if w:
                return w.splitlines()[0]
            time.sleep(0.05)
        raise RuntimeError("toastty window did not appear")

    def wait(self, timeout: float) -> dict:
        try:
            self.proc.wait(timeout)
        except subprocess.TimeoutExpired:
            self.kill()
            raise RuntimeError(f"toastty did not exit within {timeout}s")
        self.log.close()
        if not self.stats_path.exists():
            raise RuntimeError("toastty exited without writing TOASTTY_STATS")
        stats = json.loads(self.stats_path.read_text())
        self.stats_path.unlink()
        stats["launch_ns"] = self.launch_ns
        return stats

    def proc_sample(self) -> tuple[int, int]:
        """(utime+stime ticks, VmRSS kB) of the toastty process."""
        pid = self.proc.pid
        stat = Path(f"/proc/{pid}/stat").read_text()
        f = stat.rsplit(")", 1)[1].split()
        ticks = int(f[11]) + int(f[12])
        rss = 0
        for line in Path(f"/proc/{pid}/status").read_text().splitlines():
            if line.startswith("VmRSS:"):
                rss = int(line.split()[1])
        return ticks, rss

    def kill(self) -> None:
        if self.proc.poll() is None:
            self.proc.send_signal(signal.SIGTERM)
            try:
                self.proc.wait(5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        if not self.log.closed:
            self.log.close()


# --------------------------------------------------------------------------
# Metrics helpers.
# --------------------------------------------------------------------------


def pct(v: list[float], p: float) -> float:
    if not v:
        return float("nan")
    s = sorted(v)
    k = min(len(s) - 1, max(0, round(p / 100 * (len(s) - 1))))
    return s[k]


def summarize_run(st: dict) -> dict:
    active_us = max(1, st["elapsed_us"] - max(0, st["first_bytes_us"]))
    frame_ms = [x / 1000 for x in st["frame_us"]]
    return {
        "active_s": active_us / 1e6,
        "mb_per_s": st["pty_bytes"] / active_us,  # bytes/us == MB/s
        "frames": st["frames"],
        "fps": st["frames"] / (active_us / 1e6),
        "cpu_s": (st["utime_ticks"] + st["stime_ticks"]) / CLK_TCK,
        "parse_s": st["parse_us"] / 1e6,
        "render_s": sum(st["frame_us"]) / 1e6,
        "peak_rss_mb": st["vm_hwm_kb"] / 1024,
        "final_rss_mb": st["vm_rss_kb"] / 1024,
        "frame_ms_p50": pct(frame_ms, 50),
        "frame_ms_p99": pct(frame_ms, 99),
        "pty_events": st["pty_events"],
        "first_frame_ms": st["first_frame_us"] / 1000,
        "launch_to_frame_ms": (
            st["start_unix_ns"] + st["first_frame_us"] * 1000 - st["launch_ns"]
        )
        / 1e6,
        "output_latency_ms_p50": pct([x / 1000 for x in st["output_latency_us"]], 50),
        "output_latency_ms_p99": pct([x / 1000 for x in st["output_latency_us"]], 99),
        "key_latency_ms_p50": pct([x / 1000 for x in st["key_latency_us"]], 50),
        "key_latency_ms_p99": pct([x / 1000 for x in st["key_latency_us"]], 99),
        "key_samples": len(st["key_latency_us"]),
    }


def median_runs(runs: list[dict]) -> dict:
    out = {}
    for k in runs[0]:
        vals = [r[k] for r in runs]
        out[k] = statistics.median(vals)
    out["runs"] = len(runs)
    return out


# --------------------------------------------------------------------------
# Scenarios.
# --------------------------------------------------------------------------


class Bench:
    def __init__(self, binary: Path, work: Path, reps: int):
        self.binary = binary
        self.work = work
        self.reps = reps
        self.data = workloads(work)
        self.display = Display(work)
        self.cfg = {}

    def config(self, width=1280, height=800, blink=True) -> Path:
        key = (width, height, blink)
        if key not in self.cfg:
            self.cfg[key] = write_config(self.work, self.binary, width=width, height=height, blink=blink)
        return self.cfg[key]

    def launch(self, cmd: list[str], **cfg) -> Toastty:
        return Toastty(self.display, self.binary, self.config(**cfg), self.work, cmd)

    def cat(self, name: str, **cfg) -> dict:
        runs = []
        for _ in range(self.reps):
            t = self.launch(["cat", str(self.data[name])], **cfg)
            runs.append(summarize_run(t.wait(300)))
        return median_runs(runs)

    # Each scenario returns a flat dict of metrics.

    def throughput_ascii(self):
        return self.cat("ascii.txt")

    def throughput_sgr(self):
        return self.cat("sgr.txt")

    def throughput_unicode(self):
        return self.cat("unicode.txt")

    def throughput_scroll(self):
        return self.cat("scroll.txt")

    def dense_cells(self):
        return self.cat("dense.txt")

    def dense_cells_big(self):
        return self.cat("dense_big.txt", width=2560, height=1400)

    def scrollback_memory(self):
        return self.cat("scrollback.txt")

    def startup(self):
        runs = []
        for _ in range(max(3, self.reps)):
            t = self.launch(["sleep", "1.5"])
            runs.append(summarize_run(t.wait(30)))
        r = median_runs(runs)
        return {
            "first_frame_ms": r["first_frame_ms"],
            "launch_to_frame_ms": r["launch_to_frame_ms"],
            "idle_rss_mb": r["final_rss_mb"],
            "peak_rss_mb": r["peak_rss_mb"],
            "cpu_s": r["cpu_s"],
            "runs": r["runs"],
        }

    def _idle(self, blink: bool, seconds: float = 10.0):
        t = self.launch(["bash", "--norc", "--noprofile"], blink=blink)
        try:
            t.window()
            time.sleep(3.0)
            t0, _ = t.proc_sample()
            m0 = time.monotonic()
            rss = []
            while time.monotonic() - m0 < seconds:
                time.sleep(0.25)
                rss.append(t.proc_sample()[1])
            t1, _ = t.proc_sample()
            dt = time.monotonic() - m0
        finally:
            t.kill()
        return {
            "cpu_pct": 100 * (t1 - t0) / CLK_TCK / dt,
            "rss_mb": statistics.median(rss) / 1024,
        }

    def idle_blink(self):
        return self._idle(True)

    def idle_steady(self):
        return self._idle(False)

    def key_latency(self):
        t = self.launch(["cat"], blink=False)
        try:
            w = t.window()
            self.display.xdotool("windowfocus", "--sync", w)
            time.sleep(1.0)
            text = "the quick brown fox jumps over the lazy dog " * 3
            for ch in text:
                key = "space" if ch == " " else ch
                self.display.xdotool("key", key)
                time.sleep(0.04)
            self.display.xdotool("key", "Return")
            time.sleep(0.2)
            self.display.xdotool("key", "ctrl+d")
            r = summarize_run(t.wait(30))
        finally:
            t.kill()
        return {
            k: r[k] for k in ("key_latency_ms_p50", "key_latency_ms_p99", "key_samples")
        }

    def _scroll(self, **cfg):
        lines = 20000
        t = self.launch(["sh", "-c", f"seq 1 {lines}; sleep 9"], blink=False, **cfg)
        try:
            w = t.window()
            self.display.xdotool("windowfocus", "--sync", w)
            time.sleep(2.0)
            self.display.xdotool("mousemove", "--window", w, "300", "300")
            # ~3 s of continuous wheel up, then down.
            for button in ("4", "5"):
                self.display.xdotool("click", "--repeat", "50", "--delay", "30", button)
            st = t.wait(30)
        finally:
            t.kill()
        # Skip the frames that rendered the `seq` output itself: keep only
        # the last N frames, those during the wheel phase.
        frame_ms = [x / 1000 for x in st["frame_us"]]
        tail = frame_ms[-300:] if len(frame_ms) > 300 else frame_ms
        return {
            "frames": st["frames"],
            "frame_ms_p50": pct(tail, 50),
            "frame_ms_p99": pct(tail, 99),
            "frame_ms_mean": statistics.fmean(tail) if tail else float("nan"),
            "cpu_s": (st["utime_ticks"] + st["stime_ticks"]) / CLK_TCK,
        }

    def scroll_wheel(self):
        return self._scroll()

    def scroll_wheel_big(self):
        return self._scroll(width=2560, height=1400)

    def close(self):
        self.display.close()


SCENARIOS = [
    "startup",
    "idle_blink",
    "idle_steady",
    "throughput_ascii",
    "throughput_sgr",
    "throughput_unicode",
    "throughput_scroll",
    "dense_cells",
    "dense_cells_big",
    "scrollback_memory",
    "key_latency",
    "scroll_wheel",
    "scroll_wheel_big",
]


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--bin", required=True, type=Path)
    ap.add_argument("--out", required=True, type=Path)
    ap.add_argument("--work", type=Path, default=None, help="scratch dir (workloads are cached here)")
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--only", default="", help="comma-separated scenario names")
    args = ap.parse_args()

    binary = args.bin.resolve()
    work = args.work or Path(tempfile.gettempdir()) / "toastty-bench"
    work.mkdir(parents=True, exist_ok=True)
    only = [s for s in args.only.split(",") if s] or SCENARIOS
    for s in only:
        if s not in SCENARIOS:
            ap.error(f"unknown scenario {s}")

    results = {
        "binary": str(binary),
        "binary_size_mb": binary.stat().st_size / (1 << 20),
        "scenarios": {},
    }
    bench = Bench(binary, work, args.reps)
    try:
        for name in only:
            print(f"running {name} ...", file=sys.stderr, flush=True)
            t0 = time.monotonic()
            try:
                results["scenarios"][name] = getattr(bench, name)()
            except Exception as e:  # keep going; record the failure
                results["scenarios"][name] = {"error": str(e)}
            print(
                f"  {name}: {json.dumps(results['scenarios'][name])} ({time.monotonic() - t0:.1f}s)",
                file=sys.stderr,
            )
    finally:
        bench.close()
    args.out.write_text(json.dumps(results, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
