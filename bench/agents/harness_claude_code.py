#!/usr/bin/env python3
"""SWE-bench single-task runner — claude-code harness, headless.

Sibling of agent.py and harness_opencode.py, but shells out to `claude -p`
(Claude Code's non-interactive mode) instead of a hand-rolled tool-calling
loop or opencode. Only the harness varies between cells — task repo, issue
text, and model identity are held constant (bench/AGENTS.md principle #1).

Two endpoint modes for reaching DeepSeek from Claude Code:

  native / anthropic-compat (default)
      DeepSeek's documented Anthropic-compatible endpoint, selected via env
      overrides: ANTHROPIC_BASE_URL=https://api.deepseek.com/anthropic,
      ANTHROPIC_AUTH_TOKEN=$DEEPSEEK_API_KEY, ANTHROPIC_MODEL=deepseek-v4-flash.
      Verified against DeepSeek's live docs (api-docs.deepseek.com) at
      build time — see bench/agents/README.md for the citation and for the
      one caveat: the docs show ANTHROPIC_AUTH_TOKEN, not ANTHROPIC_API_KEY;
      using the wrong var name is a documented silent-failure mode elsewhere
      in this codebase's tooling, so both are exported defensively (see
      _deepseek_anthropic_env below).

  shim (fallback, --endpoint-kind shim)
      An Anthropic ->OpenAI proxy (e.g. LiteLLM) in front of the DeepSeek
      OpenAI-compatible endpoint, for use if the native compat endpoint
      misbehaves (tool-call formatting, streaming, etc). Recorded verbatim
      in provenance as endpoint_kind: "shim" so results are never silently
      conflated with the native path. This script does not run the shim
      process itself — point --shim-base-url at an already-running proxy.

For non-DeepSeek (regular Claude-model) cells, no env overrides are applied
and endpoint_kind is "native" (Anthropic's own API). Effort/thinking are
always pinned and recorded, per spec point 4, so future Claude-model cells
stay reproducible.

Usage:
    python bench/agents/harness_claude_code.py \\
        --task-id django__django-11099 \\
        --repo-path /path/to/repo \\
        --issue bench/repos/django__django-11099/ISSUE.txt \\
        --model deepseek-v4-flash \\
        --api-key "$DEEPSEEK_API_KEY" \\
        [--effort high] [--seed 42] [--save-patch bench/patches/.../task.patch]

    # Anthropic-compat endpoint misbehaves -> shim fallback:
    python bench/agents/harness_claude_code.py \\
        --task-id django__django-11099 --repo-path ... --issue ... \\
        --model deepseek-v4-flash --api-key "$DEEPSEEK_API_KEY" \\
        --endpoint-kind shim --shim-base-url http://127.0.0.1:4000

Output: single JSON object on stdout (same reproducibility contract as
agent.py, plus the harness-dimension fields — see bench/agents/README.md).
"""

import argparse
import json
import os
import subprocess
import time
from pathlib import Path

from harness_common import extract_patch, read_issue_text

DEEPSEEK_ANTHROPIC_BASE_URL = "https://api.deepseek.com/anthropic"

CLAUDE_CODE_PROMPT_PREFIX = (
    "You are an expert software engineer. You are given a GitHub issue and a "
    "repository checkout. Your goal is to produce a minimal patch that fixes the "
    "issue. Explore the codebase, understand the problem, and apply the fix "
    "directly by editing files in the repository. When you are done, briefly "
    "summarise what you changed."
)


def get_claude_version() -> str:
    try:
        result = subprocess.run(
            ["claude", "--version"], capture_output=True, text=True, timeout=10
        )
        if result.returncode == 0:
            return result.stdout.strip()
    except Exception:
        pass
    return "unknown"


def _deepseek_anthropic_env(api_key: str, model: str, base_url: str) -> dict:
    """Env overrides that redirect Claude Code's Anthropic client at
    DeepSeek's Anthropic-compatible endpoint.

    Exports both ANTHROPIC_AUTH_TOKEN (the variable name confirmed against
    DeepSeek's live docs, api-docs.deepseek.com/guides/anthropic_api and
    .../quick_start/agent_integrations/claude_code, 2026-07-04) and
    ANTHROPIC_API_KEY (belt-and-braces, in case a Claude Code version reads
    the more generic name instead) — see bench/agents/README.md for the
    citation. Exporting an unused extra env var is harmless; silently
    picking the wrong one and falling through to the user's own Anthropic
    credentials would not be (it would misattribute a Claude-native run as
    a DeepSeek one).
    """
    env = dict(os.environ)
    env["ANTHROPIC_BASE_URL"] = base_url
    env["ANTHROPIC_AUTH_TOKEN"] = api_key
    env["ANTHROPIC_API_KEY"] = api_key
    env["ANTHROPIC_MODEL"] = model
    return env


def run_claude_code(
    repo_path: Path,
    issue_text: str,
    model: str,
    effort: str,
    thinking: bool,
    env: dict,
) -> dict:
    prompt = (
        f"{CLAUDE_CODE_PROMPT_PREFIX}\n\n"
        f"Repository path: {repo_path}\n\nIssue:\n{issue_text}\n\n"
        "Please investigate the issue and apply a fix."
    )

    cmd = [
        "claude",
        "-p",
        prompt,
        "--output-format",
        "json",
        "--permission-mode",
        "acceptEdits",  # headless: accept file edits without a TTY prompt
        "--effort",
        effort,
    ]
    if thinking:
        cmd += ["--append-system-prompt", "Think step by step before acting."]

    start = time.monotonic()
    result = subprocess.run(
        cmd,
        cwd=repo_path,
        capture_output=True,
        text=True,
        timeout=900,
        env=env,
    )
    wall_seconds = round(time.monotonic() - start, 2)

    turns = 0
    input_tokens = 0
    output_tokens = 0
    harness_error = None
    resolved_model_usage = {}

    try:
        payload = json.loads(result.stdout.strip().splitlines()[-1])
        turns = payload.get("num_turns", 0)
        usage = payload.get("usage", {})
        input_tokens = usage.get("input_tokens", 0) + usage.get(
            "cache_creation_input_tokens", 0
        )
        output_tokens = usage.get("output_tokens", 0)
        resolved_model_usage = payload.get("modelUsage", {})
        if payload.get("is_error"):
            harness_error = payload.get("result", "unknown claude -p error")
    except (json.JSONDecodeError, IndexError, AttributeError):
        harness_error = (result.stderr or result.stdout)[:2000]

    return {
        "resolved": False,  # determined externally by SWE-bench harness
        "turns": turns,
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "wall_seconds": wall_seconds,
        "harness_exit_code": result.returncode,
        "harness_error": harness_error,
        "resolved_model_usage": resolved_model_usage or None,
    }


def main() -> None:
    parser = argparse.ArgumentParser(
        description="SWE-bench single-task runner — claude-code harness (headless)."
    )
    parser.add_argument(
        "--condition",
        default="baseline",
        help=(
            "Recorded verbatim in provenance as condition. claude-code is a "
            "generic coding agent, not spelunk-instrumented, so this is "
            "always baseline in practice — see bench/agents/README.md. The "
            "deepseek-vs-native distinction lives in endpoint_kind, not here."
        ),
    )
    parser.add_argument("--task-id", required=True)
    parser.add_argument("--repo-path", required=True)
    parser.add_argument("--issue", required=True)
    parser.add_argument(
        "--model",
        default="deepseek-v4-flash",
        help=(
            "Model identity for provenance + ANTHROPIC_MODEL override. "
            "Use a claude-* id (e.g. claude-sonnet-5) to run a native Claude "
            "cell instead of DeepSeek — no env overrides are applied in "
            "that case, see --no-deepseek."
        ),
    )
    parser.add_argument(
        "--no-deepseek",
        action="store_true",
        help=(
            "Skip the DeepSeek env overrides — use Claude Code's own default "
            "Anthropic credentials/model unchanged. For future native "
            "Claude-model cells (spec point 4)."
        ),
    )
    parser.add_argument(
        "--api-key",
        default=None,
        help="DeepSeek API key (falls back to DEEPSEEK_API_KEY env var). Ignored with --no-deepseek.",
    )
    parser.add_argument(
        "--endpoint-kind",
        choices=["anthropic-compat", "shim"],
        default="anthropic-compat",
        help=(
            "anthropic-compat: DeepSeek's native Anthropic-compatible endpoint "
            "(default). shim: an Anthropic->OpenAI proxy in front of DeepSeek's "
            "OpenAI-compatible endpoint, for use if anthropic-compat misbehaves."
        ),
    )
    parser.add_argument(
        "--shim-base-url",
        default=None,
        help="Base URL of a running Anthropic->OpenAI proxy (required with --endpoint-kind shim).",
    )
    parser.add_argument(
        "--deepseek-base-url",
        default=DEEPSEEK_ANTHROPIC_BASE_URL,
        help="Override DeepSeek's Anthropic-compat endpoint URL.",
    )
    parser.add_argument(
        "--effort",
        default="high",
        choices=["low", "medium", "high", "xhigh", "max"],
        help="Claude Code --effort level. Always pinned + recorded in provenance (spec point 4).",
    )
    parser.add_argument(
        "--thinking",
        action="store_true",
        help="Request step-by-step thinking. Recorded in provenance (spec point 4).",
    )
    parser.add_argument("--max-turns", type=int, default=20)
    parser.add_argument("--seed", type=int, default=42)
    parser.add_argument(
        "--save-patch",
        default=None,
        help="Save git diff to this file after the run finishes.",
    )
    args = parser.parse_args()

    repo_path = Path(args.repo_path).resolve()
    if not repo_path.is_dir():
        parser.error(f"repo-path does not exist: {repo_path}")

    if args.endpoint_kind == "shim" and not args.shim_base_url:
        parser.error("--shim-base-url is required with --endpoint-kind shim")

    issue_text = read_issue_text(args.issue)
    claude_version = get_claude_version()

    if args.no_deepseek:
        env = dict(os.environ)
        endpoint_kind = "native"
        api_key_source = "n/a (--no-deepseek, uses ambient Claude Code auth)"
        base_url_used = None
    else:
        api_key = args.api_key or os.environ.get("DEEPSEEK_API_KEY")
        api_key_source = "flag:--api-key" if args.api_key else "env:DEEPSEEK_API_KEY"
        if not api_key:
            parser.error(
                "No DeepSeek API key provided. Use --api-key, set DEEPSEEK_API_KEY, "
                "or pass --no-deepseek for a native Claude-model cell."
            )
        base_url_used = (
            args.shim_base_url
            if args.endpoint_kind == "shim"
            else args.deepseek_base_url
        )
        env = _deepseek_anthropic_env(api_key, args.model, base_url_used)
        endpoint_kind = args.endpoint_kind

    agent_result = run_claude_code(
        repo_path=repo_path,
        issue_text=issue_text,
        model=args.model,
        effort=args.effort,
        thinking=args.thinking,
        env=env,
    )

    patch_path = extract_patch(repo_path, args.save_patch)

    output = {
        "benchmark": "swebench-verified",
        "condition": args.condition,
        "harness": "claude-code",
        "harness_version": claude_version,
        "endpoint_kind": endpoint_kind,
        "effort": args.effort,
        "thinking": args.thinking,
        "model": args.model,
        "model_source": "api",
        "api_base_url": base_url_used,
        "api_key_source": api_key_source,
        "spelunk_version": None,  # claude-code harness does not invoke spelunk tools
        "seed": args.seed,
        "run_seed": args.seed,
        "max_turns": args.max_turns,
        "task_id": args.task_id,
        "patch_file": str(patch_path) if patch_path else None,
        # Populated later, once the corresponding infra lands (README §Provenance):
        "question_set_version": None,
        "instance_filter": None,
        "judge_model": None,
        "judge_version": None,
        "judge_error_rate": None,
        **agent_result,
    }
    print(json.dumps(output))


if __name__ == "__main__":
    main()
