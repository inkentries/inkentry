#!/usr/bin/env python3
"""Cross-session handoff benchmark — measure the value of spelunk memory.

Simulates the core agent workflow:
  1. Session 1: Agent works on a multi-file task, stores handoff + decisions
     via `spelunk memory add` when it reaches turn limit.
  2. Session 2: Fresh agent (no prior conversation) attempts the same task.
     - Without memory: baseline tools only
     - With memory: has spelunk_memory_search to find the handoff

Measures: turns to completion, tokens used.

Usage:
    python bench/memory/cross_session_handoff.py \\
        --repo-path /path/to/repo \\
        --task "Rename all occurrences of foo to bar across the codebase" \\
        --model deepseek-v4-flash \\
        --max-turns 15 \\
        --out bench/results/handoff-demo.json

Prerequisites:
    - Repo must be indexed: spelunk index <repo>
    - API key in DEEPSEEK_API_KEY or passed via --api-key
"""

import argparse
import json
import os
import subprocess
import sys
import time
from pathlib import Path

from dotenv import load_dotenv

# Auto-load .env.local
_root = Path(__file__).resolve().parents[2]
_dotenv = _root / ".env.local"
if _dotenv.exists():
    load_dotenv(_dotenv)

AGENT_SCRIPT = Path(__file__).resolve().parents[1] / "agents" / "agent.py"
REQUIREMENTS = Path(__file__).resolve().parents[1] / "requirements.txt"


def run_agent(
    repo_path: Path,
    task: str,
    condition: str,
    model: str,
    api_base: str,
    api_key: str,
    max_turns: int,
    extra_message: str = "",
) -> dict:
    """Run agent.py and return the parsed JSON result."""
    issue_file = repo_path / "ISSUE.txt"
    if not issue_file.exists():
        issue_file.write_text(task)

    cmd = [
        "uv",
        "run",
        "--quiet",
        "--with-requirements",
        str(REQUIREMENTS),
        "python3",
        str(AGENT_SCRIPT),
        "--condition",
        condition,
        "--task-id",
        f"handoff-{repo_path.name}",
        "--repo-path",
        str(repo_path),
        "--issue",
        str(issue_file),
        "--model",
        model,
        "--api-base-url",
        api_base,
        "--api-key",
        api_key,
        "--max-turns",
        str(max_turns),
        "--seed",
        "42",
    ]

    result = subprocess.run(cmd, capture_output=True, text=True, timeout=600)
    if result.returncode == 0:
        for line in reversed(result.stdout.strip().splitlines()):
            line = line.strip()
            if line.startswith("{"):
                return json.loads(line)
    return {
        "error": True,
        "stderr": result.stderr[:500],
        "stdout": result.stdout[-500:],
    }


def store_handoff(repo_path: Path, task: str, turns_used: int) -> None:
    """Store a handoff memory entry via spelunk memory add."""
    title = f"Handoff: {task[:80]}"
    body = (
        f"Task: {task}\n"
        f"Session ran for {turns_used} turns before reaching limit.\n"
        f"Repo state: partial changes may be present.\n"
        f"Next agent should review current state and continue the work."
    )
    subprocess.run(
        [
            "spelunk",
            "memory",
            "add",
            "--kind",
            "handoff",
            "--title",
            title,
            "--body",
            body,
        ],
        cwd=repo_path,
        capture_output=True,
        timeout=30,
    )


def get_spelunk_version() -> str:
    try:
        r = subprocess.run(
            ["spelunk", "--version"], capture_output=True, text=True, timeout=5
        )
        return r.stdout.strip()
    except Exception:
        return "unknown"


def main():
    parser = argparse.ArgumentParser(description="Cross-session handoff benchmark.")
    parser.add_argument("--repo-path", required=True)
    parser.add_argument("--task", required=True, help="Refactoring task description.")
    parser.add_argument("--model", default="deepseek-v4-flash")
    parser.add_argument("--api-base-url", default="https://api.deepseek.com/v1")
    parser.add_argument("--api-key", default=None)
    parser.add_argument(
        "--max-turns",
        type=int,
        default=10,
        help="Max turns per session (default: 10, so S1 stops early).",
    )
    parser.add_argument("--out", default=None, help="Output JSON path.")
    args = parser.parse_args()

    repo_path = Path(args.repo_path).resolve()
    if not repo_path.is_dir():
        parser.error(f"repo-path does not exist: {repo_path}")

    api_key = args.api_key or os.environ.get("DEEPSEEK_API_KEY", "")
    if not api_key:
        parser.error("No API key. Use --api-key or set DEEPSEEK_API_KEY.")

    print(f"Repo:      {repo_path}")
    print(f"Task:      {args.task[:100]}")
    print(f"Max turns: {args.max_turns}")
    print()

    # ── Session 1: Agent works until turn limit, stores handoff ──────────
    print("=== Session 1 (baseline, stores handoff) ===")
    s1_start = time.monotonic()
    s1 = run_agent(
        repo_path,
        args.task,
        "baseline",
        args.model,
        args.api_base_url,
        api_key,
        args.max_turns,
    )
    s1_wall = time.monotonic() - s1_start

    s1_turns = s1.get("turns", 0)
    s1_tokens = s1.get("input_tokens", 0) + s1.get("output_tokens", 0)

    if not s1.get("error"):
        store_handoff(repo_path, args.task, s1_turns)
        print(f"  Turns: {s1_turns}  Tokens: {s1_tokens:,}  Wall: {s1_wall:.1f}s")
        print(f"  Handoff stored.")
    else:
        print(f"  ERROR: {s1.get('stderr', 'unknown')[:120]}")
        print(f"  Cannot store handoff — aborting.")
        sys.exit(1)

    print()

    # ── Session 2a: Without memory (fresh baseline agent) ─────────────────
    print("=== Session 2a (baseline, no memory) ===")
    s2a_start = time.monotonic()
    s2a = run_agent(
        repo_path,
        args.task,
        "baseline",
        args.model,
        args.api_base_url,
        api_key,
        args.max_turns,
    )
    s2a_wall = time.monotonic() - s2a_start

    s2a_turns = s2a.get("turns", 0)
    s2a_tokens = s2a.get("input_tokens", 0) + s2a.get("output_tokens", 0)
    print(f"  Turns: {s2a_turns}  Tokens: {s2a_tokens:,}  Wall: {s2a_wall:.1f}s")

    print()

    # ── Session 2b: With memory (spelunk_search condition) ────────────────
    print("=== Session 2b (spelunk_search, can find handoff) ===")
    s2b_start = time.monotonic()
    s2b = run_agent(
        repo_path,
        args.task,
        "spelunk_search",
        args.model,
        args.api_base_url,
        api_key,
        args.max_turns,
    )
    s2b_wall = time.monotonic() - s2b_start

    s2b_turns = s2b.get("turns", 0)
    s2b_tokens = s2b.get("input_tokens", 0) + s2b.get("output_tokens", 0)
    print(f"  Turns: {s2b_turns}  Tokens: {s2b_tokens:,}  Wall: {s2b_wall:.1f}s")

    # ── Results ───────────────────────────────────────────────────────────
    output = {
        "benchmark": "cross_session_handoff",
        "repo": str(repo_path),
        "task": args.task,
        "model": args.model,
        "max_turns": args.max_turns,
        "spelunk_version": get_spelunk_version(),
        "session_1": {
            "turns": s1_turns,
            "tokens": s1_tokens,
            "wall_seconds": round(s1_wall, 2),
        },
        "session_2_no_memory": {
            "condition": "baseline",
            "turns": s2a_turns,
            "tokens": s2a_tokens,
            "wall_seconds": round(s2a_wall, 2),
        },
        "session_2_with_memory": {
            "condition": "spelunk_search",
            "turns": s2b_turns,
            "tokens": s2b_tokens,
            "wall_seconds": round(s2b_wall, 2),
        },
    }

    print()
    print("=== Results ===")
    print(f"Session 1 (baseline):          {s1_turns} turns, {s1_tokens:,} tokens")
    print(f"Session 2 no memory (baseline): {s2a_turns} turns, {s2a_tokens:,} tokens")
    print(f"Session 2 with memory (search): {s2b_turns} turns, {s2b_tokens:,} tokens")

    if args.out:
        Path(args.out).parent.mkdir(parents=True, exist_ok=True)
        with open(args.out, "w") as f:
            json.dump(output, f, indent=2)
        print(f"\nResults written to: {args.out}")
    else:
        print(json.dumps(output, indent=2))


if __name__ == "__main__":
    main()
