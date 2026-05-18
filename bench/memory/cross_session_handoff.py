#!/usr/bin/env python3
"""Cross-session handoff benchmark — verifiable tasks, forced cutoff, n>=10.

Three conditions for Session 2:
    s2a_cold_start   — fresh clone, no S1 files, no memory
    s2b_files_present — S1 file changes on disk, no memory tools
    s2c_with_memory  — S1 files + spelunk memory access

Each task has a verify_cmd that produces a binary pass/fail signal.
Tasks are defined in bench/memory/handoff_tasks.json.

Usage:
    python bench/memory/cross_session_handoff.py \\
        --tasks bench/memory/handoff_tasks.json \\
        --model deepseek-v4-flash \\
        --session-1-turns 5 \\
        --session-2-turns 15 \\
        --out bench/results/handoff.json
"""

import argparse
import atexit
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
    task_text = extra_system + "\n\n" + task if extra_system else task
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


def run_verify(repo_path: Path, cmd: str) -> tuple[bool, str]:
    """Return (success, truncated_output)."""
    try:
        r = subprocess.run(
            cmd,
            shell=True,
            cwd=repo_path,
            capture_output=True,
            text=True,
            timeout=60,
        )
        output = (r.stdout + r.stderr)[-2000:]
        return r.returncode == 0, output
    except subprocess.TimeoutExpired:
        return False, "TIMEOUT after 60s"
    except Exception as e:
        return False, f"verify_cmd error: {e}"


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


def wilson_ci(successes: int, n: int, z: float = 1.96) -> tuple[float, float]:
    """Wilson score interval for a binomial proportion."""
    if n == 0:
        return (0.0, 0.0)
    p = successes / n
    denom = 1 + z**2 / n
    centre = (p + z**2 / (2 * n)) / denom
    margin = (z * (p * (1 - p) / n + z**2 / (4 * n**2)) ** 0.5) / denom
    return (max(0, centre - margin), min(1, centre + margin))


def main():
    parser = argparse.ArgumentParser(description="Cross-session handoff benchmark.")
    parser.add_argument("--tasks", required=True)
    parser.add_argument("--model", default="deepseek-v4-flash")
    parser.add_argument("--api-base-url", default="https://api.deepseek.com/v1")
    parser.add_argument("--api-key", default=None)
    parser.add_argument("--session-1-turns", type=int, default=5)
    parser.add_argument("--session-2-turns", type=int, default=15)
    parser.add_argument("--seed", type=int, default=42)
    parser.add_argument("--workdir", default=None)
    parser.add_argument("--out", default=None)
    args = parser.parse_args()

    api_key = args.api_key or os.environ.get("DEEPSEEK_API_KEY", "")
    if not api_key:
        parser.error("No API key.")

    with open(args.tasks) as f:
        tasks = json.load(f)

    user_workdir = args.workdir is not None
    workdir = (
        Path(args.workdir)
        if user_workdir
        else Path(tempfile.mkdtemp(prefix="handoff-"))
    )
    workdir.mkdir(parents=True, exist_ok=True)
    if not user_workdir:
        atexit.register(shutil.rmtree, str(workdir), ignore_errors=True)

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

        print(f"[{ti + 1}/{len(tasks)}] {task_name[:80]}")

        # ── Clone all four copies; run setup on all ─────────────────────
        base = workdir / f"task{ti}"
        clones = {
            "s1": base / "s1",
            "s2a": base / "s2a",
            "s2b": base / "s2b",
            "s2c": base / "s2c",
        }
        for clone_dir in clones.values():
            clone_dir.parent.mkdir(parents=True, exist_ok=True)
            if not (clone_dir / ".git").exists():
                subprocess.run(
                    ["git", "clone", "--quiet", repo_url, str(clone_dir)],
                    capture_output=True,
                    timeout=120,
                )
            if setup_cmd != "true":
                subprocess.run(
                    setup_cmd,
                    shell=True,
                    cwd=clone_dir,
                    capture_output=True,
                    timeout=120,
                )

        # Index s1 and s2c for spelunk
        subprocess.run(
            ["spelunk", "index", str(clones["s1"])], capture_output=True, timeout=120
        )
        subprocess.run(
            ["spelunk", "index", str(clones["s2c"])], capture_output=True, timeout=120
        )

        # Pre-flight: verify_cmd must FAIL on unmodified clone
        pre_ok, pre_out = run_verify(clones["s2a"], verify_cmd)
        if pre_ok:
            print(
                f"  WARNING: verify_cmd passes on unmodified repo — task may be degenerate"
            )
        else:
            print(
                f'  Pre-flight: verify_cmd fails as expected ("{pre_out[:60].strip()}")'
            )

        # ── Session 1: forced cutoff ────────────────────────────────────
        print(f"  Session 1 ({args.session_1_turns} turns)...")
        s1 = run_agent(
            clones["s1"],
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
        if s1.get("error"):
            print(f"    ERROR: {s1.get('stderr', '')[:80]}")
            all_results.append({"task": task_name, "error": "session_1_failed"})
            continue
        store_handoff(clones["s1"], task_name, s1_turns)
        print(f"    {s1_turns} turns, handoff stored")

        # Copy S1 state to s2b and s2c; strip memory from s2b (no-memory condition)
        for key in ("s2b", "s2c"):
            if clones[key].exists():
                shutil.rmtree(clones[key])
            shutil.copytree(str(clones["s1"]), str(clones[key]), symlinks=True)
            if key == "s2b":
                shutil.rmtree(clones[key] / ".spelunk", ignore_errors=True)
                subprocess.run(
                    [
                        "git",
                        "-C",
                        str(clones[key]),
                        "notes",
                        "remove",
                        "--ignore-missing",
                        "HEAD",
                    ],
                    capture_output=True,
                )

        # ── Session 2a: cold start ──────────────────────────────────────
        print(f"  Session 2a (cold start)...")
        s2a = run_agent(
            clones["s2a"],
            task_name,
            "baseline",
            args.model,
            args.api_base_url,
            api_key,
            args.session_2_turns,
            args.seed,
        )
        s2a_ok, s2a_out = run_verify(clones["s2a"], verify_cmd)
        status = "PASS" if s2a_ok else "FAIL"
        print(f"    {status}  turns={s2a.get('turns', '?')}  ({s2a_out[:60].strip()})")

        # ── Session 2b: files present ───────────────────────────────────
        print(f"  Session 2b (S1 files, no memory)...")
        s2b = run_agent(
            clones["s2b"],
            task_name,
            "baseline",
            args.model,
            args.api_base_url,
            api_key,
            args.session_2_turns,
            args.seed,
        )
        s2b_ok, s2b_out = run_verify(clones["s2b"], verify_cmd)
        status = "PASS" if s2b_ok else "FAIL"
        print(f"    {status}  turns={s2b.get('turns', '?')}  ({s2b_out[:60].strip()})")

        # ── Session 2c: with memory ─────────────────────────────────────
        print(f"  Session 2c (S1 files + memory)...")
        s2c = run_agent(
            clones["s2c"],
            task_name,
            "spelunk_search",
            args.model,
            args.api_base_url,
            api_key,
            args.session_2_turns,
            args.seed,
        )
        s2c_ok, s2c_out = run_verify(clones["s2c"], verify_cmd)
        status = "PASS" if s2c_ok else "FAIL"
        print(f"    {status}  turns={s2c.get('turns', '?')}  ({s2c_out[:60].strip()})")

        all_results.append(
            {
                "task": task_name,
                "repo": repo_url,
                "session_1_turns": s1_turns,
                "s2a_cold_start": {
                    "success": s2a_ok,
                    "turns": s2a.get("turns", 0),
                    "verify_output": s2a_out,
                },
                "s2b_files_present": {
                    "success": s2b_ok,
                    "turns": s2b.get("turns", 0),
                    "verify_output": s2b_out,
                },
                "s2c_with_memory": {
                    "success": s2c_ok,
                    "turns": s2c.get("turns", 0),
                    "verify_output": s2c_out,
                },
            }
        )

    # ── Aggregate ───────────────────────────────────────────────────────
    ran = [r for r in all_results if "s2a_cold_start" in r]
    n = len(ran)
    print()
    print(f"=== Results ({n} tasks) ===")

    aggregate = {}
    for cond, key in [
        ("cold_start", "s2a_cold_start"),
        ("files_present", "s2b_files_present"),
        ("with_memory", "s2c_with_memory"),
    ]:
        successes = [r[key]["success"] for r in ran]
        s = sum(successes)
        lo, hi = wilson_ci(s, n)
        rate = s / n if n else 0
        aggregate[f"{cond}_success_rate"] = round(rate, 4)
        aggregate[f"{cond}_ci_95"] = [round(lo, 4), round(hi, 4)]
        print(f"  {cond:<18} success={rate:.0%} [{lo:.0%}, {hi:.0%}] ({s}/{n})")

    output = {
        "benchmark": "cross_session_handoff",
        "model": args.model,
        "session_1_turns": args.session_1_turns,
        "session_2_turns": args.session_2_turns,
        "seed": args.seed,
        "spelunk_version": get_spelunk_version(),
        "num_tasks": len(all_results),
        "aggregate": aggregate,
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
