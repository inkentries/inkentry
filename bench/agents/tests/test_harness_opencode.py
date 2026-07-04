"""Tests for bench/agents/harness_opencode.py.

Run:
    uv run --with pytest pytest bench/agents/tests/ -v

Network-free, no DEEPSEEK_API_KEY, no opencode binary required (PATH lookup
and npx fallback are exercised via monkeypatched shutil.which).
"""

import json

from harness_opencode import PROVIDER_ID, get_opencode_command, write_provider_config


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
