"""Tests for bench/agents/harness_common.py.

Run:
    uv run --with pytest pytest bench/agents/tests/ -v

Network-free, no DEEPSEEK_API_KEY, no opencode/claude binaries required.
Uses throwaway git repos under tmp_path (pytest's built-in tmp dir fixture).
"""

import json
import subprocess
from pathlib import Path

import pytest

from harness_common import extract_patch


def _git(repo: Path, *args: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["git", *args],
        cwd=repo,
        capture_output=True,
        text=True,
        check=True,
    )


def _init_repo(repo: Path) -> None:
    repo.mkdir(parents=True, exist_ok=True)
    _git(repo, "init", "-q")
    _git(repo, "config", "user.email", "test@example.com")
    _git(repo, "config", "user.name", "Test")


class TestExtractPatchNormalCase:
    def test_modified_tracked_file_yields_nonempty_patch(self, tmp_path):
        repo = tmp_path / "repo"
        _init_repo(repo)
        (repo / "README.md").write_text("hello\n")
        _git(repo, "add", "README.md")
        _git(repo, "commit", "-q", "-m", "init")

        (repo / "README.md").write_text("hello\nworld\n")

        patch_file = tmp_path / "out" / "task.patch"
        result = extract_patch(repo, str(patch_file))

        assert result == patch_file
        assert patch_file.exists()
        content = patch_file.read_text()
        assert content.strip() != ""
        assert "README.md" in content
        assert "+world" in content


class TestExtractPatchRegressionCase:
    """The silent-null bug: the old code did `git add -- *.py *.rs *.md ...`
    as one call. If ANY listed extension has zero matches anywhere in the
    repo, `git add` exits 128 ("pathspec '*.py' did not match any files")
    and — confirmed empirically — stages *nothing*, not even the
    extensions that did match. Since a real repo only ever contains a
    handful of the ~18 allowlisted extensions, this fired on essentially
    every real SWE-bench checkout and produced an empty/missing patch that
    looked like "the agent made no changes" instead of "patch extraction
    is broken".

    harness_common.extract_patch fixes this by asking `git diff
    --name-only` / `git ls-files --others --exclude-standard` (both
    tolerate unmatched pathspecs) which allowlisted files actually
    changed, then `git add --` only that concrete file list.
    """

    def test_untracked_file_with_no_matches_for_other_extensions(self, tmp_path):
        # Only a .py file changes (untracked/new) -- every other allowlisted
        # extension (*.rs, *.ts, *.md, ...) has zero matches anywhere in this
        # repo. The old single `git add -- <full allowlist>` call would hit
        # "pathspec '*.rs' did not match any files", exit 128, and stage
        # nothing -- silently producing a null patch despite a real change
        # sitting in the working tree.
        repo = tmp_path / "repo"
        _init_repo(repo)
        (repo / "README.md").write_text("placeholder\n")
        _git(repo, "add", "README.md")
        _git(repo, "commit", "-q", "-m", "init")

        (repo / "new_file.py").write_text("print('fix applied')\n")

        patch_file = tmp_path / "out" / "task.patch"
        result = extract_patch(repo, str(patch_file))

        assert result == patch_file
        content = patch_file.read_text()
        assert content.strip() != "", (
            "regression: patch is empty -- the allowlist git-add bug is back"
        )
        assert "new_file.py" in content
        assert "+print('fix applied')" in content

    def test_modified_tracked_file_of_rare_extension(self, tmp_path):
        # A modified *tracked* file whose extension (.go) is the only
        # allowlisted extension present in the repo -- same failure mode,
        # via the "modified" path rather than the "untracked" path.
        repo = tmp_path / "repo"
        _init_repo(repo)
        (repo / "main.go").write_text("package main\n")
        _git(repo, "add", "main.go")
        _git(repo, "commit", "-q", "-m", "init")

        (repo / "main.go").write_text("package main\n\nfunc fix() {}\n")

        patch_file = tmp_path / "out" / "task.patch"
        extract_patch(repo, str(patch_file))

        content = patch_file.read_text()
        assert "func fix()" in content


class TestExtractPatchNoChanges:
    def test_no_changes_returns_empty_patch_gracefully(self, tmp_path):
        repo = tmp_path / "repo"
        _init_repo(repo)
        (repo / "README.md").write_text("hello\n")
        _git(repo, "add", "README.md")
        _git(repo, "commit", "-q", "-m", "init")

        patch_file = tmp_path / "out" / "task.patch"
        result = extract_patch(repo, str(patch_file))

        # Should not raise/crash; patch path is returned and file exists but
        # is empty (nothing changed since HEAD).
        assert result == patch_file
        assert patch_file.exists()
        assert patch_file.read_text().strip() == ""

    def test_no_save_patch_requested_returns_none(self, tmp_path):
        repo = tmp_path / "repo"
        _init_repo(repo)
        (repo / "README.md").write_text("hello\n")
        _git(repo, "add", "README.md")
        _git(repo, "commit", "-q", "-m", "init")

        assert extract_patch(repo, None) is None

    def test_non_allowlisted_extension_change_yields_empty_patch(self, tmp_path):
        # A change to a file whose extension isn't in SOURCE_PATHSPECS at
        # all (e.g. a stray .lock file) should not appear in the patch, and
        # should not crash extraction either.
        repo = tmp_path / "repo"
        _init_repo(repo)
        (repo / "README.md").write_text("hello\n")
        _git(repo, "add", "README.md")
        _git(repo, "commit", "-q", "-m", "init")

        (repo / "uv.lock").write_text("junk\n")

        patch_file = tmp_path / "out" / "task.patch"
        extract_patch(repo, str(patch_file))

        content = patch_file.read_text()
        assert "uv.lock" not in content
        assert content.strip() == ""


class TestExtractPatchErrorHandling:
    def test_nonexistent_repo_path_does_not_raise(self, tmp_path):
        missing = tmp_path / "does-not-exist"
        patch_file = tmp_path / "out" / "task.patch"

        # extract_patch catches its own exceptions and prints a warning to
        # stderr, returning None rather than propagating -- a single bad
        # task shouldn't crash a 50-task batch run.
        result = extract_patch(missing, str(patch_file))
        assert result is None


class TestRawOutputLastJsonLineExtraction:
    """Covers swebench_run.sh's `RESULT=$(printf '%s\\n' "$RAW_OUTPUT" | grep
    '^{' | tail -1)` convention: a runner's real JSON result is always the
    last '{'-prefixed line of stdout+stderr combined, even when earlier
    lines contain non-fatal warnings (e.g. harness_common's own "Warning:
    failed to save patch: ..." on stderr, merged in via 2>&1)."""

    def _extract_last_json_line(self, raw_output: str) -> str:
        """Python re-implementation of the exact shell pipeline in
        swebench_run.sh, so this test exercises the same selection logic
        without needing to shell out."""
        lines = [line for line in raw_output.splitlines() if line.startswith("{")]
        return lines[-1] if lines else ""

    def test_noise_before_json_line_is_ignored(self):
        raw_output = (
            "Warning: failed to save patch: some transient git error\n"
            "some other stray stdout line\n"
            '{"task_id": "django__django-11099", "turns": 3, "resolved": false}\n'
        )
        result = self._extract_last_json_line(raw_output)
        assert result == '{"task_id": "django__django-11099", "turns": 3, "resolved": false}'
        parsed = json.loads(result)
        assert parsed["task_id"] == "django__django-11099"

    def test_multiple_brace_prefixed_lines_takes_last(self):
        # e.g. a tool emits a JSON-shaped progress line mid-run, followed by
        # the actual final result -- must take the LAST one, not the first.
        raw_output = (
            '{"progress": "indexing", "pct": 50}\n'
            "some noise\n"
            '{"task_id": "t1", "turns": 5, "resolved": false}\n'
        )
        result = self._extract_last_json_line(raw_output)
        parsed = json.loads(result)
        assert parsed["task_id"] == "t1"

    def test_shell_pipeline_matches_python_reimplementation(self, tmp_path):
        # Belt-and-braces: actually run the real shell pipeline (no
        # network, no subprocess beyond /bin/sh) to make sure the Python
        # re-implementation above hasn't drifted from swebench_run.sh's
        # actual `grep '^{' | tail -1` logic.
        raw_output = (
            "Warning: failed to save patch: transient\n"
            '{"task_id": "t1", "turns": 1}\n'
            "trailing non-json noise\n"
            '{"task_id": "t1", "turns": 2, "resolved": false}\n'
        )
        proc = subprocess.run(
            "grep '^{' | tail -1",
            input=raw_output,
            shell=True,
            capture_output=True,
            text=True,
        )
        shell_result = proc.stdout.strip()
        assert shell_result == self._extract_last_json_line(raw_output)
        parsed = json.loads(shell_result)
        assert parsed["turns"] == 2

    def test_no_json_line_present(self):
        raw_output = "nothing but noise\nanother line\n"
        result = self._extract_last_json_line(raw_output)
        assert result == ""


if __name__ == "__main__":
    raise SystemExit(pytest.main([__file__, "-v"]))
