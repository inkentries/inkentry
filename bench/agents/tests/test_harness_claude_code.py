"""Tests for bench/agents/harness_claude_code.py.

Run:
    uv run --with pytest pytest bench/agents/tests/ -v

Network-free, no DEEPSEEK_API_KEY, no claude binary required.
"""

import json
import os

import pytest
from harness_claude_code import (
    SERVER_NAME,
    _deepseek_anthropic_env,
    build_claude_cmd,
    write_mcp_config,
)
from spelunk_mcp_server import mcp_tool_names_for_condition

CONDITIONS = ["baseline", "spelunk_search", "spelunk_full"]
SPELUNK_CONDITIONS = ["spelunk_search", "spelunk_full"]


class TestDeepseekAnthropicEnv:
    """_deepseek_anthropic_env is the function responsible for redirecting
    Claude Code's Anthropic client at DeepSeek. Its own docstring calls out
    the stakes: "silently picking the wrong [env var] and falling through to
    the user's own Anthropic credentials... would misattribute a
    Claude-native run as a DeepSeek one." These are direct assertions on its
    output, rather than only exercising it indirectly through a fake `claude`
    shim that ignores env entirely (as the --no-deepseek provenance-contract
    test does)."""

    def test_sets_auth_token_not_just_api_key(self):
        # The var name DeepSeek's docs actually specify (see module
        # docstring/README citation) -- this is the one that must be right.
        env = _deepseek_anthropic_env(
            api_key="sk-test-key",
            model="deepseek-v4-flash",
            base_url="https://api.deepseek.com/anthropic",
        )
        assert env["ANTHROPIC_AUTH_TOKEN"] == "sk-test-key"

    def test_sets_belt_and_braces_api_key_alias_too(self):
        env = _deepseek_anthropic_env(
            api_key="sk-test-key",
            model="deepseek-v4-flash",
            base_url="https://api.deepseek.com/anthropic",
        )
        assert env["ANTHROPIC_API_KEY"] == "sk-test-key"

    def test_sets_base_url_and_model(self):
        env = _deepseek_anthropic_env(
            api_key="sk-test-key",
            model="deepseek-v4-flash",
            base_url="http://127.0.0.1:4000",
        )
        assert env["ANTHROPIC_BASE_URL"] == "http://127.0.0.1:4000"
        assert env["ANTHROPIC_MODEL"] == "deepseek-v4-flash"

    def test_does_not_mutate_real_process_environment(self, monkeypatch):
        monkeypatch.delenv("ANTHROPIC_BASE_URL", raising=False)
        _deepseek_anthropic_env(
            api_key="sk-test-key",
            model="deepseek-v4-flash",
            base_url="https://api.deepseek.com/anthropic",
        )
        assert "ANTHROPIC_BASE_URL" not in os.environ

    def test_preserves_unrelated_ambient_env_vars(self, monkeypatch):
        monkeypatch.setenv("SOME_UNRELATED_VAR", "keep-me")
        env = _deepseek_anthropic_env(
            api_key="sk-test-key",
            model="deepseek-v4-flash",
            base_url="https://api.deepseek.com/anthropic",
        )
        assert env["SOME_UNRELATED_VAR"] == "keep-me"


def _cmd(condition, mcp_config_path=None):
    return build_claude_cmd(
        prompt="fix it",
        effort="high",
        thinking=False,
        condition=condition,
        mcp_config_path=mcp_config_path,
    )


class TestStrictMcpConfig:
    """The bench host has its own MCP servers configured. Without
    --strict-mcp-config they load into *both* arms, so baseline and spelunk
    are contaminated alike and the numbers are unpublishable. Verified on
    this host during adapter work: a host server appeared in the run's
    mcp_servers without the flag, and mcp_servers was empty with it.
    """

    @pytest.mark.parametrize("condition", CONDITIONS)
    def test_passed_in_every_arm_including_baseline(self, condition, tmp_path):
        assert "--strict-mcp-config" in _cmd(condition, tmp_path / "mcp.json")

    def test_passed_on_baseline_even_with_no_mcp_config(self):
        # How main() actually calls it on baseline: flag still required.
        assert "--strict-mcp-config" in _cmd("baseline")


class TestMcpConfigIsConditionGated:
    def test_only_spelunk_arms_load_the_bench_server(self, tmp_path):
        # One predicate, both directions: the absence assertion below is only
        # evidence because the same check finds the flag when it is present.
        cfg = tmp_path / "mcp.json"
        assert "--mcp-config" in _cmd("spelunk_search", cfg)
        assert "--mcp-config" not in _cmd("baseline", cfg)

    def test_baseline_ignores_a_config_path_it_was_handed(self, tmp_path):
        # Gating is on the condition, not on the caller remembering to pass
        # None: a baseline arm that quietly gained spelunk tools would
        # invalidate results in the opposite direction.
        cfg = tmp_path / "mcp.json"
        assert str(cfg) not in _cmd("baseline", cfg)

    @pytest.mark.parametrize("condition", SPELUNK_CONDITIONS)
    def test_spelunk_arms_point_at_the_written_config(self, condition, tmp_path):
        cfg = tmp_path / "mcp.json"
        cmd = _cmd(condition, cfg)
        assert cmd[cmd.index("--mcp-config") + 1] == str(cfg)


class TestAllowedTools:
    """--permission-mode acceptEdits covers file edits, not MCP tool calls. A
    headless -p run that hits a permission prompt is a lost cell."""

    @pytest.mark.parametrize("condition", SPELUNK_CONDITIONS)
    def test_names_every_exposed_tool_in_full(self, condition, tmp_path):
        cmd = _cmd(condition, tmp_path / "mcp.json")
        expected = mcp_tool_names_for_condition(condition)
        assert set(expected) <= set(cmd)
        # Named individually rather than relying on the `mcp__spelunk`
        # server-wide shorthand, which is unverified.
        assert f"mcp__{SERVER_NAME}" not in cmd

    def test_allow_list_is_condition_gated(self, tmp_path):
        cfg = tmp_path / "mcp.json"
        search = _cmd("spelunk_search", cfg)
        full = _cmd("spelunk_full", cfg)
        assert f"mcp__{SERVER_NAME}__spelunk_graph" not in search
        assert f"mcp__{SERVER_NAME}__spelunk_graph" in full

    def test_absent_on_baseline(self, tmp_path):
        cfg = tmp_path / "mcp.json"
        assert "--allowedTools" in _cmd("spelunk_search", cfg)
        assert "--allowedTools" not in _cmd("baseline", cfg)


class TestWriteMcpConfig:
    @pytest.mark.parametrize("condition", SPELUNK_CONDITIONS)
    def test_registers_the_bench_server_under_mcp_servers(self, condition, tmp_path):
        repo = tmp_path / "repo"
        repo.mkdir()
        path = write_mcp_config(tmp_path, condition, repo, None)
        server = json.loads(path.read_text())["mcpServers"][SERVER_NAME]
        assert server["args"][0].endswith("spelunk_mcp_server.py")
        assert server["args"][server["args"].index("--condition") + 1] == condition
        assert server["env"]["SPELUNK_SECRET_STORE"] == "file"

    def test_written_outside_the_task_repo(self, tmp_path):
        # Anything written inside repo_path can land in the extracted patch.
        repo = tmp_path / "repo"
        repo.mkdir()
        scratch = tmp_path / "scratch"
        scratch.mkdir()
        path = write_mcp_config(scratch, "spelunk_search", repo, None)
        assert repo not in path.parents
