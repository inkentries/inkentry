# Memory Benchmarks

## Decision Archaeology

Measures whether spelunk memory can retrieve design rationale from git history
better than lexical search (grep, FTS5).

### Blindness Protocol

Questions MUST be authored without access to the harvested spelunk memory
database. The protocol:

1. **Source material:** Read raw `git log` output for the target repo. Use
   `git log --format="%H %s"` or GitHub PR/commit pages. Do NOT run
   `spelunk memory list` or `spelunk memory search`.
2. **Question authoring:** Write natural-language questions a developer would
   genuinely ask about the codebase's history. Examples:
   - "How does error handling work in the parser?"
   - "Why was async I/O chosen over threads for the network layer?"
   - "What tradeoffs led to the current lock-free queue design?"
3. **Ground truth:** For each question, record the commit SHA(s) that best
   answer it. Derive this from the raw git log, NOT from memory entries.
4. **Review:** Have the question set reviewed by a second party with no
   access to the spelunk memory database.
5. **Format:** Save as `bench/memory/questions-<repo>.json`:
   ```json
   [
       {
           "question": "How does error handling work in the parser?",
           "ground_truth_commit": "abc123def456"
       }
   ]
   ```

### Script

```
bench/memory/author_questions.py   — extracts git log for blind authoring
bench/memory/decision_archaeology.py — runs four-condition comparison
```

### Authoring workflow

```bash
# 1. Export raw git log (no spelunk access)
python bench/memory/author_questions.py \
    --repo-path /path/to/repo \
    --num-commits 500 \
    --out bench/memory/raw-commits-<repo>.json

# 2. Read the raw-commits file (NOT spelunk memory output).
#    Author ≥10 questions per repo. Record ground-truth commit SHAs.

# 3. Save questions
#    (hand-write into bench/memory/questions-<repo>.json)

# 4. Index + harvest memory, then run benchmark
spelunk index /path/to/repo
cd /path/to/repo && spelunk memory harvest --git-range HEAD~500..HEAD
python bench/memory/decision_archaeology.py \
    --repo-path /path/to/repo \
    --questions bench/memory/questions-<repo>.json \
    --out bench/results/archaeology-<repo>.json
```

### Four conditions

| Condition | Query | Search target |
|-----------|-------|---------------|
| `grep_literal` | Full question verbatim | `git log --grep` |
| `grep_keywords` | Regex-extracted keywords | `git log --grep` per keyword |
| `fts_commit_messages` | Full question | SQLite FTS5 over all commit messages |
| `memory_search` | Full question | `spelunk memory search` (semantic) |

## Cross-Session Handoff

See `bench/memory/cross_session_handoff.py`. Under redesign per issue #228.

### Current limitations

The 2026-05-15 proof-of-concept (n=1, no correctness check) is a demo,
not a benchmark. See #228 for the redesign requirements.
