#!/usr/bin/env python3
"""Summarize a folded-stack profile (bench/profile.sh output).

    bench/folded.py prof.folded [--top 40] [--thread main] [--focus substr]

Prints the top functions by self and inclusive sample share.
"""
import argparse
import re
from collections import Counter

ap = argparse.ArgumentParser()
ap.add_argument("path")
ap.add_argument("--top", type=int, default=40)
ap.add_argument("--thread", default=None)
ap.add_argument("--focus", default=None, help="only stacks containing this substring")
ap.add_argument("--width", type=int, default=110)
a = ap.parse_args()

def short(name: str) -> str:
    name = re.sub(r"::h[0-9a-f]{16}$", "", name)
    name = re.sub(r"<(.*) as (.*)>", r"<\2>", name)
    return name

selfc, incl, threads = Counter(), Counter(), Counter()
total = 0
for line in open(a.path):
    stack, _, n = line.rstrip("\n").rpartition(" ")
    n = int(n)
    frames = stack.split(";")
    thread, frames = frames[0], [short(f) for f in frames[1:]]
    threads[thread] += n
    if a.thread and a.thread not in thread:
        continue
    if a.focus and not any(a.focus in f for f in frames):
        continue
    total += n
    if frames:
        selfc[frames[-1]] += n
    for f in set(frames):
        incl[f] += n

print("threads:", ", ".join(f"{t}={c}" for t, c in threads.most_common()))
print(f"samples in selection: {total}")
for title, c in (("SELF", selfc), ("INCLUSIVE", incl)):
    print(f"\n== {title} ==")
    for f, n in c.most_common(a.top):
        print(f"{100*n/total:6.2f}%  {f[:a.width]}")
