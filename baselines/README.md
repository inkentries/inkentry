# baselines/

Committed baseline results for reproducible benchmarks.

Keeping these outside `bench/` is intentional: the scaffold hash is computed from
`bench/` only, so committing a new baseline here does not change that hash or
spuriously invalidate itself.

## Current baselines

| File | Benchmark | Model | Status |
|------|-----------|-------|--------|
| `repobench-gemma-4-e2b-it-baseline.json` | RepoBench-Python | gemma-4-e2b-it | Superseded — regenerate with deepseek-v4-flash |
| `swebench-local-gemma-4-e2b-it-baseline.json` | SWE-bench local | gemma-4-e2b-it | Superseded — regenerate with deepseek-v4-flash |

## When to regenerate

Re-run and commit a new baseline when any of these change:

1. **The model** — different model or API endpoint
2. **The agent scaffold** — system prompt, tool definitions, `max_turns`, or `seed`
   in `bench/agents/agent.py` or `bench/gemma/crosscodeeval/evaluate.py`
3. **The task/sample set** — `bench/agents/tasks_50.json` or sample seed/count

Each baseline JSON includes a `scaffold_hash` (last git commit of `bench/`).
The run scripts warn automatically when this hash no longer matches HEAD.

## Regenerating with DeepSeek V4 Flash

```bash
# RepoBench baseline (400 samples, cross_file_first split)
bash bench/gemma/crosscodeeval/run.sh --condition baseline --samples 400
# Review the output, then:
cp bench/results/repobench-baseline-<ts>.json baselines/repobench-deepseek-v4-flash-baseline.json
git add baselines/repobench-deepseek-v4-flash-baseline.json
git commit -m "bench: update RepoBench baseline (deepseek-v4-flash)"

# SWE-bench baseline (50 tasks, runs only tasks with repos available)
bash bench/agents/swebench_run.sh --condition baseline --tasks 50
cp bench/results/swebench-baseline-<ts>.json baselines/swebench-deepseek-v4-flash-baseline.json
git add baselines/swebench-deepseek-v4-flash-baseline.json
git commit -m "bench: update SWE-bench baseline (deepseek-v4-flash)"
```

### Notes

- **Infrastructure vs. resolve_rate:** Infrastructure fixes (Phase 3) unblock
  phases 4–8 by ensuring all tasks run without crashes. They do not improve
  `resolve_rate` — that requires a capable model. Expect `resolve_rate ≈ 0`
  with any sub-7B local model regardless of infrastructure health. DeepSeek
  V4 Flash is the minimum viable model for meaningful resolve rate measurement.

- **spelunk_full vs spelunk_search equivalence for SWE-bench:** For SWE-bench
  repos checked out at single commits, `spelunk memory harvest` has no git
  history to extract from — memory tools return empty results. The
  `spelunk_full` condition is identical to `spelunk_search` for these repos.
  The condition becomes differentiated only on repos with prior spelunk
  memory (Phase 6a/6b benchmarks).

- **Phase 6a prerequisite:** `spelunk context` (issue #201) must be merged
  before the cross-session handoff benchmark can be scripted as described.
