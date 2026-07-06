#!/usr/bin/env python3
"""Paired statistics for spelunk agentic benchmark comparisons (plan §6).

Compares a baseline condition against a spelunk condition over the SAME tasks,
paired by task_id. Emits a cell-labeled table with:

  - McNemar's exact test on discordant pairs (task-level binary outcomes)
  - Bootstrap 95% CIs over per-seed cell means (n>=3 agentic seeds)
  - Deterministic layers accept n=1 and are labeled "deterministic, n=1"
  - Non-significant deltas are reported as such, never dropped

Pure stdlib (statistics, random, math) - no scipy/numpy.

Usage:
    python bench/paired_stats.py <baseline.json> <condition.json> [--filter FILTER]

Each result file is either a JSON array of per-task records or a
{"aggregate": ..., "tasks": [...]} dict (post-harness format). Per-task records
must carry `task_id` and a binary outcome (`resolved` or `passed`). Cell fields
(`model`, `benchmark`/harness, `condition`, `seed`) are read per record.
"""

import argparse
import json
import math
import random
import statistics
import sys
from pathlib import Path

BOOTSTRAP_ITERS = 10000
BOOTSTRAP_SEED = 20260706  # fixed so CIs are reproducible across runs
OUTCOME_KEYS = ("resolved", "passed", "success")


def load_tasks(path: str) -> list[dict]:
    with open(path) as f:
        raw = json.load(f)
    tasks = raw["tasks"] if isinstance(raw, dict) and "tasks" in raw else raw
    if not isinstance(tasks, list):
        raise ValueError(f"{path}: expected a task array or {{'tasks': [...]}}")
    return [t for t in tasks if not t.get("skipped") and not t.get("error")]


def outcome(task: dict) -> bool:
    for key in OUTCOME_KEYS:
        if key in task:
            return bool(task[key])
    raise ValueError(f"task {task.get('task_id', '?')}: no outcome field {OUTCOME_KEYS}")


def task_outcomes(tasks: list[dict]) -> dict[str, bool]:
    """One binary outcome per task_id for McNemar. Multi-seed cells collapse by
    majority vote (ties -> pass)."""
    by_task: dict[str, list[bool]] = {}
    for t in tasks:
        by_task.setdefault(t["task_id"], []).append(outcome(t))
    return {tid: sum(o) * 2 >= len(o) for tid, o in by_task.items()}


def cell_label(tasks: list[dict], instance_filter: str) -> dict:
    """Full cell identity. Refuses to blend differing model/harness/condition."""

    def uniq(key: str, default: str) -> str:
        vals = {str(t.get(key, default)) for t in tasks}
        if len(vals) > 1:
            raise ValueError(
                f"refusing to aggregate across differing {key}: {sorted(vals)}"
            )
        return next(iter(vals)) if vals else default

    model = uniq("model", "unknown")
    source = uniq("model_source", "") if any("model_source" in t for t in tasks) else ""
    harness = uniq("benchmark", "unknown")
    condition = uniq("condition", "unknown")
    seeds = sorted({t["seed"] for t in tasks if "seed" in t})

    return {
        "model": f"{model} ({source})" if source else model,
        "harness": harness,
        "condition": condition,
        "instance_filter": instance_filter,
        "n_tasks": len({t["task_id"] for t in tasks}),
        "seeds": seeds,
    }


def mcnemar_exact(baseline: dict, condition: dict) -> dict:
    """Exact (binomial) McNemar on discordant pairs, paired by task_id.

    b = baseline pass & condition fail; c = baseline fail & condition pass.
    Two-sided exact p over Binomial(b+c, 0.5).
    """
    shared = sorted(set(baseline) & set(condition))
    if not shared:
        raise ValueError("no shared task_ids between the two conditions")

    both = sum(1 for t in shared if baseline[t] and condition[t])
    b = sum(1 for t in shared if baseline[t] and not condition[t])
    c = sum(1 for t in shared if not baseline[t] and condition[t])
    neither = sum(1 for t in shared if not baseline[t] and not condition[t])

    n = b + c
    if n == 0:
        p = 1.0
    else:
        k = min(b, c)
        tail = sum(math.comb(n, i) for i in range(0, k + 1)) / (2**n)
        p = min(1.0, 2 * tail)

    return {
        "n_paired": len(shared),
        "both_pass": both,
        "both_fail": neither,
        "baseline_only": b,  # regressions under condition
        "condition_only": c,  # gains under condition
        "discordant": n,
        "p_value": p,
        "significant": p < 0.05,
    }


def per_seed_means(tasks: list[dict]) -> dict[int, float]:
    """Cell mean pass-rate per seed."""
    by_seed: dict[int, list[bool]] = {}
    for t in tasks:
        by_seed.setdefault(t.get("seed", 0), []).append(outcome(t))
    return {s: statistics.mean(o) for s, o in by_seed.items()}


def bootstrap_ci(values: list[float]) -> dict:
    """95% percentile bootstrap CI over per-seed means.

    Deterministic layers (n=1) get no CI - labeled instead.
    """
    n = len(values)
    mean = statistics.mean(values)
    if n < 3:
        return {
            "mean": mean,
            "n_seeds": n,
            "ci_low": None,
            "ci_high": None,
            "note": "deterministic, n=1" if n == 1 else f"n={n}, CI needs n>=3",
        }

    rng = random.Random(BOOTSTRAP_SEED)
    boot = []
    for _ in range(BOOTSTRAP_ITERS):
        sample = [values[rng.randrange(n)] for _ in range(n)]
        boot.append(statistics.mean(sample))
    boot.sort()
    lo = boot[int(0.025 * BOOTSTRAP_ITERS)]
    hi = boot[int(0.975 * BOOTSTRAP_ITERS)]
    return {"mean": mean, "n_seeds": n, "ci_low": lo, "ci_high": hi, "note": None}


def fmt_ci(ci: dict) -> str:
    if ci["ci_low"] is None:
        return f"{ci['mean']:.3f} ({ci['note']})"
    half = (ci["ci_high"] - ci["ci_low"]) / 2
    return f"{ci['mean']:.3f} +/- {half:.3f}  [{ci['ci_low']:.3f}, {ci['ci_high']:.3f}]"


def fmt_cell(label: dict) -> str:
    return (
        f"model={label['model']} | harness={label['harness']} | "
        f"condition={label['condition']} | filter={label['instance_filter']} | "
        f"n={label['n_tasks']}"
    )


def compare(baseline_path: str, condition_path: str, instance_filter: str) -> str:
    base_tasks = load_tasks(baseline_path)
    cond_tasks = load_tasks(condition_path)
    if not base_tasks or not cond_tasks:
        raise ValueError("one or both result files have no runnable tasks")

    base_label = cell_label(base_tasks, instance_filter)
    cond_label = cell_label(cond_tasks, instance_filter)

    base_out = task_outcomes(base_tasks)
    cond_out = task_outcomes(cond_tasks)

    mc = mcnemar_exact(base_out, cond_out)
    base_ci = bootstrap_ci(list(per_seed_means(base_tasks).values()))
    cond_ci = bootstrap_ci(list(per_seed_means(cond_tasks).values()))
    delta = cond_ci["mean"] - base_ci["mean"]

    lines = []
    lines.append("## Paired comparison (plan §6)")
    lines.append("")
    lines.append(f"Baseline  cell: {fmt_cell(base_label)}")
    lines.append(f"Condition cell: {fmt_cell(cond_label)}")
    lines.append("")
    lines.append("### Pass rate (bootstrap 95% CI over per-seed means)")
    lines.append(f"  baseline : {fmt_ci(base_ci)}")
    lines.append(f"  condition: {fmt_ci(cond_ci)}")
    lines.append(f"  delta    : {delta:+.3f}")
    lines.append("")
    lines.append("### McNemar exact test (paired by task_id)")
    lines.append(f"  paired tasks : {mc['n_paired']}")
    lines.append(f"  both pass    : {mc['both_pass']}")
    lines.append(f"  both fail    : {mc['both_fail']}")
    lines.append(f"  condition only (gains)     : {mc['condition_only']}")
    lines.append(f"  baseline only (regressions): {mc['baseline_only']}")
    lines.append(f"  discordant   : {mc['discordant']}")
    lines.append(f"  exact p      : {mc['p_value']:.4f}")
    verdict = "SIGNIFICANT (p<0.05)" if mc["significant"] else "not significant"
    lines.append(f"  result       : {verdict}")
    lines.append("")
    lines.append(
        "Power note: a 50-task slice only detects large effects (~+/-15pp). "
        "Headline claims require the filtered subset or the 150+ question set."
    )
    return "\n".join(lines)


def main() -> None:
    parser = argparse.ArgumentParser(description="Paired benchmark statistics (§6).")
    parser.add_argument("baseline", help="baseline-condition result JSON")
    parser.add_argument("condition", help="spelunk-condition result JSON")
    parser.add_argument(
        "--filter",
        default="all",
        help="instance_filter label for the cell (e.g. 'all', 'django-only')",
    )
    args = parser.parse_args()

    for path in (args.baseline, args.condition):
        if not Path(path).exists():
            print(f"Error: file not found: {path}", file=sys.stderr)
            sys.exit(1)

    try:
        print(compare(args.baseline, args.condition, args.filter))
    except ValueError as e:
        print(f"Error: {e}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
