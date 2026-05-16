#!/usr/bin/env python3
"""Decision archaeology benchmark — measure spelunk memory retrieval vs grep.

Evaluates whether `spelunk memory search` can retrieve decisions, context,
and design rationale from git history better than raw `git log --grep`.

Workflow:
  1. Index the repo:           spelunk index <repo>
  2. Harvest memory:           spelunk memory harvest --git-range <range>
  3. Run this benchmark:       python bench/memory/decision_archaeology.py ...

For each curated question, runs both memory search and grep baseline,
checking whether ground-truth keywords or commits appear in results.

Usage:
    # Single repo with a questions file
    python bench/memory/decision_archaeology.py \\
        --repo-path /path/to/repo \\
        --questions bench/memory/questions-ripgrep.json \\
        --out bench/results/archaeology-ripgrep.json

Questions file format (JSON):
    [
        {
            "question": "Why was X chosen over Y?",
            "answer_keywords": ["X", "Y", "reason"],
            "ground_truth_commit": "abc123"
        }
    ]
"""

import argparse
import json
import subprocess
import time
from datetime import datetime, timezone
from pathlib import Path


def run_memory_search(repo_path: Path, query: str, limit: int = 10) -> list[dict]:
    """Run spelunk memory search, return parsed results."""
    cmd = [
        "spelunk",
        "memory",
        "search",
        query,
        "--limit",
        str(limit),
        "--format",
        "json",
    ]
    try:
        result = subprocess.run(
            cmd, cwd=repo_path, capture_output=True, text=True, timeout=30
        )
        if result.returncode == 0 and result.stdout.strip():
            return json.loads(result.stdout)
        return []
    except (subprocess.TimeoutExpired, json.JSONDecodeError, Exception):
        return []


def run_git_log(repo_path: Path, query: str, limit: int = 10) -> list[dict]:
    """Run git log --grep, return parsed results as pseudo-results."""
    cmd = [
        "git",
        "--no-pager",
        "log",
        "--grep",
        query,
        "-i",
        "--max-count",
        str(limit),
        "--format=%H%n%s%n%b%n---",
    ]
    try:
        result = subprocess.run(
            cmd, cwd=repo_path, capture_output=True, text=True, timeout=30
        )
        entries = result.stdout.strip().split("---")
        results = []
        for entry in entries:
            lines = entry.strip().split("\n")
            if len(lines) >= 2:
                results.append(
                    {
                        "commit": lines[0].strip(),
                        "title": lines[1].strip() if len(lines) > 1 else "",
                        "body": "\n".join(lines[2:]).strip() if len(lines) > 2 else "",
                    }
                )
        return results[:limit]
    except Exception:
        return []


def check_hit(
    results: list[dict], keywords: list[str], commit: str
) -> tuple[bool, int | None]:
    """Check if results contain any keyword or the ground-truth commit.
    Returns (hit: bool, rank: int | None)."""
    for i, r in enumerate(results):
        text = json.dumps(r).lower()
        for kw in keywords:
            if kw.lower() in text:
                return True, i + 1
        if commit and commit in text:
            return True, i + 1
    return False, None


def get_spelunk_version() -> str:
    try:
        r = subprocess.run(
            ["spelunk", "--version"], capture_output=True, text=True, timeout=5
        )
        return r.stdout.strip()
    except Exception:
        return "unknown"


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Decision archaeology benchmark — memory search vs grep."
    )
    parser.add_argument("--repo-path", required=True)
    parser.add_argument(
        "--questions", required=True, help="Path to questions JSON file."
    )
    parser.add_argument("--out", default=None, help="Output JSON path.")
    parser.add_argument(
        "--limit", type=int, default=10, help="Results per query (default: 10)."
    )
    args = parser.parse_args()

    repo_path = Path(args.repo_path).resolve()
    if not repo_path.is_dir():
        parser.error(f"repo-path does not exist: {repo_path}")

    with open(args.questions) as f:
        questions = json.load(f)

    print(f"Repo:      {repo_path}")
    print(f"Questions: {len(questions)}")
    print()

    memory_hits = []
    memory_ranks = []
    grep_hits = []
    grep_ranks = []
    memory_wall = []
    grep_wall = []

    for i, q in enumerate(questions):
        question = q["question"]
        keywords = q.get("answer_keywords", [])
        commit = q.get("ground_truth_commit", "")

        print(f"[{i + 1}/{len(questions)}] {question[:80]}...")

        # Memory search
        start = time.monotonic()
        mem_results = run_memory_search(repo_path, question, args.limit)
        mem_elapsed = time.monotonic() - start
        mem_hit, mem_rank = check_hit(mem_results, keywords, commit)
        memory_hits.append(1.0 if mem_hit else 0.0)
        memory_ranks.append(1.0 / mem_rank if mem_rank else 0.0)
        memory_wall.append(mem_elapsed)

        # Grep baseline
        start = time.monotonic()
        grep_results = run_git_log(repo_path, question, args.limit)
        grep_elapsed = time.monotonic() - start
        grep_hit, grep_rank = check_hit(grep_results, keywords, commit)
        grep_hits.append(1.0 if grep_hit else 0.0)
        grep_ranks.append(1.0 / grep_rank if grep_rank else 0.0)
        grep_wall.append(grep_elapsed)

        print(
            f"  memory: {'HIT' if mem_hit else 'MISS'} (rank={mem_rank or '-'}, {mem_elapsed:.2f}s)"
        )
        print(
            f"  grep:   {'HIT' if grep_hit else 'MISS'} (rank={grep_rank or '-'}, {grep_elapsed:.2f}s)"
        )

    import statistics

    mem_recall = float(sum(memory_hits) / len(memory_hits)) if memory_hits else 0.0
    mem_mrr = float(sum(memory_ranks) / len(memory_ranks)) if memory_ranks else 0.0
    grep_recall = float(sum(grep_hits) / len(grep_hits)) if grep_hits else 0.0
    grep_mrr = float(sum(grep_ranks) / len(grep_ranks)) if grep_ranks else 0.0

    output = {
        "benchmark": "decision_archaeology",
        "repo": str(repo_path),
        "spelunk_version": get_spelunk_version(),
        "timestamp": datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ"),
        "num_questions": len(questions),
        "memory_search": {
            "recall": round(mem_recall, 4),
            "mrr": round(mem_mrr, 4),
            "median_wall_seconds": round(
                statistics.median(memory_wall) if memory_wall else 0, 3
            ),
        },
        "grep_baseline": {
            "recall": round(grep_recall, 4),
            "mrr": round(grep_mrr, 4),
            "median_wall_seconds": round(
                statistics.median(grep_wall) if grep_wall else 0, 3
            ),
        },
        "details": [
            {
                "question": q["question"],
                "memory_hit": bool(h),
                "memory_rank": r,
                "grep_hit": bool(gh),
                "grep_rank": gr,
            }
            for q, h, r, gh, gr in zip(
                questions, memory_hits, memory_ranks, grep_hits, grep_ranks
            )
        ],
    }

    print()
    print(
        f"Memory search: recall={mem_recall:.2%}  MRR={mem_mrr:.3f}  wall={statistics.median(memory_wall):.3f}s"
    )
    print(
        f"Grep baseline: recall={grep_recall:.2%}  MRR={grep_mrr:.3f}  wall={statistics.median(grep_wall):.3f}s"
    )

    if args.out:
        Path(args.out).parent.mkdir(parents=True, exist_ok=True)
        with open(args.out, "w") as f:
            json.dump(output, f, indent=2)
        print(f"\nResults written to: {args.out}")
    else:
        print(json.dumps(output, indent=2))


if __name__ == "__main__":
    main()
