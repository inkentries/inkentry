# ADR-083: Memory earns its place — a within-corpus relevance gate before fusion

**Date:** 2026-08-11
**Deciders:** founder (Johan); architect
**Relationship to prior ADRs:** amends
[ADR-081](081-unified-search-rank-fusion.md). It **supersedes** exactly two of
its sections — *"The 1:1 interleave — accepted for v1"* and the
*"Per-corpus weighted RRF in v1"* entry under *Alternatives considered* — and
leaves the rest of ADR-081 standing and reaffirmed: two query embeds, rank-only
cross-corpus fusion, `RRF_K = 60`, code-before-memory tie-break, the result
envelope, and the partial-index semantics are all unchanged. It touches
[ADR-082](082-unified-search-command-surface-collapse.md) only to remove a
dependency on it (see *`--only-code` after this change*).

## Context

ADR-081 accepted a 1:1 code/memory interleave for v1 and deferred corpus
weighting to "a future benchmark-driven follow-up." The benchmark now exists,
the measurement has been taken (PR #44), and it is worse than the record
anticipated.

500 CodeSearchNet queries, a fully-embedded 575-chunk index, 20 memory entries
that are ordinary engineering notes from an unrelated project:

| metric | `--only-code` | unified default |
|---|---|---|
| MRR@10 | 0.1921 | 0.1180 |
| Recall@5 | 0.358 | 0.186 |
| Recall@10 | 0.650 | 0.356 |
| memory entries in top 10 | 0.0 | **5.0** |

ADR-081 predicted memory would take "roughly half of a small result set."
Measured: **exactly half, on all 500 queries, unconditionally.** The identity
confirms the mechanism — unified Recall@10 (0.356) is code-only Recall@**5**
(0.358), because fused positions 1,3,5,7,9 are precisely code ranks 1–5. A
caller gets the top five code results plus five memory entries, every time,
whether or not any entry has the faintest bearing on the question.

**Ruling: the 1:1 interleave is wrong. Memory appears only when relevant.**

### Why rank alone fails here — the half ADR-081 §3 did not state

ADR-081 §3 is correct that with disjoint corpora and equal weights the
cross-corpus comparison is invariant to `k`, and correct that this makes the
merge a pure rank interleave. What it does not say is *why that interleave is
not a defensible default*, and the reason is not a weighting oversight:

**A rank position carries information about relevance only in proportion to how
many candidates it beat.** Code rank 1 was selected out of thousands of chunks;
it is a strong statement. Memory rank 1 was selected out of twenty notes; it is
very nearly no statement at all — with twenty entries, *something* is always
nearest. RRF's rank-only merge is blind to this asymmetry by construction: it
sees two "rank 1"s and treats them as equals. On corpora that differ in size by
two or three orders of magnitude, "rank 1" on the small side is a formality.

That is why the fix cannot be a different way of comparing ranks. **Rank
selectivity has to be replaced, on the small corpus, by the absolute signal that
rank was standing in for.**

### What ADR-081's incomparability argument actually forbids

ADR-081 forbids comparing a *code* distance to a *memory* distance: two numbers
produced under different instruction prefixes, on scales the two pipelines
re-express differently (`1 - blended_score` vs `1 / rrf_score`). That prohibition
is right and is not relaxed here.

It does **not** forbid comparing a memory distance to a *memory-space* constant.
A within-corpus admission test and a cross-corpus score merge are different
operations: the first asks "is this note about the question at all," answerable
entirely inside one embedding space; the second asks "is this note better than
that chunk," which the two prefixes make unanswerable. This ADR does the first
and never the second.

## Decision

**Gate the memory corpus on its own absolute relevance before it reaches
fusion. Fuse the survivors with equal weight, exactly as ADR-081 specifies.**

Concretely, in the memory retrieval pipeline
(`crates/inkentry-core/src/storage/memory/search.rs`), applied to the
**candidate pool, before** the within-corpus RRF:

1. **The vector door.** A candidate from the note KNN half is admitted only if
   its distance to the QA-prefixed query embedding is at most
   `MEMORY_MAX_QA_DISTANCE`. `note_embeddings` is a `vec0` `FLOAT[896]` table
   with the default L2 metric over L2-normalised vectors, so the reported
   distance is `sqrt(2 - 2·cos)` and the constant is stated in the ADR and in
   code as **both** the L2 value the query returns and its cosine equivalent.
2. **The lexical door.** A candidate reached by the `memory_fts` BM25 half is
   admitted on its lexical match. This door exists because it is the only way an
   **unembedded** note is reachable at all, and ADR-081 §*Partial-index
   semantics* (5) requires that path to keep working. It is safe **only while
   the memory FTS matcher demands strong lexical evidence**: `search_text`
   currently matches through `fts5_quote_literal`, i.e. the entire query as one
   contiguous phrase, so a note that does not literally contain the query does
   not match. See *The lexical door is coupled to the memory FTS matcher*.
3. **Survivors are ranked among themselves.** The within-corpus RRF runs over
   the admitted candidates only, so a memory `corpus_rank` of 1 means "the best
   entry that cleared the bar," not "the least-distant of twenty."
4. **Fusion is untouched.** `fuse()` in `cli/cmd/fusion.rs` keeps equal weights,
   `RRF_K = 60`, rank-only ordering and the code-before-memory tie-break. No
   weight, no cap, no quota, no share parameter. **The memory share of a result
   page is an outcome, not a parameter** — if ten entries genuinely clear an
   absolute bar, ten entries deserve their slots, and that is the product thesis
   working rather than failing.
5. **Scope.** The gate applies wherever memory competes for shared slots: the
   default unified search and `--only-text` (whose memory half is FTS, governed
   by door 2). `--only-memory` is **ungated** — with no code corpus in scope
   there are no slots to protect, the caller has explicitly asked to interrogate
   the memory store, and the full page is what they asked for. This is a
   difference in *admission*, not in *ranking*: the ordering rule is identical in
   both, exactly as the ADR-081 embed-elision table already varies work by
   scope. It also gives a defined diagnostic — `--only-memory` shows what the
   default suppressed.
6. **`MEMORY_MAX_QA_DISTANCE` is a single `const` in core, next to the search it
   governs.** It is **not** a config key, not an environment variable and not a
   CLI flag. A user-tunable relevance floor is an untestable constant delegated
   to the user, and it would break "same query, same answer" across machines.
   Its doc comment records the calibrated value, the cosine equivalent, the
   calibration date, the dataset, and the model id + instruct prefix it was
   calibrated against.
7. **Corollary — elide the QA embed when the memory store holds no vectors.**
   A store with zero embedded notes cannot produce a vector candidate, so the
   second embed is pure cost. Add that case to ADR-081's elision table. FTS still
   runs, so an unembedded-but-present note stays reachable.

### The constant is a property of one embedding space, not a mixing ratio

This is the reason the mechanism is buildable and the alternatives are not.
`MEMORY_MAX_QA_DISTANCE` answers "at what QA-prefix cosine does a passage stop
answering a question," which is a fact about F2LLM-v2-330M under one instruct
prefix. It does not encode any judgement about how much a memory entry is worth
relative to a code chunk. Nothing in it is a trade-off, so nothing in it needs a
trade-off benchmark to set.

**It is versioned with the model and the prefix.** Changing `MODEL_ID` or the
memory/QA instruction string invalidates the calibration, and the change that
does so must re-run it. That is a maintenance rule a reviewer can enforce,
because the doc comment names both.

## How the constant is chosen — from measurement

The calibration does not use the retrieval harness and does not run the code
pipeline. It is query-embed, note-embed, dot product.

**The negative distribution is free and enormous.** The 500 CodeSearchNet
queries are, by construction, irrelevant to every entry in the 20-entry memory
store — they are Python docstrings; the entries are engineering notes from an
unrelated project. That is **10,000 labelled true-negative (query, note) pairs**
with no annotation effort and no judgement calls, embedded under the QA prefix
by the shipped embedder.

**The positive set is small and must be built deliberately.** For each memory
entry, one question that entry answers, written as a paraphrase sharing at most
one content word with the entry — otherwise the calibration measures lexical
overlap rather than semantics, and door 2 already covers lexical overlap. Twenty
paired positives. Commit both sets.

**Procedure.**

1. Embed all queries under the QA prefix and all notes as documents. Record the
   full 10,000-value negative distance distribution and the 20 paired positive
   distances.
2. Report the negative distribution's percentiles and the positive
   distribution's quantiles side by side. **This is the test of the design
   itself, before it is the choice of a number:** if the two overlap heavily,
   an absolute floor cannot separate relevant from irrelevant memory in this
   embedding space and the mechanism must be abandoned rather than tuned.
   Publish the overlap in the PR that implements this.
3. Set `MEMORY_MAX_QA_DISTANCE` at a **low percentile of the negative
   distribution** — the 1st percentile is the starting proposal — and report the
   positive set's recall at that value. Choosing off the negative side is the
   right way round: the negative sample is 500× larger and label-free, and the
   failure this ADR exists to fix is a false-positive failure.
4. Report the behaviour across the whole band, not just at the chosen point. If
   the negative 0.1st, 1st and 5th percentiles all give the same acceptance
   outcome, the parameter is not on a knife edge and the middle of that band is
   the value. If they do not, say so — that is the mechanism telling you it is
   fragile.

**Resolution.** Every step is a deterministic embed and a dot product, so the
±1.4pt run-to-run noise in the recall harness does not constrain this choice at
all. That is not a happy accident; see the next section.

## Why the alternatives lose

### A per-corpus weight `w · 1/(k + rank)` — rejected

Two independent reasons, either of which is sufficient.

**1. A weight is query-independent, so it cannot express "only when relevant."**
Work out what a weight actually does. Memory rank *j* outranks code rank *i*
iff `w/(k+j) > 1/(k+i)`, i.e. iff `i > (k+j)/w − k`. So memory rank *j* behaves
exactly like code rank `j/w + k(1−w)/w`, and with `k = 60` dominating for small
*j*, **a weight is a constant rank penalty**: "push memory down by a fixed
number of positions, always." At `w = 0.9` memory's first entry lands at fused
position 8 and its second at 10 — so an entirely irrelevant store takes 2 of
every 10 slots, on every query, forever. That is a smaller wrong answer to the
question, not an answer to it. The ruling is *conditional* presence, and no
query-independent multiplier is conditional on anything.

**2. The available instrument is monotone in `w`, so it has no interior
optimum.** ADR-081 deferred the weight to "a benchmark-driven follow-up." The
benchmark that now exists is a **code-retrieval** benchmark. Code recall
increases monotonically as `w` falls, all the way to `w = 0`. An instrument
whose optimum is "turn the feature off" cannot choose a value for the feature;
any interior `w` picked against it is taste wearing a measurement's clothes —
which is precisely the untestable-constant trap ADR-081 rejects for scores.
Choosing `w` honestly needs a *joint* benchmark carrying labelled memory
relevance for the same queries. That does not exist, and building it is a much
larger undertaking than this decision.

To be exact about ADR-081's deferral, since it was well-reasoned and this ADR is
reversing its outcome: its precondition was "a retrieval baseline exists." What
now exists is a baseline that can **measure the harm** but cannot **choose the
weight**. Half the precondition was met. The other half is not reachable with
this harness.

The sensitivity is a third, lesser objection: on a 10-result page the whole
usable range of `w` is roughly (0.85, 1.0], and 0.01 of `w` moves memory ~0.6
positions. And per ADR-081 §3 a weight re-activates `k` as a cross-corpus lever,
so a weight means tuning two coupled constants against a ±1.4pt instrument.

### A cap on memory's share of the page — rejected

Same first objection, undiluted: a cap of *c* is **filled** whenever the store is
non-empty, so memory is still unconditionally present, just less of it. Same
second objection: code recall is monotone in *c*, so the benchmark drives *c* to
zero and cannot choose an interior value. A cap also fails hardest exactly where
the founder's complaint bites — a 3-entry store with nothing relevant in it still
delivers `min(c, 3)` irrelevant entries to the top of the page.

### A contribution scaled by the memory side's own score distribution — rejected

Softmax, z-score, or a spread-relative confidence over the memory candidates is
**scale-free**, and scale-free statistics cannot detect "everything here is
bad." Twenty irrelevant notes still have a nearest one and still have a spread;
normalising within that set manufactures confidence out of the absence of
competition. It also degenerates on the case that motivated this: with one or
two entries the spread is undefined or pure sampling noise. And it needs its own
temperature or cut-off constant, inheriting the monotone-instrument problem
whole.

### A size-aware weight, e.g. discounting memory by `log(N_code / N_memory)` — rejected

The natural reply to the selectivity argument above, and still wrong for reason
1: it is a function of corpus sizes, not of the query. It fixes the *average*
share and never yields zero on an irrelevant query, while over-suppressing a
small store that genuinely is relevant — penalising a 20-entry store for being
small on exactly the query its one perfect entry answers.

### Changing cross-corpus `k` — not considered

ADR-081 §3 proves `k` cancels from every cross-corpus comparison under equal
weights, and explicitly warns against tuning it to fix an interleave ratio. It
is inert here. Recorded so the next reader does not re-derive it.

## What this costs, honestly

ADR-081's fusion is "correct by construction rather than by measurement" — it
never reads a magnitude, so it cannot be silently wrong about one. **This ADR
gives up part of that property**: one absolute magnitude now sits on the memory
side of the pipeline. The claim is not that this is free. It is that the trade is
sound, for four reasons:

1. **The magnitude lives in one embedding space** and is compared only to a
   constant in that same space. The cross-corpus path still reads nothing but
   integer ranks; the mutation test that guards it (`fuse()` sorted by
   `(corpus_rank, corpus_priority)`, no float comparison) is unaffected.
2. **A wrong threshold fails visibly, in one of two named directions**, each
   with a test below: too strict and criterion C fails; too loose and criterion A
   fails. A wrong cross-corpus score merge — the thing ADR-081 forbids — fails
   *silently*, which is the entire reason for the prohibition. Different failure
   mode, different treatment.
3. **Its calibration is deterministic and does not ride the noisy instrument.**
4. **It is one constant, in one place, with its provenance in its doc comment.**

The residual assumption, stated plainly: after gating, a surviving memory rank 1
is still interleaved at parity with code rank 1. That is the same parity
assumption ADR-081 made; this ADR makes it *conditional* rather than removing it.
If parity turns out to be wrong for entries that clear the bar, that is a
separate finding and needs the joint benchmark described above.

### The lexical door is coupled to the memory FTS matcher

Door 2 admits on lexical match alone, which is safe only because the memory FTS
half is strict. It currently matches through `fts5_quote_literal` — the whole
query as one contiguous phrase — whereas the **code** FTS half uses
`fts5_match_query`, which ORs the query's terms. That asymmetry is almost
certainly an oversight in its own right (it means the memory hybrid's BM25 half
is close to inert for multi-word natural-language questions, which is part of
why the memory list observed in PR #44 is effectively pure KNN).

**Binding constraint on whoever fixes it:** loosening the memory FTS matcher to
OR-of-terms reopens this hole through the lexical door — a query sharing one
common word with a note would admit it. The change that loosens the matcher must
in the same change re-gate door 2, either with a BM25 floor or a minimum
term-coverage requirement, and must re-run acceptance criterion A. Neither the
matcher change nor its gate is in scope here.

## Behaviour on the cases that motivated this

- **Empty memory store.** The memory list is empty, `fuse(code, [], limit)`
  returns the code list verbatim, and the QA embed is elided (decision 7), so
  the default costs exactly one embed and is byte-identical to `--only-code`.
  This was already the ranking behaviour; the elision is new.
- **Tiny store (1–3 entries).** No special case, and that is the point. Each
  entry is judged on its own absolute distance, so a three-entry store
  contributes three results, one, or none depending entirely on the query. Both
  the weight and the cap behave *worst* exactly here, because they scale a share
  of a page rather than testing an entry.
- **Entirely irrelevant store, the motivating case.** Every candidate fails both
  doors, memory contributes nothing, and the fused list is the code list. On the
  500-query benchmark this is the expected outcome for very nearly all 500.
- **No notice is emitted when memory is suppressed.** It would fire on the
  majority of code queries and become noise, and stdout must stay
  machine-clean (ADR-081 §*Partial-index semantics* 3). Silence is the correct
  answer; `--only-memory` is how a caller interrogates the store directly. The
  existing coverage/freshness notices are unchanged — they report *index*
  honesty, which is a different question from relevance.
- **Code corpus returns nothing and memory fails the floor.** Zero results, and
  the existing `No results found.` contract applies. There is deliberately no
  "rescue" that reinstates the memory page when code is empty — a special case
  would reintroduce the unconditional page through a side door.

## `--only-code` after this change

**ADR-081's justification of the interleave — "`--only-code` is the escape hatch
for a caller that wants code alone" — is withdrawn.** A default that is only
correct if the caller knows to reach for a flag is not a default, and it is in
direct tension with a v1 that sells six verbs, no modes, and "ask once." After
this change the default is the thing you want without knowing anything.

**The flag stays**, demoted from escape hatch to an ordinary filter, for two
reasons that have nothing to do with retrieval quality:

- **Cost.** It elides the QA embed and skips the memory pipeline. A harness
  measuring code retrieval wants exactly one pipeline, and so does a latency-
  sensitive caller.
- **Shape.** A machine consumer that wants a homogeneous result list gets one,
  without filtering the envelope itself.

**This matters for ADR-082.** That record's rejection of signposted "has moved
to…" errors was amended under contest in PR #44 and awaits founder
confirmation. Retrieval quality must not depend on the outcome, and after this
change it does not: nothing in the default path requires the caller to discover
`--only-code`. Whichever way the signposting question is settled, this decision
stands unchanged.

## Acceptance criteria

A reviewer can check every one of these. A, C, D and E are **exactly
reproducible** — the memory pipeline is KNN + FTS with no PageRank stage, so it
carries none of the ±1.4pt non-determinism that affects code recall. Only B
touches the noisy instrument, and its thresholds are set outside the noise band.

**A. Suppression (primary gate, noise-free).** Same 500 queries, same
fully-embedded 575-chunk index, same 20 unrelated entries. Mean memory results
in the top 10 of the unified default: **≤ 0.05** (baseline 5.0). Confirm the
count is identical across 3 runs.

**B. Code recall restored (bounded by the noise floor).** Over **≥ 3 paired
runs**, comparing medians of unified default against `--only-code`:

| metric | required | current gap |
|---|---|---|
| Recall@10 | \|Δ\| ≤ 0.020 | 0.294 |
| Recall@5 | \|Δ\| ≤ 0.020 | 0.172 |
| MRR@10 | \|Δ\| ≤ 0.010 | 0.074 |

The recall thresholds sit above the measured ±0.014 run-to-run band. The MRR
band has not been measured; the harness must print every run so the range is
visible, and if MRR proves noisier than 0.010 the threshold moves to the
measured band and the change is recorded here.

**C. Memory still reachable (the counter-gate — without this, deleting the
memory corpus passes A and B).** With the committed positive fixture of ≥ 20
(question, entry) pairs loaded alongside a code index, the unified default
places the target entry in the **top 10 for ≥ 90%** of the questions and in the
**top 3 for ≥ 70%**.

**D. Empty-store invariance.** With zero memory entries, unified default output
is byte-identical to `--only-code` for the same query, and issues exactly one
embed — asserted on request count against a mock, as
`corpus_filters_elide_the_redundant_query_embed` already does.

**E. Tiny-store invariance.** With a single entry that fails both doors, output
is byte-identical to the empty-store case.

**F. Calibration is reproducible.** The calibration script and both labelled
sets are committed. Re-running prints the negative percentiles, the positive
recall at the chosen threshold, and the behaviour across the surrounding band,
and yields the same `MEMORY_MAX_QA_DISTANCE`.

**G. The cross-corpus invariant is intact.** ADR-081's existing guards still
pass unmodified — in particular `orders_by_rank_not_by_incomparable_distance`
and `fused_order_interleaves_rank_positions_not_raw_distances`. The gate is
upstream of `fuse()`; if a change to `fuse()` is required to implement this,
that is a signal the gate has been put in the wrong place.

## On the harness

**The current harness is sufficient for this decision, and its known
non-determinism is not a prerequisite.** Stated plainly because the opposite
would make the PageRank fix a blocker:

- The threshold is calibrated on embeddings and dot products, not on recall@k,
  so the ±1.4pt band does not constrain it.
- The primary acceptance gate (A) is a count from a pipeline with no PageRank
  stage, and is exact.
- The counter-gate (C) is exact for the same reason.
- Only B rides the noisy path, and its thresholds are 0.020 against a ±0.014
  band, with the current failure at 0.294 — a factor of ~15 outside the noise.

Two things are nonetheless **asked of the harness**, neither blocking: report
paired repeat runs as median plus range rather than single values (the ±1.4pt
figure currently rests on n = 2), and report MRR@10 variance so criterion B's
MRR threshold rests on a measured band rather than an assumed one.

The root cause of the variance — `HashMap`-ordered edge-list construction and
f32 accumulation in the PageRank stage of `search/rag.rs` — is out of scope
here and tracked separately.

## What breaks

- **The default `search` result mix changes.** A caller that had come to expect
  memory entries in roughly half the slots will see them only when they clear
  the bar. On a store of unrelated entries, that is usually none.
- **`--only-memory` and the default can now disagree** about which entries are
  returned for the same query. This is intended and documented in `--help`: the
  ungated flag is the way to see the full page.
- **A new labelled fixture and a calibration script join the repo**, and the
  threshold's provenance must be maintained with the embedding model. Changing
  `MODEL_ID` or the memory/QA instruct prefix invalidates the calibration.

## Prerequisites

Docs-only decision; not yet implemented. Implementation is gated on (a) sign-off
of this record, and (b) the calibration in *How the constant is chosen* being run
and its separation reported — because step 2 of that procedure is capable of
falsifying the mechanism, and if it does, the right response is to come back
here rather than to pick a threshold anyway.
