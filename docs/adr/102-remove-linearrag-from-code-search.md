# ADR-102: Remove the LinearRAG re-rank from code search

**Date:** 2026-09-24
**Deciders:** Founder (Johan)
**Relationship to prior ADRs:** changes the code-corpus retriever that
[ADR-081](081-unified-search-rank-fusion.md) fuses with memory; the fusion
itself is unchanged. Retires the `mentions` graph that
[ADR-097](097-symbol-resolution-layer-above-tree-sitter.md) lists as a
consumer of symbol resolution.

## Context

With a query vector, the code side of `inkentry search` ran LinearRAG
(`search/rag.rs`) on top of the hybrid pool: symbols mentioned by the pool's
chunks were activated, a personalised PageRank ran over the chunk-symbol
bipartite graph, and each chunk scored `0.5 * normalised similarity + 0.5 *
normalised PPR`. The graph came from `mentions` edges written at index time,
one per identifier-like token in every named chunk.

It was never measured against the hybrid pool it re-ranked. We measured it
with a known-item evaluation: 125 target chunks per repository (functions,
classes and doc sections, tests excluded), three queries each (a
plain-language description with no identifiers, a 2-5 word keyword query, and
a query carrying one identifier), a hit being the target in the top 10. A
Python port of the ranking reproduced the CLI's own results exactly on both
repositories, which is what made the variants below comparable.

| | inkentry (9.7k chunks) | | lago (33.6k chunks) | |
|---|---|---|---|---|
| | R@1 | R@10 | R@1 | R@10 |
| hybrid pool + LinearRAG (shipped) | 0.011 | 0.565 | 0.093 | 0.571 |
| hybrid pool alone | 0.339 | 0.744 | 0.333 | 0.752 |
| full-text only (`--only-text`) | 0.224 | 0.515 | 0.245 | 0.456 |

The PPR term rewards chunks that mention many activated symbols, whatever
the query: on inkentry, `CHANGELOG-v0.md` sections were the top result for
dozens of unrelated queries. The re-rank made default search rank worse than
full-text search with no model at all (MRR 0.150 vs 0.314 on inkentry), and
removing it gained 18 points of Recall@10 on both repositories (95% bootstrap
CI +0.13 to +0.23).

## Decision

1. The code corpus ranks by `Database::search_hybrid` alone: vector KNN and
   FTS5 BM25, fused by reciprocal rank fusion. Linked dependency projects are
   searched the same way and merged by distance, as before.
2. `search/rag.rs`, the personalised PageRank it used, and the storage
   queries that read the mention graph are deleted.
3. Indexing stops writing `mentions` edges. They had no other reader, and
   they were the bulk of `graph_edges` (162,800 of 214,913 rows on inkentry).

## Consequences

- No schema change and no migration. An existing index keeps its `mentions`
  rows until each file is re-indexed (`replace_edges` clears a file's edges
  of every kind) or `inkentry index --force` rebuilds it. `EdgeKind::Mentions`
  stays so those rows still parse, and structural PageRank keeps excluding
  them.
- `inkentry plumbing graph-edges` stops reporting `mentions` edges for newly
  indexed files. They were never documented as part of its output.
- Search with a query vector is cheaper: no per-query personalised PageRank
  and no mention-graph expansion queries.
- The graph is still used where it was structural: PageRank for the embed
  queue and the `graph_rank` blend in KNN, and the `--graph` 1-hop appendix.

## What the same evaluation says next

Not decided here, recorded so the follow-ups start from the numbers.

- **The full-text index is the cheapest large win.** Splitting camelCase and
  snake_case identifiers, Porter stemming, and indexing name, path, docstring
  and structural summary as weighted columns took full-text-only Recall@10
  from 0.515 to 0.603 (inkentry) and 0.456 to 0.704 (lago), without a model.
- **Vectors still earn their place, mostly for descriptive queries.** Fused
  with that better full-text index, vectors added 17 points of Recall@10 on
  inkentry and 12 on lago; for plain-language queries 28 and 18, for
  identifier queries roughly nothing.
- **Embedding can be restricted by kind rather than cut by queue position.**
  Skipping test files, changelogs and unnamed verbatim windows kept 42%
  (inkentry) and 25% (lago) of the tokens with no recall loss on these
  queries, which were drawn from what remained, so this is an upper bound.
  Cutting the current queue at 25% or 50% instead did worse: markdown
  sections sort last (they have no `graph_rank`), and RRF penalises a chunk
  with no vector, so doc-section recall fell below full-text alone.
