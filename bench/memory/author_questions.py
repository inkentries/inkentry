#!/usr/bin/env python3
"""Export raw git log for blind question authoring.

Produces a JSON file containing commit SHAs, subjects, and bodies from
a repo's git history. The output is intended to be read by a human (or
another LLM session) that has NO access to the spelunk memory database.

The resulting questions-<repo>.json file should be authored from this
raw material, NOT from spelunk memory output.

Usage:
    python bench/memory/author_questions.py \\
        --repo-path /path/to/repo \\
        --num-commits 500 \\
        --out bench/memory/raw-commits-ripgrep.json
"""

import argparse
import json
import subprocess
from pathlib import Path


def export_git_log(repo_path: Path, num_commits: int) -> list[dict]:
    cmd = [
        "git",
        "--no-pager",
        "log",
        "--max-count",
        str(num_commits),
        "--format=%H%x00%s%x00%b%x00---",
    ]
    result = subprocess.run(cmd, cwd=repo_path, capture_output=True, text=True)

    commits = []
    for block in result.stdout.strip().split("---"):
        block = block.strip()
        if not block:
            continue
        parts = block.split("\0")
        if len(parts) >= 2:
            commits.append(
                {
                    "commit": parts[0].strip(),
                    "subject": parts[1].strip(),
                    "body": parts[2].strip() if len(parts) > 2 else "",
                }
            )
    return commits


def main():
    parser = argparse.ArgumentParser(
        description="Export raw git log for blind question authoring."
    )
    parser.add_argument("--repo-path", required=True)
    parser.add_argument("--num-commits", type=int, default=500)
    parser.add_argument("--out", required=True)
    args = parser.parse_args()

    repo_path = Path(args.repo_path).resolve()
    if not repo_path.is_dir():
        parser.error(f"repo-path does not exist: {repo_path}")

    commits = export_git_log(repo_path, args.num_commits)
    output = {
        "repo": str(repo_path),
        "num_commits_exported": len(commits),
        "instructions": (
            "Author questions from the commits below. Do NOT consult spelunk "
            "memory (spelunk memory list / spelunk memory search). Use the raw "
            "commit subjects and bodies as your only source material. Record "
            "ground-truth commit SHAs for each question."
        ),
        "commits": commits,
    }

    Path(args.out).parent.mkdir(parents=True, exist_ok=True)
    with open(args.out, "w") as f:
        json.dump(output, f, indent=2)
    print(f"Exported {len(commits)} commits to {args.out}")


if __name__ == "__main__":
    main()
