#!/usr/bin/env python3
"""anim-diff.py KITTY.jsonl OTHER.jsonl: compare probe replies case by case.

The only accepted difference is kitty's stray leading comma on replies
without i= (toastty leaves it out, see docs/decisions/kitty-replies.md).
"""
import json, sys


def load(p):
    return {json.loads(l)["name"]: json.loads(l) for l in open(p)}


k, t = load(sys.argv[1]), load(sys.argv[2])
same = diff = 0
for name, kr in k.items():
    tr = t.get(name)
    if tr is None:
        print("MISSING", name)
        diff += 1
        continue
    kre = [r[1:] if r.startswith(",") else r for r in kr["replies"]]
    if kre == tr["replies"] and tr["sentinel"]:
        same += 1
    else:
        diff += 1
        print("DIFF", name)
        print("  kitty  :", kr["replies"])
        print("  toastty:", tr["replies"], "" if tr["sentinel"] else "(no sentinel)")
print(f"{same} identical, {diff} different")
