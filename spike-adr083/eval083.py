#!/usr/bin/env python3
"""SPIKE (ADR-083): CSN retrieval eval against the PR#44 fusion envelope.

Scratch copy of inkentry-bench/codesearchnet/evaluate.py's evaluate phase,
migrated to the enveloped `--format json` output and extended with:
  * an `--arm` selector (default | only-code)
  * memory-in-top-10 counting (acceptance criterion A)
  * per-query rank dump so paired repeats can be compared query-by-query

Usage:
  eval083.py --corpus-dir DIR --arm default [--gate 1.2032] --out FILE
"""
import argparse, json, os, statistics, subprocess, sys, time
from pathlib import Path


def env(gate, lexical):
    e = dict(os.environ)
    e["INKENTRY_SECRET_STORE"] = "file"
    if gate is not None:
        e["INKENTRY_SPIKE_MEMORY_MAX_QA_DISTANCE"] = str(gate)
    else:
        e.pop("INKENTRY_SPIKE_MEMORY_MAX_QA_DISTANCE", None)
    e["INKENTRY_SPIKE_LEXICAL_DOOR"] = "1" if lexical else "0"
    return e


def search(binary, query, cwd, arm, limit, e):
    cmd = [binary, "search", query, "--limit", str(limit), "--format", "json"]
    if arm == "only-code":
        cmd.append("--only-code")
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=180,
                           cwd=str(cwd), env=e)
    except (OSError, subprocess.SubprocessError) as exc:
        print(f"search failed: {exc}", file=sys.stderr)
        return []
    if r.returncode != 0:
        print(f"search exited {r.returncode}: {r.stderr.strip()[:200]}", file=sys.stderr)
        return []
    body = r.stdout.strip()
    if not body:
        return []
    try:
        return json.loads(body)
    except json.JSONDecodeError:
        print(f"non-JSON: {body[:200]}", file=sys.stderr)
        return []


def find_rank(results, name, relpath):
    """1-based fused rank of the target, matched on symbol name AND file path.

    Reads through the ADR-081 envelope: a ranked member is
    {type, fused_rank, ..., code:{name, file_path,...}} or {..., memory:{...}}.
    Attachments carry a null fused_rank and are skipped — they never held a
    ranked position.
    """
    for i, item in enumerate(results):
        if item.get("fused_rank") is None:
            continue
        c = item.get("code")
        if not c:
            continue
        rname = c.get("name") or ""
        rpath = c.get("file_path") or c.get("path") or ""
        if rname == name and rpath.replace("\\", "/").endswith(relpath):
            return i + 1
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--corpus-dir", required=True)
    ap.add_argument("--arm", default="default", choices=["default", "only-code"])
    ap.add_argument("--gate", default=None)
    ap.add_argument("--no-lexical-door", action="store_true")
    ap.add_argument("--limit", type=int, default=10)
    ap.add_argument("--out", required=True)
    a = ap.parse_args()

    binary = os.environ["INKENTRY_BIN"]
    cdir = Path(a.corpus_dir)
    manifest = json.loads((cdir / "manifest.json").read_text())
    root = cdir / "corpus"
    e = env(a.gate, not a.no_lexical_door)

    ranks, walls, mem_counts = [], [], []
    for n, entry in enumerate(manifest["entries"], 1):
        t0 = time.monotonic()
        res = search(binary, entry["query"], root, a.arm, a.limit, e)
        walls.append(time.monotonic() - t0)
        ranks.append(find_rank(res, entry["name"], entry["relpath"]))
        mem_counts.append(sum(1 for x in res
                              if x.get("type") == "memory"
                              and x.get("fused_rank") is not None))
        if n % 50 == 0:
            print(f"  {n}/{len(manifest['entries'])}", file=sys.stderr)

    total = len(ranks)
    out = {
        "arm": a.arm,
        "gate": a.gate,
        "lexical_door": not a.no_lexical_door,
        "samples": total,
        "mrr_at_10": round(sum(1.0 / r if r else 0.0 for r in ranks) / total, 4),
        "recall_at_5": round(sum(1.0 for r in ranks if r and r <= 5) / total, 4),
        "recall_at_10": round(sum(1.0 for r in ranks if r) / total, 4),
        "mean_memory_in_top10": round(sum(mem_counts) / total, 4),
        "queries_with_any_memory": sum(1 for c in mem_counts if c > 0),
        "median_wall_seconds": round(statistics.median(walls), 3),
        "ranks": ranks,
        "mem_counts": mem_counts,
    }
    Path(a.out).write_text(json.dumps(out))
    print(json.dumps({k: v for k, v in out.items()
                      if k not in ("ranks", "mem_counts")}, indent=2))


if __name__ == "__main__":
    main()
