# ADR-104: Embed a subset of code chunks; leave the rest to full-text search

**Date:** 2026-09-25
**Deciders:** Founder (Johan)
**Relationship to prior ADRs:** follows
[ADR-102](102-remove-linearrag-from-code-search.md) and
[ADR-103](103-code-full-text-index-for-retrieval.md), whose evaluation it
reuses. It changes which code chunks the embed phase queues; the hybrid fusion
([ADR-081](081-unified-search-rank-fusion.md)), the full-text index and the
memory corpus are unchanged. It adds an `index.db` schema step, numbered
after ADR-103's.

## Context

Embedding is most of the cost of indexing. On Lago, a medium Rails and React
repository, a full embed takes about an hour on a laptop. ADR-102's
evaluation showed that the vectors earn their keep: they add 12 to 17 points
of Recall@10 over full-text search alone, almost all of it on concept
queries. The question is whether every chunk needs one.

Most of the tokens go to chunks a searcher reaches by name or not at all.
Sized on each repository, as a share of the tokens the embed phase sends
(parse-only indexes built with the current chunker):

| category | Lago | inkentry |
|---|---|---|
| test files | 57% | 31% |
| unnamed windows of code (the gaps between definitions, whole files with no named node) | 9% | 10% |
| changelogs | 0% | 2% |
| JSON | 0.1% | 1% |
| **embedded under this ADR** (measured with the rule itself) | **34%** | **54%** |

Ordering the queue does not help much on its own. The queue is already
ordered by PageRank and recency, so capping it at a fixed point would drop
whichever chunks come last, and on Lago that means most of the React
components.

## Decision

1. **A chunk is left to full-text search alone, and never queued for
   embedding, when it is:**
   - in a test file: a `test`, `tests`, `spec`, `specs`, `__tests__`,
     `__mocks__`, `testdata` or `e2e` directory, or named like one
     (`*_test.go`, `*_spec.rb`, `*.test.ts`, `test_*.py`, `FooTest.java`,
     `tests.rs`);
   - in a changelog (`CHANGELOG*`, `CHANGES*`, `HISTORY*`, as Markdown or
     text);
   - JSON;
   - an unnamed window of code: a `verbatim` chunk with no name, in any
     language but prose (Markdown, text, notebooks and the rich document
     formats keep theirs).
2. **The rule is one function, `indexer::embed_scope::is_text_only`**, over
   columns the index already stores (path, language, node type, name). It is
   evaluated when a chunk is written and stored as `chunks.text_only`, so the
   embed queue, `status` and `search` agree on it without recomputing it.
3. **A text-only chunk is not re-embedded and not refined by tier 3.** Tier
   3 refines title-less chunks. In code those are now text-only, so tier 3 is
   left refining prose windows and unnamed `impl` blocks.
4. **`status` and `search` measure embedding coverage over the embeddable
   chunks.** `status` names the text-only count
   (`Embeddings: N (M chunks full-text only)`, and `text_only_count` in
   `--format json`). A fully embedded index no longer reads as incomplete.
5. **`*-lock.json` joins the default excludes.** The other lockfiles were
   already there; `skills-lock.json` and the like were not.

## Consequences

- **Measured by simulation** on the two indexes ADR-102 embedded in full,
  with the ADR-103 full-text index, by removing text-only chunks from the
  vector side of the hybrid ranking (375 queries each):

  | | Recall@10 | MRR@10 | 95% CI of the R@10 change |
  |---|---|---|---|
  | Lago, all embedded | 0.821 | 0.621 | |
  | Lago, text-only skipped | **0.861** | **0.648** | +0.021 to +0.059 |
  | inkentry, all embedded | 0.771 | 0.528 | |
  | inkentry, text-only skipped | 0.779 | 0.552 | −0.008 to +0.024 |

  Leaving tests out of the vector search is itself a gain. Lago has 15.6k
  test chunks against 31k others. Their vectors sit close to the code they
  test and take its places in the KNN list, where they are rarely what the
  query wanted. The same query still reaches a test through the full-text
  side.
- **What this does not measure:** none of the 375 targets is a text-only
  chunk (targets were sampled from named non-test definitions and doc
  sections), so the cost to a query that is looking for a test is not
  measured. Such queries usually name what the test exercises, which the
  full-text index matches. The simulation also ran on the chunking from
  before gap windows were added; the real hybrid search is to be re-measured
  after merging, on fresh indexes.
- **Existing indexes migrate in place.** The schema step adds the column and
  computes it for every chunk. A text-only chunk that already has a vector
  keeps it, and counts as embedded. Nothing is re-embedded.
- **Fusion is unchanged.** A text-only chunk enters the hybrid ranking only
  from the full-text list, so it scores one reciprocal-rank term against an
  embedded chunk's two. The simulation above includes that effect.
- **Not configurable.** A setting to embed everything is easy to add if a
  repository turns out to need it. Nothing measured so far asks for one.
