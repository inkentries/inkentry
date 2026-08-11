# ADR-081: Unified search — rank-fusion over code and memory

**Date:** 2026-08-08
**Deciders:** founder (Johan); architect
**Relationship to prior ADRs:** pairs with
[ADR-082](082-unified-search-command-surface-collapse.md), which collapses the
command surface that exposes the ranking this record decides — 081 is *how* the
two corpora become one ranked list, 082 is *what* the user types to get it. Its
partial-index behaviour builds on ADR-080's coverage/freshness/tier model (the
shared re-embed work) and does not invent a parallel one. Memory results carry
the UUIDv7 identity fixed by [ADR-078](078-uuidv7-memory-entry-identity.md).

> **Amended 2026-08-11 by
> [ADR-083](083-memory-relevance-gate-in-unified-search.md).** Two sections of
> this record are superseded: *"The 1:1 interleave — accepted for v1"* and the
> *"Per-corpus weighted RRF in v1"* entry under *Alternatives considered*.
> Memory is now gated on its own absolute relevance before it reaches fusion,
> so it appears only when relevant. Everything else here stands and is
> reaffirmed by ADR-083 — two query embeds, rank-only cross-corpus fusion,
> `RRF_K = 60`, the code-before-memory tie-break, the result envelope, and the
> partial-index semantics. In particular §3's proof that `k` is inert
> cross-corpus, and its prohibition on comparing a code distance to a memory
> distance, are unchanged: ADR-083's threshold is compared only to a memory
> distance in the memory embedding space, never to a code distance.

## Context

The product thesis for `search` is one result that answers both halves of a
question at once — "the function *and* the decision that set its 800 ms
timeout." Delivering it means one command returns **code chunks and memory
entries interleaved**, ranked into a single list.

Two retrieval pipelines already exist and are kept:

- **Code corpus** — `crates/inkentry-core/src/search/rag.rs::linearrag_search`
  over the hybrid pool in `storage/search.rs` (int8 vector KNN blended with
  FTS5 BM25 over `chunks_fts`), cross-DB merged and deduplicated. It reports a
  final `distance` re-expressed as `1 - blended_score`.
- **Memory corpus** — `storage/memory/search.rs::search_hybrid` (note vector
  KNN fused with `memory_fts` BM25). It reports `distance = 1 / rrf_score`.

The trap that makes a naive union wrong is not that the pipelines differ — it
is that the **query is embedded twice under different instruction prefixes**,
and the two corpora were embedded to match their own prefix's task:

- Code search uses `Instruct: Given a code search query, retrieve the relevant
  code snippets`.
- Memory/QA uses `Instruct: Given a question, retrieve passages that answer the
  question`.

A code-search distance and a QA distance are therefore two numbers produced by
different instructions, on scales the two pipelines then re-express
differently (`1 - blended_score` vs `1 / rrf_score`). **Merging the two lists
by raw distance compares incomparable numbers.** The result looks plausible and
is quietly wrong: no test fails, no error is raised, retrieval just gets worse
in a way nobody can point at. This is the whole problem this ADR exists to
foreclose.

## Decision

**Embed the query twice — once under the code prefix, once under the QA prefix
— run each corpus's existing pipeline to a ranked list, then fuse the two lists
by reciprocal rank fusion (RRF) on rank position alone, discarding the
incomparable per-corpus magnitudes.** The fusion constant is `RRF_K = 60`, and
the cross-corpus tie-break is **code before memory**.

For each candidate, `fused_score = 1 / (RRF_K + corpus_rank)`, where
`corpus_rank` is its 1-based position in its own corpus's ranked list. Merge
both lists, sort by `fused_score` descending, and truncate to `--limit`.

Consequently:

1. **Cross-corpus order comes only from rank.** The per-corpus `distance`
   survives in output as a within-corpus diagnostic but is never on the
   critical path of the merge, so the incomparable distances are never
   compared. This is the property that makes the decision correct by
   construction rather than by measurement.
2. **One fusion constant governs the whole system.** 60 is *already* the
   within-corpus RRF constant in both `storage/search.rs` and
   `storage/memory/search.rs` (`const K = 60.0` in each). The two local
   constants and the new cross-corpus one should be hoisted to a single shared
   `RRF_K`, so the invariant "one number to reason about, one to tune" is
   enforced in code, not just prose.
3. **Every result is typed.** The fused list is heterogeneous, so each result
   carries a `code`/`memory` discriminator and its fusion metadata (see
   *Result typing*).
4. **The merge is a total, deterministic order** (see *Determinism* under
   Consequences): the same query over an unchanged, fully-fresh index yields
   byte-identical ordering.

### Why RRF, and why the rejected options are rejected

- **(a) Raw-score merge across corpora.** Rejected — the silent-failure trap
  above.
- **(b) Normalise each corpus's scores into a shared range, then merge.**
  Rejected: min-max or z-score normalisation across two prefixes is still
  comparing distributions produced by *different instructions*, and the
  normalisation constants are themselves untestable magic — they trade one
  unjustified number for several.
- **(c) Rank fusion — discard cross-corpus magnitudes, fuse on rank position
  only.** Chosen. It never reads the incomparable numbers.

Rank fusion is provably the right *shape* independent of any benchmark. A
benchmark can tune `k`; it cannot rescue a raw-score merge across two prefixes.

### Why `k = 60`, fixed here rather than left to the implementer

1. **One system, one constant.** Reusing the value that already governs both
   within-corpus fusions means the whole retrieval system has exactly one
   fusion constant to reason about and, when a retrieval benchmark exists,
   exactly one to tune — not a zoo of per-layer constants.
2. **60 is the original-paper default** (Cormack, Clarke & Büttcher, 2009) and
   the ubiquitous baseline. Fixing it makes behaviour deterministic and
   specified; leaving it implementer's-choice is how a copied-from-a-paper
   magic number becomes undocumented and untunable later.
3. **The honest subtlety.** With two **disjoint** corpora (a chunk is never a
   note) and **equal weights**, `k` is an additive constant applied identically
   to both lists. Because `1 / (k + rank)` is strictly decreasing in `rank`, a
   code item at rank *i* and a memory item at rank *j* order purely by *i* vs
   *j* — the comparison is **invariant to k**. So the cross-corpus merge
   degenerates to a pure **rank interleave**: code rank *i* ties memory rank
   *i*, broken code-before-memory, and the cross-corpus order never touches the
   incomparable distances. `k`'s material effect is preserved **within** each
   corpus's hybrid fusion, where the FTS and vector lists **overlap** and
   summing `1 / (k + rank)` across the shared items genuinely depends on `k`.
   `k` regains a cross-corpus effect **only** if a per-corpus weight is
   introduced (`w · 1 / (k + rank)`) — which v1 does not ship. Recording this
   stops a future maintainer from "tuning cross-corpus k" to fix an
   interleave-ratio problem that only a weight can fix.

### The 1:1 interleave — accepted for v1 — SUPERSEDED by ADR-083

> **Superseded 2026-08-11 by
> [ADR-083](083-memory-relevance-gate-in-unified-search.md).** The measurement
> this section deferred has been taken (PR #44) and is worse than predicted:
> memory took **exactly** half of the top 10 on **all 500** benchmark queries,
> unconditionally, costing ~45% of code recall — because the memory side runs
> vector KNN and returns a full page regardless of relevance. The interleave is
> reversed. ADR-083 also records why the follow-up this section anticipated
> could not be the one it named: the benchmark that now exists is a
> *code-retrieval* benchmark, and code recall is monotone in a corpus weight, so
> that instrument can measure the harm but cannot choose a weight. The section
> is kept below unedited, because its reasoning was sound on its own terms and
> ADR-083's argument is a response to it.

Because the equal-weight merge is a pure rank interleave, a **thin memory store
still contributes roughly half of a small result set**: the top slots split
about evenly between the two corpora regardless of how few memory entries
exist. This is accepted for v1. The memory entries are the user's own recorded
decisions and requirements — worth surfacing — and `--only-code` is the escape
hatch for a caller that wants code alone. **Corpus-weighting** (a
relevance-governed mix, so a marginal memory store does not take half the top
slots) is deferred to a **future benchmark-driven follow-up**, because choosing
weights without a retrieval baseline is exactly the untestable-constant trap
this ADR rejects for scores. Equal-weight RRF is the honest default until
numbers justify a weight.

### Two query embeds, and eliding the second

Default `search` issues two query embeds (code prefix + QA prefix). When a
corpus filter makes one unnecessary, the second is **elided**:

| Invocation | Code embed | Memory embed | Corpora searched |
|---|---|---|---|
| default (no filter) | ✅ | ✅ | code + memory, fused |
| `--only-code` | ✅ | skipped | code |
| `--only-memory` | skipped | ✅ | memory |
| `--only-text` | skipped | skipped | FTS over in-scope corpora (0 embeds) |

`--only-code` and `--only-memory` are mutually exclusive.
[ADR-083](083-memory-relevance-gate-in-unified-search.md) adds one row: the
memory embed is also elided when the memory store holds **no embedded notes**,
since it cannot then produce a vector candidate. FTS still runs, so an
unembedded note stays reachable.

**Cost.** This takes the per-search embed count from 1 → 2 (elided to 1 under
`--only-*`, to 0 under `--only-text`). The added work is one extra
short-sequence query forward pass — the query is tens of tokens, so there is no
`seq²` blow-up. Because both embeds share the mutex-serialised local embedder,
they do not parallelise on a single-embedder host, so the two-embed path adds
approximately one short-query forward latency, not a doubling of end-to-end
`search` (KNN + FTS + fusion dominate on a warm index). The exact CPU-host
delta between the one-embed path (`--only-code`) and the two-embed default is
to be **measured at implementation** against the shared retrieval benchmark;
until numbers exist, the claim is this estimate plus its basis.

### Result typing — the agent contract

The fused list is heterogeneous, so every result carries a discriminator and
its fusion metadata. The envelope is **nested, not flat-spread**: nesting the
two payloads under `code`/`memory` keys avoids the colliding field names
between them (`distance`/`content` vs `body`, `project_name` vs
`source_project`) and reuses the existing `SearchResult`/`Note` serializers
unchanged.

```jsonc
{
  "type": "code",          // "code" | "memory" — required discriminator
  "fused_rank": 1,         // 1-based position in the fused list; null in the --graph appendix
  "fused_score": 0.01639,  // 1/(RRF_K + corpus_rank); comparable ONLY within this response; null in appendix
  "corpus_rank": 1,        // 1-based rank within its own corpus list; null in appendix
  "code":   { /* the existing SearchResult, verbatim */ }   // present iff type == "code"
  // or
  "memory": { /* the existing Note, verbatim */ }           // present iff type == "memory"
}
```

Exactly one of `code`/`memory` is present, matching `type`. Results are emitted
**in fused order**; consumers MUST treat the emitted order (or `fused_rank`) as
authoritative and MUST NOT re-sort by `distance`/`score`, which are incomparable
across corpora. `--format jsonl` emits one object per line; `--format json`
emits an array of the same objects. The human/text format interleaves in fused
order with a per-result type label so the interleave is legible. `--graph`
enrichment neighbours are appended **after** the ranked members with
`from_graph = true` and `fused_rank`/`fused_score`/`corpus_rank` = `null` — they
are attachments, not ranked members.

### Partial-index semantics (build on ADR-080)

Unified search binds to ADR-080's coverage/freshness/tier model rather than
inventing a parallel one. Coverage = chunks with any vector / total; freshness =
chunks whose vector reflects the current embedding text (the `embed_pending`
count), kept distinct from coverage.

1. **Hybrid where embedded, FTS elsewhere is already the behaviour and is
   preserved.** `search_hybrid` fuses vector (embedded chunks only) with FTS
   (`chunks_fts` covers *every* chunk from parse time), so a never-embedded
   chunk is still reachable via FTS. The code list therefore degrades
   *continuously* as the embed queue drains — not a binary switch. The code
   pipeline is not gated on full coverage.
2. **A stale-but-present vector (`embed_pending`) still participates** — it has
   *a* vector, so it is searchable; its rank may shift once re-embedded. This is
   the coverage-vs-freshness distinction: coverage can read 100% while freshness
   < 100%.
3. **The completeness notice reflects both signals, and the `No results found.`
   invariant extends to two corpora.** Print the bare `No results found.` only
   when **every in-scope corpus is complete** (coverage 100% *and* freshness
   100%); otherwise name the incomplete corpus and its fraction, and while
   freshness < 100% add that rankings may still shift. Notices go to **stderr**
   so json/jsonl stdout stays machine-clean.
4. **With the ast-grep live-search engine removed** (ADR-082), the
   zero-coverage / embedder-unavailable / stale-empty paths degrade to **FTS**
   (which covers every chunk), not to a structural-search fallback. An
   uninitialised directory is a runtime funnel to `inkentry init`, not a
   fallback.
5. **The memory corpus follows the same honesty** using existing signals; its
   FTS half (`memory_fts`) covers unembedded notes. No new memory-coverage
   subsystem is built.

## What breaks

- **Every search now issues up to two embeds** instead of one (bounded below by
  the elision table).
- **The code-search output shape changes** from a top-level `SearchResult[]`
  array to the nested envelope above. This is a hard cut, detailed in ADR-082;
  it is noted here because fusion is what forces the heterogeneous shape.
- **Consumers that sorted results by `distance` are wrong under fusion** and
  must switch to the emitted order / `fused_rank`.

## Alternatives considered

- **Raw-score cross-corpus merge.** Rejected — the silent-failure trap; it
  compares incomparable distances regardless of any benchmark.
- **Score normalisation, then merge.** Rejected — still comparing distributions
  produced by different instructions, and it introduces its own untestable
  constants.
- **Per-corpus weighted RRF in v1.** Rejected as premature: weights can only be
  set honestly against a retrieval baseline. Deferred to a benchmark-driven
  follow-up; equal-weight RRF is the honest default meanwhile.
  **Superseded by [ADR-083](083-memory-relevance-gate-in-unified-search.md)**,
  which rejects a per-corpus weight outright rather than deferring it — a
  query-independent multiplier cannot express "only when relevant", and the
  code-retrieval benchmark is monotone in the weight, so it has no interior
  optimum to find. The equal-weight fusion itself is kept; what changes is
  which memory candidates reach it.
- **Leave `k` to the implementer.** Rejected: a copied-from-a-paper magic
  number becomes undocumented and untunable. Fixing it to 60 with a stated
  reason keeps behaviour deterministic and, later, tunable in exactly one place.

## Consequences

- **One tunable number** for the whole ranking system, hoisted to a shared
  `RRF_K`. (ADR-083 adds a second constant, `MEMORY_MAX_QA_DISTANCE`, but not to
  the ranking: it governs *admission* to the memory corpus, is compared only
  within the memory embedding space, and never enters the merge.)
- **Determinism.** The fused order is a total, stable order — `fused_score`
  descending, then code-before-memory, then a stable per-corpus id — so the same
  query over an unchanged, fully-fresh index yields byte-identical ordering.
  This underwrites "same query, same answer" and must not depend on `HashMap`
  iteration order or float-equality in the merge or tie-break.
- **No new egress.** Both embeds go through the same local loopback embedder
  path, so the two-embed change adds no outbound destination. The raw user
  query is only ever wrapped by the existing prefix helpers; it is never
  interpolated into SQL or a shell, and the corpus filters use bound
  parameters, as the existing `search_*` queries already do.

## Prerequisites

Docs-only decision; not yet implemented. Implementation is gated on (a)
sign-off of this ADR and ADR-082, and (b) a shared retrieval-benchmark baseline
(recall@k / MRR) captured *before* this ranking change and re-run after —
deferred, and shared with the summaries/re-embed work behind ADR-080. Rank
fusion is the right shape regardless of that baseline; the baseline confirms
whether `k = 60` is good and is tunable after the fact.
