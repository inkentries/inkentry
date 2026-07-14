"""Provenance additive-only contract test.

Run:
    uv run --with pytest pytest bench/agents/tests/ -v

Verifies each harness's result JSON still carries the pre-existing
(pre-harness-matrix) reproducibility fields, plus the new harness-matrix
fields -- additive only, per bench/agents/README.md "Reproducibility /
provenance contract". Exercised against all three harnesses: agent.py
(harness=none), harness_opencode.py, and harness_claude_code.py.

Cheapest fully-offline route for the two subprocess-based harnesses:
rather than shelling out to the real `claude`/`opencode` binaries (not
guaranteed to be installed, and would be a live invocation), this drives
harness_claude_code.py's and harness_opencode.py's actual CLI end-to-end
via subprocess with fake `claude`/`opencode` executables shimmed onto
PATH. That exercises the real provenance-dict-construction code path in
each script's main() with zero network access and zero real coding-agent
invocation.

agent.py (harness=none) is different in kind: it isn't a subprocess
wrapper around an external binary, it *is* the OpenAI-compatible
tool-calling loop, so there's no external process to shim. Importing it
directly would normally pull in the real `openai` (and `dotenv`) packages
-- neither a bare `pytest` dependency -- which would make `uv run --with
pytest pytest ...` fail to collect this file on a machine that hasn't
separately installed them. Instead, fake `openai`/`dotenv` modules are
injected into sys.modules before import (see `_stub_openai_and_dotenv`
below): a minimal fake `OpenAI` client whose `chat.completions.create`
returns one canned non-tool-call response, so `run_agent`'s loop exits
after a single turn with zero network access, and a no-op `load_dotenv`.
This keeps the suite offline and dependency-light (no extra `--with`
flags needed) while still exercising agent.py's real `main()` and its
provenance-dict construction.
"""

import json
import os
import stat
import subprocess
import sys
import types
from pathlib import Path

import pytest

AGENTS_DIR = Path(__file__).resolve().parents[1]
HARNESS_CLAUDE_CODE = AGENTS_DIR / "harness_claude_code.py"
HARNESS_OPENCODE = AGENTS_DIR / "harness_opencode.py"
AGENT_PY = AGENTS_DIR / "agent.py"

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


FAKE_OPENCODE_SHIM = """#!/usr/bin/env bash
# Fake `opencode` binary for offline provenance testing: emits a minimal
# --format json event stream shaped like a real `opencode run` result,
# without invoking any real model or network call.
echo '{"type": "step", "usage": {"input": 20, "output": 8}}'
echo '{"type": "step", "usage": {"input": 15, "output": 6}}'
"""


@pytest.fixture()
def fake_opencode_on_path(tmp_path):
    """Prepend a directory containing a fake `opencode` executable to PATH
    (so get_opencode_command() picks the PATH binary over the npx
    fallback), mirroring fake_claude_on_path above."""
    bin_dir = tmp_path / "fakebin"
    bin_dir.mkdir()
    opencode_path = bin_dir / "opencode"
    opencode_path.write_text(FAKE_OPENCODE_SHIM)
    opencode_path.chmod(
        opencode_path.stat().st_mode | stat.S_IEXEC | stat.S_IXGRP | stat.S_IXOTH
    )

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
        # condition defaults to "baseline" (not a harness-specific hardcoded
        # string like the old "claude_code_native"/"claude_code_deepseek") --
        # see README "Conditions": claude-code is generic, not
        # spelunk-instrumented, so condition is always baseline in practice.
        assert payload["condition"] == "baseline"

    def test_condition_flows_from_flag_not_hardcoded(
        self, fake_claude_on_path, throwaway_repo, tmp_path
    ):
        # Neither the old hardcoded strings
        # ("claude_code_deepseek"/"claude_code_native") nor the default
        # ("baseline"), to prove the output tracks --condition rather than a
        # fixed per-harness string. A real condition rather than a sentinel:
        # --condition is validated against the condition set.
        issue_file = tmp_path / "ISSUE.txt"
        issue_file.write_text("Fix the bug.")

        result = subprocess.run(
            [
                sys.executable,
                str(HARNESS_CLAUDE_CODE),
                "--task-id",
                "fake__task-1b",
                "--repo-path",
                str(throwaway_repo),
                "--issue",
                str(issue_file),
                "--no-deepseek",
                "--condition",
                "spelunk_search",
            ],
            capture_output=True,
            text=True,
            timeout=30,
            env=fake_claude_on_path,
        )

        assert result.returncode == 0, result.stderr
        line = [l for l in result.stdout.splitlines() if l.startswith("{")][-1]
        payload = json.loads(line)
        assert payload["condition"] == "spelunk_search"


class TestOpencodeProvenanceAdditiveContract:
    def test_opencode_result_is_strict_superset(
        self, fake_opencode_on_path, throwaway_repo, tmp_path
    ):
        issue_file = tmp_path / "ISSUE.txt"
        issue_file.write_text("Fix the bug.")

        result = subprocess.run(
            [
                sys.executable,
                str(HARNESS_OPENCODE),
                "--task-id",
                "fake__task-2",
                "--repo-path",
                str(throwaway_repo),
                "--issue",
                str(issue_file),
                "--model",
                "deepseek-v4-flash",
                "--api-key",
                "sk-fake-not-a-real-key",
            ],
            capture_output=True,
            text=True,
            timeout=30,
            env=fake_opencode_on_path,
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
        assert payload["task_id"] == "fake__task-2"
        assert payload["harness"] == "opencode"
        assert payload["turns"] == 2
        assert payload["input_tokens"] == 35
        assert payload["output_tokens"] == 14
        assert payload["resolved"] is False
        # condition defaults to "baseline" (not the old hardcoded
        # "opencode_deepseek") -- see README "Conditions".
        assert payload["condition"] == "baseline"
        # opencode has no claude-code effort/thinking concept -- always null
        # (README "Reproducibility / provenance contract").
        assert payload["effort"] is None
        assert payload["thinking"] is None
        # The scratch opencode.json must never survive the run (README
        # "Adapter notes" -- would otherwise leak the fake API key into
        # repo state).
        assert not (throwaway_repo / "opencode.json").exists()


def _install_fake_openai_and_dotenv(monkeypatch, canned_content: str):
    """Inject minimal fake `openai` and `dotenv` modules into sys.modules so
    agent.py can be imported (and its main() driven end-to-end) without the
    real packages installed, and without any network access.

    The fake OpenAI client's chat.completions.create returns one canned
    response with finish_reason != "tool_calls", so run_agent()'s while
    loop executes exactly one turn and returns immediately -- no tool
    dispatch, no real model call.
    """

    class _FakeUsage:
        def __init__(self, prompt_tokens, completion_tokens):
            self.prompt_tokens = prompt_tokens
            self.completion_tokens = completion_tokens

    class _FakeMessage:
        def __init__(self, content):
            self.content = content
            self.tool_calls = None
            # Deliberately no `reasoning_content` attribute, so agent.py's
            # `hasattr(msg, "reasoning_content")` branch is exercised as
            # False, matching a plain (non-DeepSeek-thinking) response.

    class _FakeChoice:
        def __init__(self, content):
            self.message = _FakeMessage(content)
            self.finish_reason = "stop"

    class _FakeResponse:
        def __init__(self, content):
            self.choices = [_FakeChoice(content)]
            self.usage = _FakeUsage(prompt_tokens=7, completion_tokens=3)

    class _FakeCompletions:
        def create(self, **kwargs):
            return _FakeResponse(canned_content)

    class _FakeChat:
        def __init__(self):
            self.completions = _FakeCompletions()

    class FakeOpenAI:
        def __init__(self, *args, **kwargs):
            self.chat = _FakeChat()

    fake_openai_module = types.ModuleType("openai")
    fake_openai_module.OpenAI = FakeOpenAI
    monkeypatch.setitem(sys.modules, "openai", fake_openai_module)

    fake_dotenv_module = types.ModuleType("dotenv")
    fake_dotenv_module.load_dotenv = lambda *args, **kwargs: None
    monkeypatch.setitem(sys.modules, "dotenv", fake_dotenv_module)


@pytest.fixture()
def agent_module(monkeypatch):
    """Import agent.py with fake openai/dotenv modules in place, reloading
    it fresh each test so module-level state (e.g. which fake OpenAI class
    is bound) never leaks between tests."""
    _install_fake_openai_and_dotenv(monkeypatch, canned_content="Fixed the bug.")

    import importlib

    module_name = "agent"
    if module_name in sys.modules:
        del sys.modules[module_name]
    agent = importlib.import_module(module_name)
    yield agent
    if module_name in sys.modules:
        del sys.modules[module_name]


class TestAgentPyProvenanceAdditiveContract:
    """harness=none: agent.py is imported directly (with openai/dotenv
    stubbed, see _install_fake_openai_and_dotenv) rather than driven as a
    subprocess, since it's the tool-calling loop itself, not a wrapper
    around an external harness binary."""

    def test_harness_none_result_is_strict_superset(
        self, agent_module, throwaway_repo, tmp_path, monkeypatch
    ):
        monkeypatch.setattr(
            sys,
            "argv",
            [
                "agent.py",
                "--condition",
                "baseline",
                "--task-id",
                "fake__task-3",
                "--repo-path",
                str(throwaway_repo),
                "--issue",
                "Fix the bug.",
                "--api-key",
                "sk-fake-not-a-real-key",
            ],
        )

        captured = {}
        monkeypatch.setattr(
            "builtins.print", lambda s: captured.setdefault("stdout", s)
        )

        agent_module.main()

        assert "stdout" in captured, "agent.py main() did not print a result"
        payload = json.loads(captured["stdout"])

        missing_pre_existing = PRE_EXISTING_FIELDS - payload.keys()
        assert not missing_pre_existing, (
            f"pre-existing provenance fields dropped (not additive-only): {missing_pre_existing}"
        )

        missing_new = NEW_HARNESS_FIELDS - payload.keys()
        assert not missing_new, f"documented new harness fields missing: {missing_new}"

        # Sanity on a few actual values, not just key presence.
        assert payload["task_id"] == "fake__task-3"
        assert payload["harness"] == "none"
        assert payload["harness_version"] is None
        assert payload["turns"] == 1
        assert payload["input_tokens"] == 7
        assert payload["output_tokens"] == 3
        assert payload["resolved"] is False
        # harness=none has no claude-code effort/thinking concept -- always
        # null (README "Reproducibility / provenance contract").
        assert payload["effort"] is None
        assert payload["thinking"] is None
