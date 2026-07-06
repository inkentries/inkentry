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

# 3. Run the full 50-task benchmark + Docker evaluation in one step
bash bench/agents/swebench_run.sh \
    --condition spelunk_full \
    --api-key "$DEEPSEEK_API_KEY" \
    --eval

# 3b. Agent run only (evaluate later)
bash bench/agents/swebench_run.sh \
    --condition spelunk_full \
    --api-key "$DEEPSEEK_API_KEY"
# The script prints the swebench_eval.sh command to run next.
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
    [--tasks 50] [--max-turns 20] [--seed 42] [--skip-index] [--eval]
```

Reads `bench/agents/tasks_50.json`, expects repos checked out at
`bench/repos/<task_id>/` (via `bench/setup_repos.sh`).

For `spelunk_search` and `spelunk_full` conditions, runs `spelunk index` on
each repo before the agent (unless `--skip-index` is set).

Each task's git diff is saved to `bench/patches/<condition>-<timestamp>/<task_id>.patch`
(override with `--patches-dir`). These patches are required for the Docker harness.

Results are written to `bench/results/swebench-<condition>-<timestamp>.json`.

Pass `--eval` to automatically invoke `swebench_eval.sh` after the agent run
completes, computing real `resolve_rate` via the SWE-bench Docker harness.
Without `--eval`, the script prints the exact command to run next.

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

## Contamination control — leakage-filtered instances

Track-B (SWE-bench) numbers are reported on two instance sets, **always
separately**, and every published figure names its `instance_filter`:

| `instance_filter`          | Instance set                                              |
|----------------------------|----------------------------------------------------------|
| `full`                     | SWE-bench Verified, unfiltered (500 instances)           |
| `swebench_plus_filtered`   | Verified minus SWE-Bench+ leakage/suspicious instances   |

**Why.** SWE-Bench+ (arXiv:2410.06992) found ~32.67% of passing SWE-bench
patches benefited from *solution leakage* — the fix appears in the issue report
or comments — plus a large share of *suspicious* passes on weak tests (55.36%
of the Verified sample they inspected). A resolve_rate on the full set is
inflated by these. Reporting a `swebench_plus_filtered` figure alongside `full`
shows how much of a result survives contamination control.

**Reporting rule.** Never publish a single blended Track-B number. Every figure
carries its `instance_filter`; `full` and `swebench_plus_filtered` are reported
side by side.

### Generating the filtered list

`build_filtered_tasks.py` intersects Verified with a SWE-Bench+ exclude set and
writes `tasks_filtered.json` (with a provenance header):

```bash
python bench/agents/build_filtered_tasks.py \
    --labels swebench_plus_verified_exclude.json \
    --labels-source "arXiv:2410.06992 replication pkg, rev <sha/date>" \
    --out bench/agents/tasks_filtered.json
```

`--labels` is the SWE-Bench+ per-instance leakage/suspicious label set for
Verified (list of instance_ids to exclude, or an `id -> reason` map). SWE-Bench+
publishes a new post-cutoff dataset rather than a single filtered-Verified file,
so a maintainer must obtain these labels from the authors' released artifact and
pin the revision via `--labels-source`. Target survivor count is 150–300; the
script warns if the intersection falls outside that band. `--dry-run` reports
counts and the `tasks_50.json` overlap without writing.

> **Status:** `tasks_filtered.json` is **not yet committed** — it requires the
> SWE-Bench+ label set, which is not distributed as a fetchable file. The script
> above generates it once that input is supplied. Do not hand-author the list.

### Overlap with `tasks_50.json`

Of the 50-slice, **24** instances are in SWE-bench Verified (the other 26 come
from the SWE-bench *full* split — see `setup_repos.sh`, issue #252). Only those
24 can ever survive the filter; the survivor subset is reported by
`build_filtered_tasks.py --overlap-with bench/agents/tasks_50.json` once the
label set is available.

## Notes

- `resolved` is always `false` in agent output from `agent.py` — resolution
  comes from the SWE-bench Docker harness. Use `--eval` on `swebench_run.sh`
  or run `swebench_eval.sh` separately to populate real resolve rates.
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
