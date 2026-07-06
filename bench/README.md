# spelunk Benchmarks

Developer-only scripts for measuring whether spelunk improves agent task completion and retrieval accuracy. Run manually before releases or after significant changes to the indexer, chunker, or embedding pipeline.

---

## Overview

| Tier | Benchmark | Model | Cadence |
|------|-----------|-------|---------|
| **Primary** | CrossCodeEval | gemma-4-e2b-it (local) | Pre-release |
| **Primary** | SWE-bench | any OpenAI-compatible | Pre-release |
| Secondary | SWE-bench (Claude) | claude-sonnet-4-6 | Major releases only |
| Retrieval | CodeSearchNet | model-agnostic | On indexer/chunker changes |
| Retrieval | Code-graph | model-agnostic | On graph/indexer changes |

---

## Committed baselines

Baseline results (no-spelunk condition) live in `baselines/` at the repo root and are committed to git. Normal runs execute only the spelunk condition and auto-compare against the baseline. See `baselines/README.md` for when and how to regenerate.

---

## Prerequisites

**For Gemma benchmarks (primary):**
- `uv` in PATH (run scripts use `uv run` and install Python deps automatically)
- Local OpenAI-compatible server at `http://127.0.0.1:1234` with `gemma-4-e2b-it` loaded
- `spelunk` in PATH (build: `cargo build --release`)
- Docker (SWE-bench only)

## SWE-bench repo setup

The SWE-bench local scripts expect each task's repo cloned at the pre-fix commit under `bench/repos/<task_id>/`, with an `ISSUE.txt` alongside. Run once before benchmarking:

```bash
bash bench/setup_repos.sh
```

This fetches task metadata from HuggingFace (`princeton-nlp/SWE-bench_Verified`) and clones each repo at the correct base commit. Re-running is idempotent; already-correct checkouts are skipped. Requires internet access and `uv`.

Options: `--tasks N` (first N only) · `--repos-dir DIR` · `--dataset SLUG`

**For Claude benchmarks (secondary):**
- `ANTHROPIC_API_KEY` in environment
- Docker

---

## RepoBench (cross-file completion)

Measures whether `spelunk_search` helps complete lines that require symbols from other files. Uses [RepoBench-Python](https://huggingface.co/datasets/tianyang/repobench_python_v1.1), `cross_file_first` split: the completion point requires a symbol introduced in another file, making it the most relevant split for measuring spelunk's retrieval benefit.

```bash
# Spelunk condition — compares against committed baseline automatically
bash bench/gemma/crosscodeeval/run.sh --condition spelunk --repo-path /path/to/indexed/repo

# Regenerate baseline (run once after scaffold changes)
bash bench/gemma/crosscodeeval/run.sh --condition baseline --samples 400
```

**Options:** `--split cross_file_first|cross_file_random|in_file` · `--samples 200` · `--model gemma-4-e2b-it` · `--api-base-url http://127.0.0.1:1234/v1`

**Metrics:** `exact_match`, `edit_similarity`, `identifier_recall`

---

## SWE-bench (local model)

Measures whether `spelunk_search` helps fix real GitHub issues. Uses the same 50-task slice as the Claude variant so results are directly comparable.

```bash
# Run agent + Docker harness in one step (recommended)
bash bench/agents/swebench_run.sh \
    --condition spelunk_search \
    --model gemma-4-e2b-it \
    --api-base-url http://127.0.0.1:1234/v1 \
    --eval

# Agent run only (then eval separately)
bash bench/agents/swebench_run.sh \
    --condition spelunk_search \
    --model gemma-4-e2b-it \
    --api-base-url http://127.0.0.1:1234/v1

# Evaluate a prior agent run
bash bench/agents/swebench_eval.sh \
    --results bench/results/swebench-spelunk_search-<timestamp>.json \
    --patches-dir bench/patches/spelunk_search-<timestamp>
```

Repo checkouts are expected at `bench/repos/<task_id>/` (via `bench/setup_repos.sh`). Each directory must contain an `ISSUE.txt`. Patches are saved to `bench/patches/<condition>-<timestamp>/` during the agent run. Pass `--eval` to automatically invoke the Docker harness after all tasks complete.

> **Note:** `bench/gemma/swebench_local/run.sh` is retired; it always outputs `resolved=0` because it never ran the Docker harness. Use `bench/agents/swebench_run.sh` instead.

**Metrics:** `resolve_rate` (via harness), `median_tokens_per_task`, `median_wall_seconds`

---

## SWE-bench (Claude) - secondary

```bash
bash bench/swebench/run.sh --condition baseline --tasks 50
bash bench/swebench/run.sh --condition spelunk  --tasks 50
```

Requires `ANTHROPIC_API_KEY`. Results go to `bench/results/swebench-{condition}-{timestamp}.json`.

---

## Code-graph - call graph retrieval quality

Model-agnostic. Measures how well `spelunk graph` retrieves files containing callers/callees/implementers of a symbol, compared against `git grep` and `spelunk search` as baselines.

**Repo setup:** clone the benchmark repos somewhere *outside* this repository (to avoid polluting the spelunk index), then point the evaluator at them:

```bash
mkdir -p ~/spelunk-bench/repos
git clone https://github.com/BurntSushi/ripgrep      ~/spelunk-bench/repos/ripgrep
git clone https://github.com/django/django            ~/spelunk-bench/repos/django__django-12125
git clone https://github.com/sympy/sympy              ~/spelunk-bench/repos/sympy__sympy-20590
```

Index each repo with spelunk before running:

```bash
for repo in ~/spelunk-bench/repos/*/; do spelunk index "$repo"; done
```

**Run:**

```bash
# via --repos-dir flag
python bench/graph/evaluate.py \
    --tasks bench/graph/tasks.json \
    --repos-dir ~/spelunk-bench/repos \
    --k 10 \
    --out bench/results/graph.json

# or via environment variable
export SPELUNK_BENCH_REPOS=~/spelunk-bench/repos
python bench/graph/evaluate.py --tasks bench/graph/tasks.json --k 10
```

**Metrics:** `precision@k`, `recall@k`, `F1` (averaged across all 42 tasks in three repos: ripgrep, django, sympy).

---

## CodeSearchNet — retrieval quality

Model-agnostic. Measures how accurately `spelunk search` retrieves relevant code for natural-language queries.

```bash
bash bench/codesearchnet/run.sh --languages python --samples 1000
```

The target repo must be indexed before running: `spelunk index /path/to/repo`.

**Metrics:** `mrr_at_10`, `recall_at_5`, `recall_at_10`

---

## Comparing results

```bash
python bench/report.py baselines/crosscodeeval-gemma-4-e2b-it-baseline.json bench/results/crosscodeeval-spelunk-<ts>.json
```

The spelunk run scripts print this comparison automatically when a baseline exists. You can also compare any two result files directly.

Example output:
```
| benchmark      | condition | model                    | exact_match | edit_sim | id_recall | med_wall_s |
|----------------|-----------|--------------------------|-------------|----------|-----------|------------|
| crosscodeeval  | baseline  | gemma-4-e2b-it (local)  | 0.170       | 0.481    | 0.224     | 3.800      |
| crosscodeeval  | spelunk   | gemma-4-e2b-it (local)  | 0.210       | 0.541    | 0.291     | 4.100      |
```

---

## Paired statistics (§6 reporting standard)

`report.py` gives a quick side-by-side of aggregate figures. For any *published*
agentic comparison, use `paired_stats.py` instead: it applies the plan §6
standards over per-task result files (arrays of records, or the post-harness
`{"aggregate": ..., "tasks": [...]}` form).

```bash
python bench/paired_stats.py \
    bench/results/examples/swebench-local-baseline.json \
    bench/results/examples/swebench-local-spelunk.json
```

What it computes:

- **McNemar's exact test**, paired by `task_id` on the task-level binary outcome
  (`resolved`/`passed`). Discordant pairs and an exact binomial p-value are
  reported. This is a paired test (not a two-proportion z-test) because both
  conditions run the same tasks. Multi-seed cells collapse to one outcome per
  task by majority vote before pairing.
- **Bootstrap 95% CIs** over per-seed cell means (`mean +/- half-width [lo, hi]`),
  reproducible via a fixed RNG seed. Needs n>=3 seeds.
- **Deterministic layers** (Track A retrieval, n=1) are stated as
  `deterministic, n=1` rather than given a fabricated CI.
- **Cell-labeled output:** every figure names its full cell (model, harness,
  condition, instance_filter, n). The tool **refuses (errors)** to aggregate
  across records with differing model / harness / condition; pass `--filter` to
  label the instance subset.
- **Negative results** are printed as `not significant`, never dropped.

**Power note:** the 50-task slice can only detect large effects (about +/-15pp).
Headline claims must come from the filtered subset or the 150+ question set.

Committed example fixtures live in `bench/results/examples/`; real runs land in
`bench/results/` (gitignored).

---

## Metrics reference

| Metric | Benchmark | Meaning |
|--------|-----------|---------|
| `exact_match` | CrossCodeEval | Fraction of completions that exactly match ground truth |
| `edit_similarity` | CrossCodeEval | Average SequenceMatcher ratio between prediction and ground truth |
| `identifier_recall` | CrossCodeEval | Fraction of identifiers in ground truth that appear in the prediction |
| `resolve_rate` | SWE-bench | Fraction of tasks where the patch passes all tests (set by harness) |
| `mrr_at_10` | CodeSearchNet | Mean Reciprocal Rank at 10 |
| `recall_at_5/10` | CodeSearchNet | Fraction of queries where ground truth appears in top 5/10 results |
| `median_tokens_per_task` | SWE-bench | Median total tokens per task |
| `median_wall_seconds` | All | Median wall-clock seconds per task/query |
