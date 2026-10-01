#!/usr/bin/env python3
"""Median + range across paired repeats, plus paired per-query deltas."""
import json, glob, statistics as st, os, sys

D = os.path.dirname(os.path.abspath(__file__)) + "/runs"


def load(pat):
    out = []
    for f in sorted(glob.glob(f"{D}/{pat}")):
        out.append((os.path.basename(f), json.load(open(f))))
    return out


def agg(rows, key):
    v = [r[key] for _, r in rows]
    if not v:
        return None
    return min(v), st.median(v), max(v)


ARMS = [("only-code", "onlycode-r*.json"),
        ("unified ungated", "ungated-r*.json"),
        ("unified gated T=1.2032", "gated1.2032-r*.json")]

print("=" * 96)
print("PAIRED REPEATS — median [min, max] over n runs")
print("=" * 96)
print(f"{'arm':<26}{'n':>3}{'MRR@10':>26}{'Recall@5':>22}{'Recall@10':>22}")
data = {}
for label, pat in ARMS:
    rows = load(pat)
    data[label] = rows
    if not rows:
        continue
    def fmt(k):
        lo, md, hi = agg(rows, k)
        return f"{md:.4f} [{lo:.4f},{hi:.4f}]"
    print(f"{label:<26}{len(rows):>3}{fmt('mrr_at_10'):>26}"
          f"{fmt('recall_at_5'):>22}{fmt('recall_at_10'):>22}")

print()
print(f"{'arm':<26}{'n':>3}{'mean mem in top10':>22}{'queries w/ any mem':>22}{'median wall s':>16}")
for label, pat in ARMS:
    rows = data[label]
    if not rows:
        continue
    def fmt(k, d=4):
        lo, md, hi = agg(rows, k)
        return f"{md:.{d}f} [{lo:.{d}f},{hi:.{d}f}]"
    print(f"{label:<26}{len(rows):>3}{fmt('mean_memory_in_top10'):>22}"
          f"{fmt('queries_with_any_memory',1):>22}{fmt('median_wall_seconds',3):>16}")

print()
print("=" * 96)
print("RUN-TO-RUN NOISE within each arm (max - min across repeats)")
print("=" * 96)
for label, pat in ARMS:
    rows = data[label]
    if len(rows) < 2:
        continue
    print(f"  {label}")
    for k in ("mrr_at_10", "recall_at_5", "recall_at_10"):
        lo, md, hi = agg(rows, k)
        print(f"     {k:<14} range = {hi - lo:+.4f}   values = "
              + " ".join(f"{r[k]:.4f}" for _, r in rows))
    # per-query rank churn between repeats
    if len(rows) >= 2:
        a, b = rows[0][1]["ranks"], rows[1][1]["ranks"]
        churn = sum(1 for x, y in zip(a, b) if x != y)
        print(f"     per-query rank differs on {churn}/{len(a)} queries between r1 and r2")

print()
print("=" * 96)
print("CRITERION B — |Δ| vs --only-code, on medians")
print("=" * 96)
base = data.get("only-code") or []
if base:
    def med(rows, k):
        return st.median([r[k] for _, r in rows])
    print(f"{'comparison':<26}{'ΔRecall@10':>14}{'req':>10}"
          f"{'ΔRecall@5':>14}{'req':>10}{'ΔMRR@10':>14}{'req':>10}")
    for label in ("unified ungated", "unified gated T=1.2032"):
        rows = data.get(label) or []
        if not rows:
            continue
        d10 = med(rows, "recall_at_10") - med(base, "recall_at_10")
        d5 = med(rows, "recall_at_5") - med(base, "recall_at_5")
        dm = med(rows, "mrr_at_10") - med(base, "mrr_at_10")
        def v(d, t):
            return f"{'PASS' if abs(d) <= t else 'FAIL'}"
        print(f"{label:<26}{d10:>+14.4f}{v(d10,0.020):>10}"
              f"{d5:>+14.4f}{v(d5,0.020):>10}{dm:>+14.4f}{v(dm,0.010):>10}")

print()
print("=" * 96)
print("CRITERION A — mean memory in top 10 (target <= 0.05), across the band")
print("=" * 96)
for f in sorted(glob.glob(f"{D}/gated*-r*.json")) + sorted(glob.glob(f"{D}/ungated-r*.json")):
    r = json.load(open(f))
    g = r["gate"] or "none (baseline)"
    print(f"  T={str(g):<12} mean={r['mean_memory_in_top10']:.4f}  "
          f"queries_with_any={r['queries_with_any_memory']:>3}/500  "
          f"R@10={r['recall_at_10']:.4f}  MRR={r['mrr_at_10']:.4f}   [{os.path.basename(f)}]")
