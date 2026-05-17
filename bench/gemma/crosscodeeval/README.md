# RepoBench Cross-File Completion

Evaluates spelunk's semantic search against RepoBench-Python cross-file
completion tasks.

## Conditions

| Condition | API calls | Tools | Purpose |
|-----------|-----------|-------|---------|
| `baseline_single_shot` | 1 | none | Single-shot completion, no loop |
| `multi_turn_no_tools` | <=5 | none | Same loop budget, no retrieval |
| `naive_search` | <=5 | `read_file`, `run_grep` | Loop with grep-level tools |
| `spelunk` | <=5 | `spelunk_search` | Loop with semantic search |

## Indexed repo mismatch

The RepoBench dataset spans 1,751 repos; no single repo has more than 86 tasks.
Spelunk indexes a single repo, so naive sampling produces near-0% overlap.

### Option A: Per-task indexing (most accurate, heavy)

Clone and index the source repo for each sampled task. ~50 clones +
index runs. Many repos may have moved or disappeared.

### Option B: Sub-select to one repo (recommended)

Filter tasks to a single repo we have indexed.

Recommended repo: `mpenning/ciscoconfparse2` (86 tasks in `cross_file_first`,
the largest single-repo slice of the 8,033-task dataset). Next largest:
`MarilynKeller/aitviewer-skel` (64), `Jisshubot/JISSHU_BOT` (63).
For `--samples 50` runs, ccp2 is the only repo with comfortable headroom.

```bash
git clone https://github.com/mpenning/ciscoconfparse2.git /tmp/ccp2
spelunk index /tmp/ccp2

python bench/gemma/crosscodeeval/evaluate.py \
    --condition spelunk \
    --repo-path /tmp/ccp2 \
    --repo-filter mpenning/ciscoconfparse2 \
    --samples 50 \
    --out bench/results/repobench-spelunk-ccp2.json
```

The `--repo-filter` flag ensures 100% overlap. The script cross-validates
against the git remote of `--repo-path` and warns on mismatch. Output
includes measured `indexed_repo_overlap_pct`.

### Option C: Transfer test (lightest)

Use any indexed repo without filtering. Frame results as a
transfer/generalisation test. Overlap will be near 0%.
