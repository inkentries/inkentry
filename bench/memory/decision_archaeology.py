#!/usr/bin/env python3
"""Decision archaeology benchmark — measure spelunk memory retrieval vs grep.

Four conditions compared against the same question set:

    grep_literal    — git log --grep with the full question string (verbatim)
    grep_keywords   — git log --grep with regex-extracted keywords from question
    fts_commit_msgs — SQLite FTS5 index over all commit messages, full question
    memory_search   — spelunk memory search (semantic over harvested entries)

Usage:
    python bench/memory/decision_archaeology.py \\
        --repo-path /path/to/repo \\
        --questions bench/memory/questions-ripgrep.json \\
        --out bench/results/archaeology.json

Questions file format (JSON):
    [
        {
            "question": "How does error handling work in the parser?",
            "ground_truth_commit": "abc123"
        }
    ]

See bench/memory/README.md for the authoring protocol.
"""

import argparse
import json
import re
import sqlite3
import statistics
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

# ---------------------------------------------------------------------------
# Keyword extraction
# ---------------------------------------------------------------------------


def extract_keywords(question: str) -> list[str]:
    """Extract search keywords from a natural-language question.

    Heuristic: capitalised words, underscored symbols, quoted strings,
    and CamelCase tokens. Returns ≤5 keywords, sorted by length descending.
    Returns empty list if no technical tokens found (caller handles honestly).
    """
    keywords: list[str] = []

    quoted = re.findall(r'"([^"]+)"', question)
    keywords.extend(quoted)
    caps = re.findall(r"\b[A-Z][A-Z_]{2,}\b", question)
    keywords.extend(caps)
    snake = re.findall(r"\b[a-z]+_[a-z_]+\b", question, re.IGNORECASE)
    keywords.extend(snake)
    camel = re.findall(r"\b[A-Z][a-z]+(?:[A-Z][a-z]+)+\b", question)
    keywords.extend(camel)

    seen = set()
    unique = []
    for kw in keywords:
        low = kw.lower()
        if low not in seen and len(kw) >= 3:
            seen.add(low)
            unique.append(kw)
    unique.sort(key=len, reverse=True)
    return unique[:5]  # empty if no technical tokens — caller scores 0 honestly


# ---------------------------------------------------------------------------
# Retrieval functions
# ---------------------------------------------------------------------------


def run_memory_search(repo_path: Path, query: str, limit: int = 10) -> list[dict]:
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


def _git_log_grep(repo_path: Path, pattern: str, limit: int = 10) -> list[dict]:
    cmd = [
        "git",
        "--no-pager",
        "log",
        "--grep",
        pattern,
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


def run_grep_literal(repo_path: Path, query: str, limit: int = 10) -> list[dict]:
    return _git_log_grep(repo_path, query, limit)


def run_grep_keywords(repo_path: Path, query: str, limit: int = 10) -> list[dict]:
    keywords = extract_keywords(query)
    all_results: list[dict] = []
    seen_commits: set[str] = set()
    for kw in keywords:
        for r in _git_log_grep(repo_path, kw, limit):
            if r["commit"] not in seen_commits:
                seen_commits.add(r["commit"])
                all_results.append(r)
        if len(all_results) >= limit:
            break
    return all_results[:limit]


def _build_fts_index(repo_path: Path, rebuild: bool = False) -> Path:
    db_path = repo_path / ".git" / "spelunk_fts_commits.db"
    if rebuild and db_path.exists():
        db_path.unlink()
    if db_path.exists():
        return db_path

    conn = sqlite3.connect(str(db_path))
    conn.execute(
        "CREATE VIRTUAL TABLE IF NOT EXISTS commits USING fts5(sha, title, body)"
    )
    conn.commit()

    proc = subprocess.Popen(
        ["git", "--no-pager", "log", "--format=%H%x00%s%x00%b%x00---"],
        cwd=repo_path,
        stdout=subprocess.PIPE,
        text=True,
    )
    batch = []
    for line in proc.stdout:
        line = line.strip()
        if line == "---" or not line:
            continue
        parts = line.split("\0")
        if len(parts) >= 2:
            batch.append((parts[0], parts[1], parts[2] if len(parts) > 2 else ""))
        if len(batch) >= 1000:
            conn.executemany(
                "INSERT INTO commits(sha, title, body) VALUES(?,?,?)", batch
            )
            batch = []
    if batch:
        conn.executemany("INSERT INTO commits(sha, title, body) VALUES(?,?,?)", batch)
    conn.commit()
    proc.wait()
    conn.close()
    return db_path


def _sanitize_fts_query(query: str) -> str:
    """Strip FTS5-special characters; return space-separated tokens."""
    cleaned = "".join(c if c.isalnum() or c.isspace() else " " for c in query)
    tokens = cleaned.split()
    return " ".join(tokens) if tokens else ""


def run_fts_commit_messages(
    repo_path: Path, query: str, limit: int = 10, rebuild_fts: bool = False
) -> list[dict]:
    """Build FTS5 index over commit messages and query with the full question."""
    try:
        clean = _sanitize_fts_query(query)
        if not clean:
            return []
        db_path = _build_fts_index(repo_path, rebuild=rebuild_fts)
        conn = sqlite3.connect(str(db_path))
        rows = conn.execute(
            "SELECT sha, title, body FROM commits WHERE commits MATCH ? ORDER BY rank LIMIT ?",
            (clean, limit),
        ).fetchall()
        conn.close()
        return [{"commit": r[0], "title": r[1], "body": r[2]} for r in rows]
    except sqlite3.OperationalError as e:
        # FTS query syntax errors — treat as no results
        print(f"  FTS query failed (syntax): {e}", file=sys.stderr)
        return []
    except Exception as e:
        print(f"  FTS query failed: {type(e).__name__}: {e}", file=sys.stderr)
        return []


# ---------------------------------------------------------------------------
# Hit checking
# ---------------------------------------------------------------------------


def check_hit(results: list[dict], commit: str) -> tuple[bool, int | None]:
    """Check if ground-truth commit appears in results.

    Handles both grep/FTS results (commit field) and memory results
    (source_ref field). Prefix-matches so short SHAs work with full SHAs.
    """
    if not commit:
        return False, None
    for i, r in enumerate(results):
        candidate = r.get("commit") or r.get("source_ref") or ""
        if candidate.startswith(commit) or commit.startswith(candidate):
            return True, i + 1
    return False, None


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def get_spelunk_version() -> str:
    try:
        r = subprocess.run(
            ["spelunk", "--version"], capture_output=True, text=True, timeout=5
        )
        return r.stdout.strip()
    except Exception:
        return "unknown"


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------


def main() -> None:
    parser = argparse.ArgumentParser(description="Decision archaeology benchmark.")
    parser.add_argument("--repo-path", required=True)
    parser.add_argument("--questions", required=True)
    parser.add_argument("--out", default=None)
    parser.add_argument("--limit", type=int, default=10)
    parser.add_argument(
        "--rebuild-fts",
        action="store_true",
        help="Rebuild the FTS5 commit index from scratch.",
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

    conditions = {
        "grep_literal": {"hits": [], "ranks": [], "wall": []},
        "grep_keywords": {"hits": [], "ranks": [], "wall": []},
        "fts_commit_messages": {"hits": [], "ranks": [], "wall": []},
        "memory_search": {"hits": [], "ranks": [], "wall": []},
    }

    for i, q in enumerate(questions):
        question = q["question"]
        commit = q.get("ground_truth_commit", "")

        print(f"[{i + 1}/{len(questions)}] {question[:80]}...")

        for cond_name, cond_data in conditions.items():
            start = time.monotonic()
            if cond_name == "grep_literal":
                results = run_grep_literal(repo_path, question, args.limit)
            elif cond_name == "grep_keywords":
                results = run_grep_keywords(repo_path, question, args.limit)
            elif cond_name == "fts_commit_messages":
                results = run_fts_commit_messages(
                    repo_path, question, args.limit, rebuild_fts=args.rebuild_fts
                )
            elif cond_name == "memory_search":
                results = run_memory_search(repo_path, question, args.limit)
            else:
                results = []
            elapsed = time.monotonic() - start

            hit, rank = check_hit(results, commit)
            cond_data["hits"].append(1.0 if hit else 0.0)
            cond_data["ranks"].append(1.0 / rank if rank else 0.0)
            cond_data["wall"].append(elapsed)

            status = "HIT" if hit else "MISS"
            print(f"  {cond_name:<22} {status:4} (rank={rank or '-'}, {elapsed:.2f}s)")

    output: dict = {
        "benchmark": "decision_archaeology",
        "repo": str(repo_path),
        "spelunk_version": get_spelunk_version(),
        "fts_rebuilt": args.rebuild_fts,
        "timestamp": datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ"),
        "num_questions": len(questions),
    }

    for cond_name, cond_data in conditions.items():
        hits = cond_data["hits"]
        ranks = cond_data["ranks"]
        walls = cond_data["wall"]
        output[cond_name] = {
            "recall": round(float(sum(hits) / len(hits)), 4) if hits else 0.0,
            "mrr": round(float(sum(ranks) / len(ranks)), 4) if ranks else 0.0,
            "median_wall_seconds": round(float(statistics.median(walls)), 3)
            if walls
            else 0.0,
        }

    output["details"] = [
        {
            "question": q["question"],
            **{f"{c}_hit": bool(conditions[c]["hits"][i]) for c in conditions},
            **{f"{c}_rank": conditions[c]["ranks"][i] for c in conditions},
        }
        for i, q in enumerate(questions)
    ]

    print()
    for cond_name in conditions:
        r = output[cond_name]["recall"]
        m = output[cond_name]["mrr"]
        w = output[cond_name]["median_wall_seconds"]
        print(f"{cond_name:<22} recall={r:.2%}  MRR={m:.3f}  wall={w:.3f}s")

    if args.out:
        Path(args.out).parent.mkdir(parents=True, exist_ok=True)
        with open(args.out, "w") as f:
            json.dump(output, f, indent=2)
        print(f"\nResults written to: {args.out}")
    else:
        print(json.dumps(output, indent=2))


if __name__ == "__main__":
    main()
