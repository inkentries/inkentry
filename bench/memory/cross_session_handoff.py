#!/usr/bin/env python3
"""Cross-session handoff benchmark — verifiable tasks, forced cutoff, n>=10.

Three conditions for Session 2:
    no_memory_no_session_1 — cold start, clean repo, no prior session
    no_memory_present      — Session 1 file changes on disk, no memory tools
    with_memory            — full spelunk memory access

Each task has a verify_cmd that produces a binary pass/fail signal.
Tasks are defined in bench/memory/handoff_tasks.json.

Usage:
    python bench/memory/cross_session_handoff.py \\
        --tasks bench/memory/handoff_tasks.json \\
        --model deepseek-v4-flash \\
        --session-1-turns 5 \\
        --session-2-turns 15 \\
        --out bench/results/handoff.json

Workflow per task:
  1. Clone fresh repo, run setup_cmd (installs deps)
  2. Session 1: agent runs --session-1-turns, forced cutoff, stores handoff
  3. Session 2a: fresh clone, no memory, no S1 files
  4. Session 2b: S1 files on disk, no memory tools
  5. Session 2c: S1 files + spelunk memory access
  6. Verify each session's outcome with verify_cmd
"""

import argparse
import json
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from dotenv import load_dotenv

_root = Path(__file__).resolve().parents[2]
_dotenv = _root / ".env.local"
if _dotenv.exists():
    load_dotenv(_dotenv)

AGENT_SCRIPT = Path(__file__).resolve().parents[1] / "agents" / "agent.py"
REQUIREMENTS = Path(__file__).resolve().parents[1] / "requirements.txt"

SESSION_1_SYSTEM_ADDON = (
    "You have LIMITED TURNS. At the end of your turns, you MUST write a "
    "detailed handoff summarising what you investigated, what you changed, "
    "what is left to do, and any decisions you made. The next agent will "
    "rely on this handoff to continue your work."
)


def run_agent(
    repo_path: Path,
    task: str,
    condition: str,
    model: str,
    api_base: str,
    api_key: str,
    max_turns: int,
    seed: int,
    extra_system: str = "",
) -> dict:
    issue_file = repo_path / "ISSUE.txt"
    task_text = task
    if extra_system:
        task_text = extra_system + "\n\n" + task
    issue_file.write_text(task_text)

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
        str(seed),
    ]
    result = subprocess.run(cmd, capture_output=True, text=True, timeout=600)
    if result.returncode == 0:
        for line in reversed(result.stdout.strip().splitlines()):
            line = line.strip()
            if line.startswith("{"):
                return json.loads(line)
    return {"error": True, "stderr": result.stderr[:500]}


def run_verify(repo_path: Path, cmd: str) -> bool:
    try:
        r = subprocess.run(
            cmd, shell=True, cwd=repo_path, capture_output=True, text=True, timeout=60
        )
        return r.returncode == 0
    except Exception:
        return False


def store_handoff(repo_path: Path, task: str, turns: int) -> None:
    subprocess.run(
        [
            "spelunk",
            "memory",
            "add",
            "--kind",
            "handoff",
            "--title",
            f"Handoff: {task[:80]}",
            "--body",
            (
                f"Task: {task}\n"
                f"Session cut off after {turns} turns.\n"
                f"Repo state: partial changes present.\n"
                f"Next agent: review current state and continue."
            ),
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
    parser.add_argument("--tasks", required=True, help="Tasks JSON file.")
    parser.add_argument("--model", default="deepseek-v4-flash")
    parser.add_argument("--api-base-url", default="https://api.deepseek.com/v1")
    parser.add_argument("--api-key", default=None)
    parser.add_argument("--session-1-turns", type=int, default=5)
    parser.add_argument("--session-2-turns", type=int, default=15)
    parser.add_argument("--seed", type=int, default=42)
    parser.add_argument(
        "--workdir", default=None, help="Scratch directory (default: tmp)."
    )
    parser.add_argument("--out", default=None)
    args = parser.parse_args()

    api_key = args.api_key or os.environ.get("DEEPSEEK_API_KEY", "")
    if not api_key:
        parser.error("No API key.")

    with open(args.tasks) as f:
        tasks = json.load(f)

    workdir = (
        Path(args.workdir)
        if args.workdir
        else Path(tempfile.mkdtemp(prefix="handoff-"))
    )
    workdir.mkdir(parents=True, exist_ok=True)

    print(f"Tasks:     {len(tasks)}")
    print(f"S1 turns:  {args.session_1_turns}")
    print(f"S2 turns:  {args.session_2_turns}")
    print(f"Workdir:   {workdir}")
    print()

    all_results = []

    for ti, task in enumerate(tasks):
        task_name = task["task"]
        repo_url = task["repo_url"]
        setup_cmd = task.get("setup_cmd", "true")
        verify_cmd = task["verify_cmd"]
        repo_name = repo_url.rstrip("/").split("/")[-1].replace(".git", "")

        print(f"[{ti + 1}/{len(tasks)}] {task_name[:80]}")

        # ── Clone fresh repo ─────────────────────────────────────────────
        base = workdir / f"task{ti}"
        for clone_dir, label in [
            (base / "s1", "Session 1"),
            (base / "s2a", "S2 cold start"),
            (base / "s2b", "S2 files present"),
            (base / "s2c", "S2 with memory"),
        ]:
            clone_dir.parent.mkdir(parents=True, exist_ok=True)
            if not (clone_dir / ".git").exists():
                subprocess.run(
                    ["git", "clone", "--quiet", repo_url, str(clone_dir)],
                    capture_output=True,
                    timeout=120,
                )

        # Run setup once on a template
        template = base / "s1"
        if setup_cmd != "true":
            subprocess.run(
                setup_cmd, shell=True, cwd=template, capture_output=True, timeout=120
            )

        # Index for spelunk conditions
        subprocess.run(
            ["spelunk", "index", str(template)], capture_output=True, timeout=120
        )

        # ── Session 1: forced cutoff ─────────────────────────────────────
        print(f"  Session 1 (baseline, {args.session_1_turns} turns)...")
        s1 = run_agent(
            template,
            task_name,
            "baseline",
            args.model,
            args.api_base_url,
            api_key,
            args.session_1_turns,
            args.seed,
            extra_system=SESSION_1_SYSTEM_ADDON,
        )
        s1_turns = s1.get("turns", 0)
        if not s1.get("error"):
            store_handoff(template, task_name, s1_turns)
            print(f"    {s1_turns} turns, handoff stored")
        else:
            print(f"    ERROR: {s1.get('stderr', '')[:80]}")
            all_results.append({"task": task_name, "error": "session_1_failed"})
            continue

        # Copy S1 state to S2b and S2c
        for label, dest in [("s2b", base / "s2b"), ("s2c", base / "s2c")]:
            if dest.exists():
                shutil.rmtree(dest)
            shutil.copytree(str(template), str(dest), symlinks=True)
            # Index S2c for memory
            if label == "s2c":
                subprocess.run(
                    ["spelunk", "index", str(dest)], capture_output=True, timeout=120
                )

        # ── Session 2a: cold start ───────────────────────────────────────
        print(f"  Session 2a (cold start, no memory)...")
        s2a = run_agent(
            base / "s2a",
            task_name,
            "baseline",
            args.model,
            args.api_base_url,
            api_key,
            args.session_2_turns,
            args.seed,
        )
        s2a_ok = run_verify(base / "s2a", verify_cmd)
        print(f"    {'PASS' if s2a_ok else 'FAIL'}  turns={s2a.get('turns', '?')}")

        # ── Session 2b: files present, no memory ─────────────────────────
        print(f"  Session 2b (S1 files, no memory)...")
        s2b = run_agent(
            base / "s2b",
            task_name,
            "baseline",
            args.model,
            args.api_base_url,
            api_key,
            args.session_2_turns,
            args.seed,
        )
        s2b_ok = run_verify(base / "s2b", verify_cmd)
        print(f"    {'PASS' if s2b_ok else 'FAIL'}  turns={s2b.get('turns', '?')}")

        # ── Session 2c: with memory ──────────────────────────────────────
        print(f"  Session 2c (S1 files + memory)...")
        s2c = run_agent(
            base / "s2c",
            task_name,
            "spelunk_search",
            args.model,
            args.api_base_url,
            api_key,
            args.session_2_turns,
            args.seed,
        )
        s2c_ok = run_verify(base / "s2c", verify_cmd)
        print(f"    {'PASS' if s2c_ok else 'FAIL'}  turns={s2c.get('turns', '?')}")

        all_results.append(
            {
                "task": task_name,
                "repo": repo_url,
                "session_1_turns": s1_turns,
                "s2a_cold_start": {"success": s2a_ok, "turns": s2a.get("turns", 0)},
                "s2b_files_present": {"success": s2b_ok, "turns": s2b.get("turns", 0)},
                "s2c_with_memory": {"success": s2c_ok, "turns": s2c.get("turns", 0)},
            }
        )

    # ── Aggregate ────────────────────────────────────────────────────────
    ran = [r for r in all_results if "s2a_cold_start" in r]
    n = len(ran)
    print()
    print(f"=== Results ({n} tasks) ===")

    for cond, key in [
        ("Cold start", "s2a_cold_start"),
        ("Files present", "s2b_files_present"),
        ("With memory", "s2c_with_memory"),
    ]:
        successes = [r[key]["success"] for r in ran]
        rate = sum(successes) / n if n else 0
        print(f"  {cond:<18} success={rate:.0%} ({sum(successes)}/{n})")

    output = {
        "benchmark": "cross_session_handoff",
        "model": args.model,
        "session_1_turns": args.session_1_turns,
        "session_2_turns": args.session_2_turns,
        "seed": args.seed,
        "spelunk_version": get_spelunk_version(),
        "num_tasks": len(all_results),
        "results": all_results,
    }

    if args.out:
        Path(args.out).parent.mkdir(parents=True, exist_ok=True)
        with open(args.out, "w") as f:
            json.dump(output, f, indent=2)
        print(f"\nResults written to: {args.out}")
    else:
        print(json.dumps(output, indent=2))


if __name__ == "__main__":
    main()
