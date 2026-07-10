"""Argument-validation tests for bench/agents/swebench_run.sh.

Run:
    uv run --with pytest pytest bench/agents/tests/ -v

These only exercise swebench_run.sh's arg-parsing / early-validation code
paths (before any task loop, agent subprocess, or network call would
happen), by asserting on exit codes + stderr text. No live API calls, no
DEEPSEEK_API_KEY, no repos checked out.
"""

import shutil
import subprocess
from pathlib import Path

import pytest

SCRIPT = Path(__file__).resolve().parents[1] / "swebench_run.sh"
RESULTS_DIR = SCRIPT.parent.parent / "results"
PATCHES_DIR = SCRIPT.parent.parent / "patches"


def run_script(args, env=None):
    return subprocess.run(
        ["bash", str(SCRIPT), *args],
        capture_output=True,
        text=True,
        timeout=30,
        env=env,
    )


@pytest.fixture(autouse=True)
def _no_stray_results_artifacts():
    """Safety net: every test here is expected to fail validation before
    swebench_run.sh's task loop ever creates bench/results/*.json or
    bench/patches/<...>/ (arg-parsing/validation happens first in the
    script). Snapshot before/after so a future shell-portability change
    (e.g. GNU head not erroring on `-n 0` the way BSD head does) can't
    leave stray artifacts in the repo without at least failing loudly here."""
    before_results = set(RESULTS_DIR.glob("*")) if RESULTS_DIR.exists() else set()
    before_patches = set(PATCHES_DIR.glob("*")) if PATCHES_DIR.exists() else set()
    yield
    after_results = set(RESULTS_DIR.glob("*")) if RESULTS_DIR.exists() else set()
    after_patches = set(PATCHES_DIR.glob("*")) if PATCHES_DIR.exists() else set()
    new_results = after_results - before_results
    new_patches = after_patches - before_patches
    for p in new_results:
        p.unlink(missing_ok=True)
    for p in new_patches:
        shutil.rmtree(p, ignore_errors=True)
    assert not new_results, f"unexpected results artifact(s) created: {new_results}"
    assert not new_patches, f"unexpected patches dir(s) created: {new_patches}"


class TestHarnessEnumValidation:
    def test_rejects_unknown_harness_value(self):
        result = run_script(
            ["--condition", "baseline", "--harness", "bogus-harness", "--api-key", "fake-key"]
        )
        assert result.returncode != 0
        assert "--harness must be one of none|opencode|claude-code" in result.stderr
        assert "bogus-harness" in result.stderr

    def test_accepts_opencode_and_claude_code_spellings(self):
        # Neither should trip the enum-rejection branch. Missing --condition
        # is checked before the harness enum switch, so pair each with a
        # valid --condition but rely on the tasks-file/repo-dir path being
        # unreachable in a plain checkout (no repos set up) to keep this
        # fast and network-free -- we only assert the specific "must be one
        # of" error text is ABSENT, not that the whole script succeeds.
        for harness in ("opencode", "claude-code"):
            result = run_script(
                ["--condition", "baseline", "--harness", harness, "--api-key", "fake-key", "--tasks", "0"]
            )
            assert "--harness must be one of" not in result.stderr


class TestConditionMustBeBaselineForNonNoneHarness:
    """opencode/claude-code are generic coding agents, not
    spelunk-instrumented, so spelunk_search/spelunk_full don't apply to them
    (README "Conditions"). Enforced here rather than just documented, so a
    mismatched --condition can't silently produce a result JSON whose
    condition field disagrees with what --condition claimed to request."""

    def test_rejects_non_baseline_condition_for_opencode(self):
        result = run_script(
            ["--condition", "spelunk_full", "--harness", "opencode", "--api-key", "fake-key"]
        )
        assert result.returncode != 0
        assert "--condition must be baseline for --harness opencode" in result.stderr

    def test_rejects_non_baseline_condition_for_claude_code(self):
        result = run_script(
            ["--condition", "spelunk_search", "--harness", "claude-code", "--api-key", "fake-key"]
        )
        assert result.returncode != 0
        assert "--condition must be baseline for --harness claude-code" in result.stderr

    def test_baseline_condition_passes_for_opencode(self):
        result = run_script(
            ["--condition", "baseline", "--harness", "opencode", "--api-key", "fake-key", "--tasks", "0"]
        )
        assert "--condition must be baseline" not in result.stderr

    def test_non_baseline_condition_still_allowed_for_harness_none(self):
        result = run_script(
            ["--condition", "spelunk_full", "--harness", "none", "--api-key", "fake-key", "--tasks", "0"]
        )
        assert "--condition must be baseline" not in result.stderr


class TestConditionRequired:
    def test_missing_condition_errors(self):
        result = run_script(["--harness", "none", "--api-key", "fake-key"])
        assert result.returncode != 0
        assert "--condition is required" in result.stderr


class TestEndpointKindShimRequiresShimBaseUrl:
    def test_shim_without_shim_base_url_errors(self):
        result = run_script(
            [
                "--condition",
                "baseline",
                "--harness",
                "claude-code",
                "--endpoint-kind",
                "shim",
                "--api-key",
                "fake-key",
            ]
        )
        assert result.returncode != 0
        assert "--shim-base-url is required" in result.stderr

    def test_shim_with_shim_base_url_passes_that_check(self):
        result = run_script(
            [
                "--condition",
                "baseline",
                "--harness",
                "claude-code",
                "--endpoint-kind",
                "shim",
                "--shim-base-url",
                "http://127.0.0.1:4000",
                "--api-key",
                "fake-key",
                "--tasks",
                "0",
            ]
        )
        # The specific shim-base-url error must not fire once one is given.
        assert "--shim-base-url is required" not in result.stderr

    def test_anthropic_compat_default_does_not_require_shim_base_url(self):
        result = run_script(
            ["--condition", "baseline", "--harness", "claude-code", "--api-key", "fake-key", "--tasks", "0"]
        )
        assert "--shim-base-url is required" not in result.stderr


class TestNoDeepseekSkipsApiKeyRequirement:
    def test_no_deepseek_without_api_key_does_not_error_on_missing_key(self, monkeypatch):
        env = {k: v for k, v in __import__("os").environ.items() if k != "DEEPSEEK_API_KEY"}
        result = run_script(
            ["--condition", "baseline", "--harness", "claude-code", "--no-deepseek", "--tasks", "0"],
            env=env,
        )
        assert "No API key" not in result.stderr

    def test_missing_api_key_errors_without_no_deepseek(self, monkeypatch):
        env = {k: v for k, v in __import__("os").environ.items() if k != "DEEPSEEK_API_KEY"}
        result = run_script(["--condition", "baseline", "--harness", "none"], env=env)
        assert result.returncode != 0
        assert "No API key" in result.stderr

    def test_missing_api_key_errors_for_opencode_harness_too(self, monkeypatch):
        env = {k: v for k, v in __import__("os").environ.items() if k != "DEEPSEEK_API_KEY"}
        result = run_script(["--condition", "baseline", "--harness", "opencode"], env=env)
        assert result.returncode != 0
        assert "No API key" in result.stderr


class TestHelpFlag:
    def test_help_flag_exits_and_prints_usage(self):
        result = run_script(["-h"])
        assert "--harness" in result.stdout
        assert "none|opencode|claude-code" in result.stdout
