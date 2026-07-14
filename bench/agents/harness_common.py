#!/usr/bin/env python3
"""Shared helpers for harness adapters (opencode, claude-code).

Keeps patch-extraction identical across every harness so that the only
varying dimension between conditions is the harness itself (never the
diffing/staging logic) — see bench/AGENTS.md binding principle #1.
"""

import subprocess
import sys
import time
from pathlib import Path

# Every condition agent.py accepts. The harness adapters validate against
# this rather than a per-harness subset: all three conditions now run under
# all three harnesses (spelunk tools reach opencode/claude-code over the
# bench-local MCP server, see spelunk_mcp_server.py).
CONDITIONS = ("baseline", "spelunk_search", "spelunk_full")

# Mirrors the sentence agent.py's SYSTEM_PROMPT_SPELUNK adds over
# SYSTEM_PROMPT_BASE. Not imported verbatim: agent.py's wording names bare
# tool names, but an MCP client's model only ever sees the namespaced
# `mcp__spelunk__*` spelling, so a verbatim copy would point the model at
# names that don't exist in these harnesses.
SPELUNK_PROMPT_GUIDANCE = (
    "You have access to spelunk tools for fast semantic code search, code "
    "graph traversal, and project memory retrieval — use them to locate "
    "relevant code and context before diving into files. They are available "
    "as these tools: {tools}."
)


def build_system_prompt(base_prompt: str, condition: str, tool_names: list[str]) -> str:
    """Mirror agent.py's get_system_prompt() split for a harness adapter.

    Without this the spelunk arm is handed tools it is never told to use —
    handicapped against agent.py's spelunk arm rather than comparable to it.
    base_prompt stays each harness's own so the baseline arm is untouched.
    """
    if condition == "baseline":
        return base_prompt
    return f"{base_prompt} {SPELUNK_PROMPT_GUIDANCE.format(tools=', '.join(tool_names))}"

# Same allowlist as agent.py's --save-patch handling. Keeping this as a
# single shared list (imported by every adapter) is the point: if the
# denylist-vs-allowlist tradeoff is ever revisited, it only needs to change
# in one place, and every harness stays in lockstep.
SOURCE_PATHSPECS = [
    "*.py",
    "*.rs",
    "*.ts",
    "*.tsx",
    "*.js",
    "*.jsx",
    "*.go",
    "*.java",
    "*.c",
    "*.cpp",
    "*.h",
    "*.rb",
    "*.sh",
    "*.toml",
    "*.json",
    "*.yaml",
    "*.yml",
    "*.md",
]


def _run_git(args: list[str], repo_path: Path, retries: int = 3, check: bool = True):
    """Run a git command, retrying on transient index-lock contention.

    Both opencode and claude-code are full coding-agent harnesses that may
    still be finishing their own background git operations (status checks,
    checkpointing) for a brief moment after the subprocess returns. A `git
    add` that lands in that window fails with exit 128 ("Unable to create
    '.git/index.lock': File exists") — observed in practice during adapter
    verification. agent.py never hits this because its write_file tool never
    touches git itself, so this retry has no equivalent upstream to mirror;
    it is a new, harness-adapter-specific need.
    """
    last_exc: Exception | None = None
    for attempt in range(retries):
        result = subprocess.run(
            args,
            cwd=repo_path,
            capture_output=True,
            text=True,
            timeout=30,
        )
        if result.returncode == 0 or not check:
            return result
        if "index.lock" in (result.stderr or "") and attempt < retries - 1:
            time.sleep(1)
            continue
        last_exc = subprocess.CalledProcessError(
            result.returncode, args, result.stdout, result.stderr
        )
        raise last_exc
    raise last_exc  # pragma: no cover — unreachable, satisfies type checkers


def extract_patch(repo_path: Path, save_patch: str | None) -> Path | None:
    """Stage known source-file extensions and write the cached diff to
    save_patch. Returns the patch path, or None if save_patch was not
    requested or extraction failed (a warning is printed to stderr).

    This is a git-diff-of-the-working-tree extraction, identical in shape
    for every harness: opencode and claude-code both edit files directly in
    repo_path, exactly like agent.py's write_file tool does, so the same
    staged-diff approach applies unchanged (spec point 3: "patch extraction
    unchanged").

    `git add -- <pathspecs>` treats *any* pathspec with zero matches across
    the whole call as a fatal error (exit 128 — "pathspec '*.rs' did not
    match any files") and — confirmed empirically against git 2.55 — stages
    *nothing at all* when this happens, not even the pathspecs that did
    match. Since every SWE-bench task repo only ever contains a handful of
    the ~18 listed source extensions, this fires on essentially every real
    run and would silently produce an empty (or missing) patch — the worst
    kind of failure for a benchmark harness, since "no patch" reads as "the
    agent produced no fix" rather than "patch extraction was broken so we
    never captured a real fix." The safe fix is to never pass a
    possibly-unmatched pathspec to `git add`: first ask `git diff
    --name-only` (tracked, modified) and `git ls-files --others
    --exclude-standard` (untracked, new) which allowlisted files actually
    changed — both tolerate unmatched pathspecs cleanly — then `git add --`
    only that concrete file list. (agent.py's own --save-patch handler
    already carries this identical fix inline — its comment explicitly
    keeps itself "in lockstep" with this function. It isn't deduplicated
    into a shared import here because touching agent.py's tool-calling loop
    is out of scope per spec point 2, "existing agent.py flow unchanged" —
    if this logic ever changes again, update both call sites.)
    """
    if not save_patch:
        return None
    try:
        modified = _run_git(
            ["git", "diff", "--name-only", "--", *SOURCE_PATHSPECS], repo_path
        ).stdout.splitlines()
        untracked = _run_git(
            ["git", "ls-files", "--others", "--exclude-standard", "--", *SOURCE_PATHSPECS],
            repo_path,
        ).stdout.splitlines()
        changed_files = [f for f in (*modified, *untracked) if f.strip()]

        if changed_files:
            _run_git(["git", "add", "--", *changed_files], repo_path)

        diff = _run_git(
            ["git", "diff", "--cached", "HEAD", "--", *SOURCE_PATHSPECS],
            repo_path,
        ).stdout
        patch_path = Path(save_patch)
        patch_path.parent.mkdir(parents=True, exist_ok=True)
        patch_path.write_text(diff)
        return patch_path
    except Exception as e:
        print(f"Warning: failed to save patch: {e}", file=sys.stderr)
        return None


def read_issue_text(issue_arg: str) -> str:
    """--issue accepts either inline text or a path to a file (agent.py
    convention, mirrored here for consistency across harnesses)."""
    issue_path = Path(issue_arg)
    if issue_path.is_file():
        return issue_path.read_text()
    return issue_arg
