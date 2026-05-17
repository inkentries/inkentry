# spelunk Benchmarking Report

**Date:** 2026-05-17
**Model:** `deepseek-v4-flash` (DeepSeek V4 Flash, non-thinking)
**spelunk version:** 0.6.0
**Status:** Infrastructure complete. Controlled reruns pending.

---

## Executive Summary

We built a reproducible benchmarking framework covering spelunk's three
value pillars. All benchmark scripts produce verifiable, seed-controlled
output with reproducibility-contract fields. Controlled experiments with
matched compute budgets, n>=30, and correctness checks are now ready to
run — the framework is in place; the headline numbers await the rerun.

| Pillar | Benchmark | Status |
|--------|-----------|--------|
| Semantic search | RepoBench cross-file completion | 4-condition eval ready; pending controlled rerun with indexed-repo overlap |
| Project memory | Decision archaeology | 4-condition eval ready; pending blind question sets |
| Project memory | Cross-session handoff | 3-condition eval with verify_cmd ready; pending task corpus |
| Code graph | Call-graph retrieval | 3-condition eval ready; pending task corpus |
| Performance | Index/search/memory at scale | `perf_scale.sh` orchestrator ready; pending scale runs |

---

## 1. RepoBench — Cross-File Code Completion

**Dataset:** [RepoBench-Python v1.1](https://huggingface.co/datasets/tianyang/repobench_python_v1.1),
`cross_file_first` split.

**Four conditions** (matched 5-turn compute budget):

| Condition | Loop | Tools | Isolates |
|-----------|------|-------|----------|
| `baseline_single_shot` | No | None | Single-call baseline |
| `multi_turn_no_tools` | Yes | None | Loop without retrieval |
| `naive_search` | Yes | `read_file`, `run_grep` | Tool-calling without semantic search |
| `spelunk` | Yes | `spelunk_search` | Semantic search |

**Indexed-repo matching:** `--repo-filter` flag sub-selects tasks to a single
repo, with measured `indexed_repo_overlap_pct` in output JSON.
Recommended repo: `mpenning/ciscoconfparse2` (86 tasks, largest slice).

**Scripts:** `bench/gemma/crosscodeeval/evaluate.py`, `README.md`

**Reproducing:**
```bash
export DEEPSEEK_API_KEY=sk-...
spelunk index /tmp/ccp2
python bench/gemma/crosscodeeval/evaluate.py \
    --condition spelunk --repo-path /tmp/ccp2 \
    --repo-filter mpenning/ciscoconfparse2 --samples 50 --seed 42
```

---

## 2. Decision Archaeology — Memory Search vs. Lexical Search

**Four conditions** with matched queries:

| Condition | Query | Search target |
|-----------|-------|---------------|
| `grep_literal` | Full question verbatim | `git log --grep` |
| `grep_keywords` | Regex-extracted keywords | `git log --grep` per keyword |
| `fts_commit_messages` | Full question (OR-semantics) | SQLite FTS5 over all commits |
| `memory_search` | Full question | `spelunk memory search` (semantic) |

Hit detection uses `ground_truth_commit` SHA with prefix-matching across
both `source_ref` (memory) and `commit` (grep/FTS) fields.

**Blindness protocol** documented in `bench/memory/README.md`. Questions
must be authored from raw `git log` via `author_questions.py`, without
access to the spelunk memory database. Pending blind question sets for
>=3 repos with >=10 questions each.

**Scripts:** `bench/memory/decision_archaeology.py`, `author_questions.py`, `README.md`

**Reproducing:**
```bash
spelunk index /path/to/repo
cd /path/to/repo && spelunk memory harvest --git-range HEAD~500..HEAD
python bench/memory/decision_archaeology.py \
    --repo-path /path/to/repo \
    --questions bench/memory/questions-<repo>.json \
    --rebuild-fts
```

---

## 3. Cross-Session Handoff — Agent Memory Transfer

**Three Session 2 conditions:**

| Condition | S1 files | Memory | Measures |
|-----------|----------|--------|----------|
| Cold start | None | None | Intrinsic task difficulty |
| Files present | On disk | None | Value of file state alone |
| With memory | On disk | Full | Value of files + memory |

Session 1 is force-cut at `--session-1-turns` (default 5) with a system
prompt instructing the agent to write a detailed handoff. Each task has a
`verify_cmd` for binary pass/fail. Pending a task corpus with >=10 tasks
across >=2 repos.

**Scripts:** `bench/memory/cross_session_handoff.py`, `handoff_tasks.json`, `README.md`

**Reproducing:**
```bash
python bench/memory/cross_session_handoff.py \
    --tasks bench/memory/handoff_tasks.json \
    --session-1-turns 5 --session-2-turns 15 \
    --out bench/results/handoff.json
```

---

## 4. Code Graph — Call-Graph Retrieval

**Three conditions:** `grep` (git grep), `spelunk_search` (semantic),
`spelunk_graph` (code graph edges). Metrics: precision@k, recall@k, F1.
No LLM required.

**Scripts:** `bench/graph/evaluate.py`, `tasks.json`

**Reproducing:**
```bash
python bench/graph/evaluate.py \
    --tasks bench/graph/tasks.json --k 10 \
    --out bench/results/graph.json
```

---

## 5. SWE-bench — Agent Evaluation

Agent patch generation works across 3 conditions. `--save-patch` flag
in `agent.py` captures `git diff` output. `export_patches.py` collates
into SWE-bench prediction format. `swebench_eval.sh` orchestrates Docker
harness evaluation. Pending Docker harness run.

**Scripts:** `bench/agents/agent.py`, `export_patches.py`, `swebench_eval.sh`

---

## 6. Performance at Scale

`bench/perf_scale.sh` orchestrator runs indexing, search latency, and
memory benchmarks across multiple labelled repo sizes, aggregating into
a single JSON. Pending scale runs with >=3 repo sizes including one
with >=10k files.

**Scripts:** `bench/perf_scale.sh`, `perf_index.sh`, `perf_search.sh`, `git_meta_perf.sh`

---

## Scaffolding — Infrastructure Ready

These scripts are committed and runnable; results are pending:

| Script | Purpose |
|--------|---------|
| `bench/codesearchnet/evaluate.py` | Retrieval accuracy with `--mode text/hybrid` and `--graph` |
| `bench/agents/batch_run.py` | Incremental batch agent orchestrator |
| `bench/linearrag/run_eval.py` | Internal KNN vs LinearRAG comparison |
| `bench/git_notes_perf.sh` | GitNotesBackend performance regression |

---

## Data Artefacts

### Committed baselines
```
baselines/repobench-gemma-4-e2b-it-baseline.json      — superseded
baselines/repobench-deepseek-v4-flash-baseline.json    — STALE (pre-control, 0% overlap)
baselines/swebench-local-gemma-4-e2b-it-baseline.json  — superseded
```

### Benchmark scripts (17 files)
```
bench/agents/          — agent.py, batch_run.py, swebench_run.sh, export_patches.py, swebench_eval.sh
bench/memory/          — decision_archaeology.py, author_questions.py, cross_session_handoff.py, handoff_tasks.json, questions-ripgrep.json
bench/graph/           — evaluate.py, tasks.json
bench/codesearchnet/   — evaluate.py
bench/gemma/crosscodeeval/ — evaluate.py
bench/perf_scale.sh, perf_index.sh, perf_search.sh, git_meta_perf.sh, git_notes_perf.sh
```

---

## Limitations

- **API non-determinism:** even at `temperature=0.0`, API responses vary
  slightly between runs. `seed` is passed to the API where supported.
- **Repo availability:** the SWE-bench Verified dataset contains 24 of the
  50 pinned tasks. The remaining 26 require the full SWE-bench dataset.
- **Indexed-repo overlap:** RepoBench spans 1,751 repos. Without
  `--repo-filter`, overlap between indexed codebase and sampled tasks is ~0%.
- **Blind question sets:** the decision archaeology benchmark has 4
  functioning conditions but is evaluated against corpus-derived questions
  until blind sets are authored.
- **Single-machine perf:** all performance numbers are from a single Apple
  M-series MacBook Pro. Numbers will differ on other hardware.
- **Handoff task corpus:** the cross-session handoff benchmark has the
  full 3-condition infrastructure but a placeholder task corpus.
