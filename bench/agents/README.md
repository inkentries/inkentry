# Agents — SWE-bench Agent Scripts

Unified agent for running SWE-bench tasks via any OpenAI-compatible API
(DeepSeek, self-hosted, or local LM Studio).

## Quick Start

```bash
# 1. Set up repos (one-time)
bash bench/setup_repos.sh --tasks 5

# 2. Run the agent on a single task
python bench/agents/agent.py \
    --condition baseline \
    --task-id django__django-11099 \
    --repo-path bench/repos/django__django-11099 \
    --issue bench/repos/django__django-11099/ISSUE.txt \
    --api-key "$DEEPSEEK_API_KEY"

# 3. Run the full 50-task benchmark
bash bench/agents/swebench_run.sh \
    --condition spelunk_full \
    --api-key "$DEEPSEEK_API_KEY"
```

## Conditions

| Condition | Tools |
|-----------|-------|
| `baseline` | `read_file`, `run_bash`, `write_file` |
| `spelunk_search` | baseline + `spelunk_search` (semantic code retrieval) |
| `spelunk_full` | baseline + `spelunk_search` + `spelunk_graph` + `spelunk_memory_search` |

## agent.py — Single-task runner

```bash
python bench/agents/agent.py \
    --condition baseline|spelunk_search|spelunk_full \
    --task-id <task_id> \
    --repo-path /path/to/repo \
    --issue "Issue text or path to ISSUE.txt" \
    --model deepseek-v4-flash \
    --api-base-url https://api.deepseek.com/v1 \
    --api-key "$DEEPSEEK_API_KEY" \
    [--max-turns 20] [--seed 42]
```

The `--issue` argument accepts either inline text or a file path. If the
argument points to an existing file, its contents are read as the issue text.

Output is a single JSON object on stdout with reproducibility contract fields:
`benchmark`, `condition`, `model`, `model_source`, `api_base_url`,
`api_key_source`, `spelunk_version`, `seed`, plus task-level metrics
(`task_id`, `turns`, `input_tokens`, `output_tokens`, `wall_seconds`).

## swebench_run.sh — Batch orchestrator

```bash
bash bench/agents/swebench_run.sh \
    --condition spelunk_full \
    --model deepseek-v4-flash \
    --api-key "$DEEPSEEK_API_KEY" \
    [--tasks 50] [--max-turns 20] [--seed 42] [--skip-index]
```

Reads `bench/agents/tasks_50.json`, expects repos checked out at
`bench/repos/<task_id>/` (via `bench/setup_repos.sh`).

For `spelunk_search` and `spelunk_full` conditions, runs `spelunk index` on
each repo before the agent (unless `--skip-index` is set).

Results are written to `bench/results/swebench-<condition>-<timestamp>.json`.

## Reproducibility

Every result JSON includes:

```json
{
    "benchmark": "swebench-verified",
    "condition": "spelunk_full",
    "model": "deepseek-v4-flash",
    "model_source": "api",
    "api_base_url": "https://api.deepseek.com/v1",
    "api_key_source": "env:DEEPSEEK_API_KEY",
    "spelunk_version": "0.6.0",
    "seed": 42,
    "max_turns": 20,
    "task_id": "django__django-11099",
    "turns": 5,
    "input_tokens": 12000,
    "output_tokens": 1500,
    "wall_seconds": 45.2,
    "resolved": false
}
```

Anyone with a DeepSeek API key can reproduce:
```bash
export DEEPSEEK_API_KEY=sk-...
bash bench/agents/swebench_run.sh --condition spelunk_full --seed 42
```

## Notes

- `resolved` is always `false` in agent output — resolution comes from the
  SWE-bench Docker harness, run separately.
- The spelunk CLI must be in PATH. The agent handles exit code 1 (no results)
  gracefully.
- DeepSeek API may have rate limits — the orchestrator pauses 1 s between tasks.
- **Infrastructure vs. resolve_rate:** Infrastructure fixes (Phase 3) unblock
  benchmarks by ensuring tasks run without crashes. They do not improve
  `resolve_rate` — that requires a capable model (deepseek-v4-flash).
- **spelunk_full vs spelunk_search:** For SWE-bench repos checked out at single
  commits, `spelunk memory harvest` has no git history — memory tools return
  empty results. `spelunk_full` is equivalent to `spelunk_search` for these
  repos. The condition differentiates only on repos with prior spelunk memory
  (Phase 6 benchmarks).
- **Phase 6a prerequisite:** `spelunk context` (#201) must be merged before the
  cross-session handoff benchmark can be scripted as described in the plan.
