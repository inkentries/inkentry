"""Tests for bench/agents/harness_claude_code.py.

Run:
    uv run --with pytest pytest bench/agents/tests/ -v

Network-free, no DEEPSEEK_API_KEY, no claude binary required.
"""

import os

from harness_claude_code import _deepseek_anthropic_env


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
