# ADR-103: A code full-text index built for retrieval

**Date:** 2026-09-24
**Deciders:** Founder (Johan)
**Relationship to prior ADRs:** follows
[ADR-102](102-remove-linearrag-from-code-search.md), whose evaluation
identified this as the largest remaining gain that needs no model. Changes
the full-text half of the code corpus's hybrid search; the vector half, the
fusion ([ADR-081](081-unified-search-rank-fusion.md)) and the memory corpus's
own full-text index are unchanged. An `index.db` schema step, numbered after
ADR-097's `target_file` step.

## Context

`chunks_fts` indexed a chunk's `name`, `content` and `node_type` with FTS5's
default `unicode61` tokenizer, and the query was its whitespace-separated
words, OR-ed. That leaves out most of what a searcher knows about a chunk:

- The **file path**, the **docstring** (held in the `metadata` JSON) and the
  deterministic **structural summary** were indexed only for the vector side.
- **camelCase and PascalCase identifiers stayed whole.** `unicode61` splits
  `snake_case` on `_` but indexes `LinearRag` as `linearrag`, which `linear`
  or `rag` never reach.
- **No stemming.** `burst` missed `bursts`.
- **Question words** such as `how`, `does` and `the` each admitted every chunk
  containing them as a candidate.

This matters in two places. It is the whole ranking under `--only-text` or
without an embedder. It is also half of the hybrid pool.

We ran the ADR-102 known-item evaluation on both repositories (inkentry and
lago), adding one component at a time. Each row is the gain in Recall@10,
in points, over today's full-text search:

| component | inkentry | lago |
|---|---|---|
| path, docstring and summary as columns | +5 | +12 |
| Porter stemming | +1 | +4 |
| query stopwords | +1 | +5 |
| column weights | ~0 (lifts R@1 and MRR) | ~0 (lifts R@1 and MRR) |
| camelCase parts (chunk text and path) | +0.6 | +3 |

## Decision

1. **`chunks_fts` becomes a contentless FTS5 table** (`contentless_delete`)
   over `name, path, doc, summary, content`, tokenized `porter unicode61`.
   - It is contentless because four of the five columns are derived. Nothing
     reads a value back out of it, and a row can be deleted by rowid without
     reproducing what it was indexed with.
   - Triggers on `chunks` keep it in step. Path comes from `files`, and the
     docstring from `json_extract(metadata, '$.docstring')`.
2. **Identifier parts are computed in Rust at write time**
   (`search::lexical::identifier_subwords`) and stored on the row:
   - `chunks.name_words`
   - `chunks.body_words` (docstring and content)
   - `files.path_words`

   The triggers append them to the name, content and path columns. Only this
   part needs Rust; everything else is SQL.
3. **The update trigger fires only on the columns the index reads.** The old
   trigger fired on every `UPDATE chunks`, so each index run re-tokenised
   every chunk when it rewrote `graph_rank`.
4. **The query is built by `search::lexical::code_fts_query`:**
   - its words, lowercased;
   - plus the parts of any compound identifier;
   - minus stopwords, unless nothing else is left;
   - deduplicated, each quoted and OR-ed.
5. **BM25 weighs name 6, path, docstring and summary 2 each, content 1.**
6. **`search_text` returns the raw BM25 score as `distance`.** FTS5 makes that
   more negative for a better match, so lower is better, as it is for vector
   search. It used to return the negated score under a comment claiming the
   opposite.

## Consequences

- **Existing indexes migrate in place** (schema step): the FTS table is
  dropped and recreated, and the sub-word columns are backfilled.
  - The backfill's `UPDATE`s fire the new trigger, which populates the index.
  - Nothing is re-parsed or re-embedded.
  - Measured: 1.4 s for 9.7k chunks and 3.2 s for 33.6k.
- **Measured through the CLI on both repositories:**

  | R@10 | inkentry | lago |
  |---|---|---|
  | `--only-text` | 0.515 → 0.603 | 0.456 → 0.701 |
  | default search | 0.744 → 0.771 | 0.752 → 0.821 |

  On lago, full-text search alone now beats the old default hybrid search.
- **Costs:**
  - about 2 MB more in `chunks` for 9.7k chunks (the sub-word columns);
  - the FTS index itself is the same size;
  - no measurable change in parse time.
- **`--only-text` JSON `distance` values change sign.** Results are ordered as
  before; fusion is rank-based and never read the value.
- **Out of scope:** the memory corpus keeps its own phrase-matching FTS
  (`memory_fts`).
