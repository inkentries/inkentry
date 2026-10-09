#!/usr/bin/env python3
"""Criterion C: the 20 positive (question, entry) pairs through the unified
default. Reports fused top-10 / top-3 hit rates at each threshold in the band."""
import json, os, subprocess, sys
from pathlib import Path

D = Path(os.path.dirname(os.path.abspath(__file__)))
BIN = os.environ["INKENTRY_BIN"]
ROOT = D / "corpus"
pos = json.loads((D / "positives.json").read_text())
notes = {n["id"]: n["title"] for n in json.loads((D / "notes-clean.json").read_text())}


def run(q, gate):
    e = dict(os.environ)
    e["INKENTRY_SECRET_STORE"] = "file"
    if gate is None:
        e.pop("INKENTRY_SPIKE_MEMORY_MAX_QA_DISTANCE", None)
    else:
        e["INKENTRY_SPIKE_MEMORY_MAX_QA_DISTANCE"] = str(gate)
    r = subprocess.run([BIN, "search", q, "--limit", "10", "--format", "json"],
                       capture_output=True, text=True, timeout=180, cwd=str(ROOT), env=e)
    try:
        return json.loads(r.stdout.strip() or "[]")
    except json.JSONDecodeError:
        return []


for gate in [None, "1.1822", "1.2032", "1.2337"]:
    top10 = top3 = 0
    detail = []
    for p in pos:
        res = run(p["question"], gate)
        want = notes[p["note_id"]]
        rank = None
        for i, x in enumerate(res):
            if x.get("fused_rank") is None:
                continue
            m = x.get("memory")
            if m and m.get("title") == want:
                rank = i + 1
                break
        if rank:
            top10 += 1
            if rank <= 3:
                top3 += 1
        detail.append((p["note_id"], rank))
    n = len(pos)
    print(f"gate={str(gate):<10} top10={top10}/{n} ({top10/n*100:.0f}%, need >=90%)  "
          f"top3={top3}/{n} ({top3/n*100:.0f}%, need >=70%)")
    print("   ranks:", " ".join(f"{i}:{r if r else '-'}" for i, r in detail))
    sys.stdout.flush()
