"""Provenance additive-only contract test.

Run:
    uv run --with pytest pytest bench/agents/tests/ -v

Verifies a harness=none-equivalent result JSON still carries the
pre-existing (pre-harness-matrix) reproducibility fields, plus the new
harness-matrix fields -- additive only, per bench/agents/README.md
"Reproducibility / provenance contract".

Cheapest fully-offline route: rather than importing agent.py (which pulls
in the `openai` package, not a bare `pytest` dependency, and would make
`uv run --with pytest pytest ...` fail to collect this file) or shelling
out to the real `claude` binary (not guaranteed to be installed, and would
be a live invocation), this drives harness_claude_code.py's actual CLI
end-to-end via subprocess with a fake `claude` executable shimmed onto
PATH. That exercises the real provenance-dict-construction code path in
main() with zero network access and zero real coding-agent invocation.
"""

import json
import os
import stat
import subprocess
import sys
from pathlib import Path

import pytest

AGENTS_DIR = Path(__file__).resolve().parents[1]
HARNESS_CLAUDE_CODE = AGENTS_DIR / "harness_claude_code.py"

# Fields that existed in the result JSON before the harness-matrix change
# (agent.py's original reproducibility contract, see bench/agents/README.md
# and the git history of agent.py's final `output = {...}` dict).
PRE_EXISTING_FIELDS = {
    "benchmark",
    "condition",
    "model",
    "model_source",
    "api_base_url",
    "api_key_source",
    "spelunk_version",
    "seed",
    "max_turns",
    "task_id",
    "patch_file",
    "turns",
    "input_tokens",
    "output_tokens",
    "wall_seconds",
    "resolved",
}

# New harness-matrix provenance fields (README "Reproducibility / provenance
# contract" table).
NEW_HARNESS_FIELDS = {
    "harness",
    "harness_version",
    "endpoint_kind",
    "effort",
    "thinking",
    "run_seed",
    "question_set_version",
    "instance_filter",
    "judge_model",
    "judge_version",
    "judge_error_rate",
}


FAKE_CLAUDE_SHIM = """#!/usr/bin/env bash
# Fake `claude` binary for offline provenance testing: emits a minimal
# --output-format json payload shaped like the real `claude -p` result
# object, without invoking any real model or network call.
echo '{"num_turns": 2, "usage": {"input_tokens": 10, "output_tokens": 5}, "is_error": false, "modelUsage": {}}'
"""


@pytest.fixture()
def fake_claude_on_path(tmp_path):
    """Prepend a directory containing a fake `claude` executable to PATH,
    and a fake `git` is NOT needed here (extract_patch works against a real
    throwaway repo). Returns the augmented env dict."""
    bin_dir = tmp_path / "fakebin"
    bin_dir.mkdir()
    claude_path = bin_dir / "claude"
    claude_path.write_text(FAKE_CLAUDE_SHIM)
    claude_path.chmod(claude_path.stat().st_mode | stat.S_IEXEC | stat.S_IXGRP | stat.S_IXOTH)

    env = dict(os.environ)
    env["PATH"] = f"{bin_dir}:{env.get('PATH', '')}"
    return env


@pytest.fixture()
def throwaway_repo(tmp_path):
    repo = tmp_path / "repo"
    repo.mkdir()
    subprocess.run(["git", "init", "-q"], cwd=repo, check=True)
    subprocess.run(["git", "config", "user.email", "test@example.com"], cwd=repo, check=True)
    subprocess.run(["git", "config", "user.name", "Test"], cwd=repo, check=True)
    (repo / "README.md").write_text("hello\n")
    subprocess.run(["git", "add", "README.md"], cwd=repo, check=True)
    subprocess.run(["git", "commit", "-q", "-m", "init"], cwd=repo, check=True)
    return repo


class TestProvenanceAdditiveContract:
    def test_harness_none_equivalent_result_is_strict_superset(
        self, fake_claude_on_path, throwaway_repo, tmp_path
    ):
        issue_file = tmp_path / "ISSUE.txt"
        issue_file.write_text("Fix the bug.")

        result = subprocess.run(
            [
                sys.executable,
                str(HARNESS_CLAUDE_CODE),
                "--task-id",
                "fake__task-1",
                "--repo-path",
                str(throwaway_repo),
                "--issue",
                str(issue_file),
                "--no-deepseek",
                "--effort",
                "high",
            ],
            capture_output=True,
            text=True,
            timeout=30,
            env=fake_claude_on_path,
        )

        assert result.returncode == 0, result.stderr
        line = [l for l in result.stdout.splitlines() if l.startswith("{")][-1]
        payload = json.loads(line)

        missing_pre_existing = PRE_EXISTING_FIELDS - payload.keys()
        assert not missing_pre_existing, (
            f"pre-existing provenance fields dropped (not additive-only): {missing_pre_existing}"
        )

        missing_new = NEW_HARNESS_FIELDS - payload.keys()
        assert not missing_new, f"documented new harness fields missing: {missing_new}"

        # Sanity on a few actual values, not just key presence.
        assert payload["task_id"] == "fake__task-1"
        assert payload["harness"] == "claude-code"
        assert payload["effort"] == "high"
        assert payload["turns"] == 2
        assert payload["input_tokens"] == 10
        assert payload["output_tokens"] == 5
        assert payload["resolved"] is False
        # --no-deepseek path: no live DeepSeek key/base-url involved.
        assert payload["endpoint_kind"] == "native"
