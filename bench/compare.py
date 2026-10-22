#!/usr/bin/env python3
"""Compare two bench/run.py result files as a Markdown table.

    bench/compare.py base.json new.json [more.json ...]

The change column is relative to the first file. Metrics where lower is
better (time, CPU, memory, latency) and higher is better (throughput,
FPS) are marked so a regression stands out.
"""

import json
import math
import sys

# metric -> (label, higher_is_better, format)
METRICS = {
    "mb_per_s": ("throughput MB/s", True, "{:.1f}"),
    "fps": ("FPS during run", True, "{:.1f}"),
    "frames": ("frames", True, "{:.0f}"),
    "cpu_s": ("CPU s", False, "{:.2f}"),
    "active_s": ("wall s", False, "{:.2f}"),
    "peak_rss_mb": ("peak RSS MB", False, "{:.1f}"),
    "final_rss_mb": ("final RSS MB", False, "{:.1f}"),
    "frame_ms_p50": ("frame ms p50", False, "{:.2f}"),
    "frame_ms_p99": ("frame ms p99", False, "{:.2f}"),
    "frame_ms_mean": ("frame ms mean", False, "{:.2f}"),
    "output_latency_ms_p50": ("output→frame ms p50", False, "{:.2f}"),
    "output_latency_ms_p99": ("output→frame ms p99", False, "{:.2f}"),
    "key_latency_ms_p50": ("key→frame ms p50", False, "{:.2f}"),
    "key_latency_ms_p99": ("key→frame ms p99", False, "{:.2f}"),
    "first_frame_ms": ("first frame ms", False, "{:.1f}"),
    "launch_to_frame_ms": ("launch→first frame ms", False, "{:.1f}"),
    "idle_rss_mb": ("idle RSS MB", False, "{:.1f}"),
    "cpu_pct": ("CPU %", False, "{:.2f}"),
    "rss_mb": ("RSS MB", False, "{:.1f}"),
}

# Per-scenario metric selection (others are noise for that scenario).
PICK = {
    "startup": ["launch_to_frame_ms", "first_frame_ms", "idle_rss_mb"],
    "idle_blink": ["cpu_pct", "rss_mb"],
    "idle_steady": ["cpu_pct", "rss_mb"],
    "key_latency": ["key_latency_ms_p50", "key_latency_ms_p99"],
    "scroll_wheel": ["frame_ms_p50", "frame_ms_p99", "cpu_s"],
    "scroll_wheel_big": ["frame_ms_p50", "frame_ms_p99", "cpu_s"],
    "scrollback_memory": ["final_rss_mb", "peak_rss_mb", "mb_per_s"],
}
DEFAULT = ["mb_per_s", "fps", "frame_ms_p50", "frame_ms_p99", "cpu_s", "peak_rss_mb",
           "output_latency_ms_p50", "output_latency_ms_p99"]


def fmt(fmt_s, v):
    if v is None or (isinstance(v, float) and math.isnan(v)):
        return "–"
    return fmt_s.format(v)


def main():
    files = sys.argv[1:]
    if len(files) < 2:
        sys.exit(__doc__)
    data = [json.load(open(f)) for f in files]
    names = [f.rsplit("/", 1)[-1].removesuffix(".json") for f in files]
    head = "| scenario | metric | " + " | ".join(names) + " | change |"
    print(head)
    print("|" + "---|" * (len(names) + 3))
    print(
        "| binary | size MB | "
        + " | ".join(f"{d['binary_size_mb']:.1f}" for d in data)
        + " | |"
    )
    for scen in data[0]["scenarios"]:
        for m in PICK.get(scen, DEFAULT):
            vals = [d["scenarios"].get(scen, {}).get(m) for d in data]
            if all(v is None for v in vals):
                continue
            label, hib, f = METRICS[m]
            base, last = vals[0], vals[-1]
            change = ""
            if (
                isinstance(base, (int, float)) and isinstance(last, (int, float))
                and not math.isnan(base) and not math.isnan(last) and base != 0
            ):
                r = last / base
                better = (r > 1) == hib
                arrow = "✅" if better and abs(r - 1) > 0.05 else ("⚠️" if abs(r - 1) > 0.05 else "")
                change = f"{r:.2f}× {arrow}"
            print(f"| {scen} | {label} | " + " | ".join(fmt(f, v) for v in vals) + f" | {change} |")


if __name__ == "__main__":
    main()
