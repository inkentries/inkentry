# bench/AGENTS.md — Benchmark Coding Conventions

Operational findings and coding conventions for the `bench/` directory.
Companion to `AGENTS.md` at the repo root.

---

## Benchmark Design Principles

These emerged from the benchmarking overhaul (issues #224–#232) and the
original review in `tmp/benchmark-fix-plan.md`:

1. **Every claim needs a control.** Multi-turn-with-spelunk must be compared
   against multi-turn-without-spelunk, not against single-shot. A lift that
   disappears when the baseline gets the same compute budget is not spelunk's lift.
2. **No question-from-corpus circularity.** Questions used to evaluate retrieval
   must be authored by a party that has not seen what is in the memory store.
3. **Correctness > activity.** "Turns used" and "tokens spent" without a
   correctness signal are activity metrics, not quality metrics. Every agent
   benchmark needs a verifiable success criterion.
4. **Honest sample sizes.** No n=1 demos in a report that compares means.
5. **The report should under-promise.** Where infrastructure is ready but
   results aren't in, say so explicitly. Don't list empty rows in the
   executive summary.

### Scaffolding vs. Deliverable split

When a PR delivers the *framework* but not the *content* (e.g. benchmark script
is ready but task corpus is placeholder), use `Refs #N` not `Closes #N`. File a
follow-up issue for the content-authoring half. This pattern was used for:

- #226 (blind protocol) → #237 (question sets)
- #228 (handoff redesign) → #247 (task corpus)
- #230 (graph benchmark) → #248 (task corpus)
- #231 (perf orchestrator) → follow-up (scale runs)

---

## Python Benchmark Scripts

### Dependency management
- Use `uv run --quiet --with-requirements bench/requirements.txt` for all Python
  scripts under `bench/`. Never assume `python3` has the needed packages.
- `bench/requirements.txt` is the single source of truth for Python deps.

### Secrets
- API keys live in `.env.local` (gitignored). Load via `python-dotenv` at
  the top of every script that calls an API:
  ```python
  from dotenv import load_dotenv
  _root = Path(__file__).resolve().parents[2]  # or parents[3] for nested scripts
  _dotenv = _root / ".env.local"
  if _dotenv.exists():
      load_dotenv(_dotenv)
  ```
- Fall back to `DEEPSEEK_API_KEY` env var, then `--api-key` flag.
- Record `api_key_source` in result JSON for auditability.

### Reproducibility contract
Every result JSON must include: `benchmark`, `condition`, `model`,
`model_source`, `api_base_url`, `spelunk_version`, `seed`, `timestamp`.
Add `scaffold_hash` for committed baselines.

### Seed plumbing
- `np.random.seed(args.seed)` for numpy.
- `seed=args.seed` passed to `client.chat.completions.create()`.
- Both must be present; numpy-only seeding was a bug caught in review.

### Incremental writes
For long-running benchmarks, write results incrementally (every N tasks) so a
crash doesn't lose everything. `batch_run.py` does this; `swebench_eval.sh`
writes the final envelope at the end.

---

## Shell Script Patterns

### Don't shadow `PATH`
```bash
# WRONG — overrides the shell's command search path
PATH="${entry#*:}"

# RIGHT — use a different name
REPO_DIR="${entry#*:}"
```

### spelunk status for aggregate counts
```bash
# WRONG — spelunk chunks wants a file, not a directory
CHUNKS=$(spelunk chunks --format json "$DIR" ...)

# RIGHT — spelunk status gives file_count and chunk_count
STATS=$(cd "$DIR" && spelunk status --format json)
FILES=$(echo "$STATS" | python3 -c "import json,sys; print(json.load(sys.stdin).get('file_count',0))")
CHUNKS=$(echo "$STATS" | python3 -c "import json,sys; print(json.load(sys.stdin).get('chunk_count',0))")
```

### Verify field names against actual output
Always smoke-test spelunk commands to confirm JSON field names before
committing. `file_count` vs `files` was a silent-zero bug caught in review.

### set -euo pipefail with explicit failure handling
```bash
# Prefer explicit failure over || true
if ! "$SPELUNK" index "$REPO_DIR" >/dev/null 2>&1; then
    echo "FAILED — skipping repo" >&2
    continue
fi
```

---

## Common Silent-Failure Patterns

These bugs all produce zero/empty results without raising exceptions.
Smoke tests catch them; code review often misses them.

| Pattern | Example | Fix |
|---------|---------|-----|
| Field name mismatch | `r.get("files")` vs actual `file_count` | Smoke-test against real output |
| Bare `except Exception: return []` | FTS syntax error swallowed | Log to stderr, narrow exception types |
| FTS5 AND vs OR semantics | Multi-word query requires all tokens | OR-join with quoted tokens |
| FTS5 special characters | `?` in query breaks parser | Strip non-alphanumeric before MATCH |
| `git diff` misses untracked files | Agent creates new files via `write_file` | `git add -A && git diff --cached HEAD` |
| `source_ref` vs `commit` field | Memory entries use different SHA field | Check both with prefix match |
| Wrong harness output filename | SWE-bench version drift | Glob for file containing `"resolved"` key |

---

## Project-Specific Conventions

### Directory layout
- `bench/agents/` — SWE-bench agent and evaluation
- `bench/memory/` — decision archaeology and cross-session handoff
- `bench/graph/` — code-graph retrieval
- `bench/codesearchnet/` — CodeSearchNet retrieval
- `bench/gemma/crosscodeeval/` — RepoBench cross-file completion
- `bench/perf_*.sh` — performance benchmarks

### Output paths
- Scratch results: `bench/results/` (gitignored except `.gitignore`)
- Committed baselines: `baselines/` (outside `bench/` so scaffold hash is stable)
- Plans and reports: `tmp/` (gitignored; use `git add -f` to commit)

### Configuration priority for API scripts
1. `--api-key` CLI flag
2. `DEEPSEEK_API_KEY` environment variable
3. `.env.local` auto-loaded via `python-dotenv`

### Model name
`deepseek-v4-flash` (not `deepseek-chat`).
