"""Tests for bench/agents/harness_opencode.py.

Run:
    uv run --with pytest pytest bench/agents/tests/ -v

Network-free, no DEEPSEEK_API_KEY, no opencode binary required (PATH lookup
and npx fallback are exercised via monkeypatched shutil.which).
"""

import json

import pytest
from harness_opencode import (
    PROVIDER_ID,
    SERVER_NAME,
    get_opencode_command,
    write_provider_config,
)

SPELUNK_CONDITIONS = ["spelunk_search", "spelunk_full"]


def _config(repo_path, condition="baseline", telemetry_log=None):
    path = write_provider_config(
        repo_path,
        "deepseek-v4-flash",
        "https://api.deepseek.com/v1",
        "sk-test-123",
        condition=condition,
        telemetry_log=telemetry_log,
    )
    return json.loads(path.read_text())


class TestWriteProviderConfig:
    def test_config_matches_documented_opencode_json_schema(self, tmp_path):
        api_base_url = "https://api.deepseek.com/v1"
        api_key = "sk-test-123"
        model = "deepseek-v4-flash"

        config_path = write_provider_config(tmp_path, model, api_base_url, api_key)

        assert config_path == tmp_path / "opencode.json"
        assert config_path.exists()

        config = json.loads(config_path.read_text())

        # Top-level shape documented in README.md's opencode.json example.
        assert config["$schema"] == "https://opencode.ai/config.json"
        assert "provider" in config
        assert PROVIDER_ID in config["provider"]

        provider = config["provider"][PROVIDER_ID]
        assert provider["npm"] == "@ai-sdk/openai-compatible"
        assert provider["name"] == "DeepSeek (spelunk-bench)"
        assert provider["options"]["baseURL"] == api_base_url
        assert provider["options"]["apiKey"] == api_key
        assert provider["models"] == {model: {"name": model}}

    def test_config_written_scoped_to_repo_dir_not_global(self, tmp_path):
        # Written under the given repo_path, never under a global
        # ~/.config/opencode/ location -- so concurrent task runs never
        # race on a shared config file (README "Adapter notes").
        repo_a = tmp_path / "repo_a"
        repo_b = tmp_path / "repo_b"
        repo_a.mkdir()
        repo_b.mkdir()

        path_a = write_provider_config(
            repo_a, "deepseek-v4-flash", "https://api.deepseek.com/v1", "key-a"
        )
        path_b = write_provider_config(
            repo_b, "deepseek-v4-flash", "https://api.deepseek.com/v1", "key-b"
        )

        assert path_a == repo_a / "opencode.json"
        assert path_b == repo_b / "opencode.json"
        assert json.loads(path_a.read_text())["provider"][PROVIDER_ID]["options"]["apiKey"] == "key-a"
        assert json.loads(path_b.read_text())["provider"][PROVIDER_ID]["options"]["apiKey"] == "key-b"

    def test_config_is_valid_json_indented(self, tmp_path):
        config_path = write_provider_config(
            tmp_path, "deepseek-v4-flash", "https://api.deepseek.com/v1", "key"
        )
        text = config_path.read_text()
        # Written with indent=2 -- sanity check it isn't a single unindented line.
        assert "\n" in text
        json.loads(text)  # must parse cleanly


class TestMcpBlockIsConditionGated:
    """The spelunk tools reach opencode through an `mcp` block in the same
    generated, repo-scoped config. Gating is on the condition: a baseline arm
    that silently gained spelunk tools would invalidate results just as badly
    as a spelunk arm that never got them.
    """

    def test_only_spelunk_conditions_declare_an_mcp_block(self, tmp_path):
        # One predicate, both directions: the absence below is only evidence
        # because the same check finds the block when it is present.
        assert "mcp" in _config(tmp_path, condition="spelunk_search")
        assert "mcp" not in _config(tmp_path, condition="baseline")

    def test_default_condition_declares_no_mcp_block(self, tmp_path):
        assert "mcp" not in _config(tmp_path)

    @pytest.mark.parametrize("condition", SPELUNK_CONDITIONS)
    def test_block_matches_the_documented_local_server_schema(self, condition, tmp_path):
        # Shape per opencode.ai/config.json: local entries require type +
        # command; environment/enabled are optional.
        block = _config(tmp_path, condition=condition)["mcp"][SERVER_NAME]
        assert block["type"] == "local"
        assert isinstance(block["command"], list)
        assert all(isinstance(part, str) for part in block["command"])
        assert block["environment"] == {"SPELUNK_SECRET_STORE": "file"}
        assert block["enabled"] is True

    @pytest.mark.parametrize("condition", SPELUNK_CONDITIONS)
    def test_command_spawns_the_bench_server_for_this_condition(self, condition, tmp_path):
        command = _config(tmp_path, condition=condition)["mcp"][SERVER_NAME]["command"]
        assert command[1].endswith("spelunk_mcp_server.py")
        assert command[command.index("--condition") + 1] == condition
        assert command[command.index("--repo-path") + 1] == str(tmp_path.resolve())

    def test_telemetry_log_wired_through_when_requested(self, tmp_path):
        log = tmp_path / "calls.jsonl"
        command = _config(tmp_path, condition="spelunk_full", telemetry_log=log)["mcp"][
            SERVER_NAME
        ]["command"]
        assert command[command.index("--telemetry-log") + 1] == str(log)

    def test_provider_block_survives_on_a_spelunk_condition(self, tmp_path):
        # The mcp block is a sibling of provider, not a replacement.
        config = _config(tmp_path, condition="spelunk_full")
        assert config["provider"][PROVIDER_ID]["options"]["apiKey"] == "sk-test-123"
        assert config["$schema"] == "https://opencode.ai/config.json"


class TestSystemPromptNamesTheToolsOnSpelunkConditions:
    """Tools the model is never told about are a handicap, not a condition.
    agent.py swaps its whole prompt; a harness adapter can only restate the
    delta, so the restatement is pinned to agent.py's own text.
    """

    def test_guidance_core_is_an_exact_substring_of_agent_prompt(self):
        import agent
        from harness_common import SPELUNK_GUIDANCE_CORE

        assert SPELUNK_GUIDANCE_CORE in agent.SYSTEM_PROMPT_SPELUNK

    def test_baseline_prompt_is_untouched(self):
        from harness_common import build_system_prompt
        from harness_opencode import OPENCODE_SYSTEM_PROMPT

        assert (
            build_system_prompt(OPENCODE_SYSTEM_PROMPT, "baseline", [])
            == OPENCODE_SYSTEM_PROMPT
        )

    @pytest.mark.parametrize("condition", SPELUNK_CONDITIONS)
    def test_spelunk_prompt_names_every_tool_the_model_can_see(self, condition):
        from harness_common import build_system_prompt
        from harness_opencode import OPENCODE_SYSTEM_PROMPT
        from spelunk_mcp_server import mcp_tool_names_for_condition

        names = mcp_tool_names_for_condition(condition)
        prompt = build_system_prompt(OPENCODE_SYSTEM_PROMPT, condition, names)
        for name in names:
            assert name in prompt
        # Gating again: the prompt must not advertise a tool the condition
        # does not expose.
        if condition == "spelunk_search":
            assert "spelunk_graph" not in prompt


class TestGetOpencodeCommand:
    def test_prefers_installed_binary_when_on_path(self, monkeypatch):
        monkeypatch.setattr(
            "harness_opencode.shutil.which",
            lambda name: "/usr/local/bin/opencode" if name == "opencode" else None,
        )
        assert get_opencode_command() == ["opencode"]

    def test_falls_back_to_npx_when_not_on_path(self, monkeypatch):
        monkeypatch.setattr("harness_opencode.shutil.which", lambda name: None)
        assert get_opencode_command() == ["npx", "--yes", "opencode-ai@latest"]
