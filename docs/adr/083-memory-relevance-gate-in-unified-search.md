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

1. **The gate is a vector-distance floor.** A candidate from the note KNN half
   is admitted only if its distance to the QA-prefixed query embedding is at
   most `MEMORY_MAX_QA_DISTANCE = 1.2032`. `note_embeddings` is a `vec0`
   `FLOAT[896]` table with the default L2 metric over L2-normalised vectors, so
   the reported distance is `sqrt(2 - 2·cos)`; 1.2032 L2 is cosine 0.2762. Both
   forms are stated in the constant's doc comment.
   **Placement:** gate the output of `MemoryStore::search()` — the KNN half —
   **not** the output of `search_hybrid`. `search_hybrid`'s result-mapping
   closure sets `n.distance = Some(1.0 / rrf_score)`, destroying the raw vector
   distance the gate needs. Gating upstream of that overwrite avoids plumbing a
   new field through `Note`, and the spike confirmed it works.
2. **There is no second door.** An earlier draft of this record admitted a
   candidate on a `memory_fts` BM25 match as well, to keep unembedded notes
   reachable per ADR-081 §*Partial-index semantics* (5). **Measurement retired
   that door**: the memory FTS matcher requires the entire query as one
   contiguous phrase (`fts5_quote_literal`), and across the calibration set it
   matched **0 of 500** negative queries and **0 of 20** positive ones.
   Disabling it outright changed no number. **The mechanism is vector-only**,
   and the record says so rather than describing a two-door design that is one
   door in practice. See *What the dead lexical path means*.
3. **Survivors are ranked among themselves.** The within-corpus RRF runs over
   the admitted candidates only, so a memory `corpus_rank` of 1 means "the best
   entry that cleared the bar," not "the least-distant of twenty."
4. **Fusion is untouched.** `fuse()` in `cli/cmd/fusion.rs` keeps equal weights,
   `RRF_K = 60`, rank-only ordering and the code-before-memory tie-break. No
   weight, no cap, no quota, no share parameter. **The memory share of a result
   page is an outcome, not a parameter** — if ten entries genuinely clear an
   absolute bar, ten entries deserve their slots, and that is the product thesis
   working rather than failing.
5. **Scope.** The gate applies wherever memory competes for shared slots — in
   practice, the default unified search. `--only-text` needs no gate: its memory
   half is the phrase matcher, which already returns nothing (point 2), so the
   half-the-page problem never existed on that path.
   `--only-memory` is **ungated** — with no code corpus in scope
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
   second embed is pure cost. Add that case to ADR-081's elision table. Since
   the lexical path is inert (point 2), such a store contributes nothing to the
   default search at all, and the elision costs no recall.

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
pipeline. It is query-embed, note-embed, distance. **It has been run**; the
numbers below are results, not a plan.

### The two labelled sets

**Negatives:** the 500 CodeSearchNet queries against the 20-entry memory store —
Python docstrings against engineering notes from an unrelated project. 10,000
(query, note) pairs, no annotation effort. **Positives:** 20 hand-written
paraphrase questions, one per entry, each sharing at most one content word with
the entry it targets, so the set measures semantics rather than lexical overlap.
Both sets are committed.

### Result: the space separates

L2 over L2-normalised vectors, QA prefix on the queries:

| | mean | sd |
|---|---|---|
| negatives (n = 10,000) | 1.4026 | 0.0457 |
| positives (n = 20) | 1.0802 | 0.0707 |

The nominal ranges overlap, but the **mass overlap is 0.13% — 13 pairs of
10,000** — and the negative median sits 4.6 positive standard deviations above
the positive median. **An absolute floor works in this embedding space.** This
was the falsification test the design had to survive, and it survived it.

### The threshold is a two-sided fit, not a percentile

The natural-looking move — put the threshold at a low percentile of the negative
distribution — **fails this ADR's own criterion A**, and the record states that
plainly so nobody re-proposes it:

| threshold | negative pairs admitted | mean memory in top 10 | vs criterion A (≤ 0.05) |
|---|---|---|---|
| negative p5 | far more | — | fails badly |
| negative p1 | 100 / 10,000 | 0.20 | **fails, 4×** |
| **T = 1.2032** | 11 / 10,000 | **0.0200** | passes |

Criterion A caps admissions at roughly 25 of 10,000 — the **p0.25** percentile.
The hoped-for outcome that "p0.1, p1 and p5 all give the same result" is simply
not met, so the parameter is not chosen off one distribution at all.

**The usable band is determined by criteria A and C jointly**: A bounds it from
above (admit too much and memory reappears on irrelevant queries), C bounds it
from below (admit too little and memory stops being reachable on the queries it
answers). The measured band is

> **L2 ∈ [1.1822, 1.2337]** — roughly 6 points of cosine —
> with **`MEMORY_MAX_QA_DISTANCE` = 1.2032**, its midpoint.

This two-sidedness is the point, and worth naming: **a constant pinned between
two opposing acceptance criteria is not an untestable constant.** It cannot be
moved in either direction without a named test failing, which is exactly the
property ADR-081 demands and which a per-corpus weight cannot have — the
code-retrieval benchmark bounds a weight from one side only, so a weight has an
edge but no band.

### The negative set is contaminated in its tail, and the bias is conservative

The "10,000 label-free true negatives" framing is right in aggregate and
**wrong exactly where the threshold is drawn.** Of the 11 pairs admitted at T,
most are *genuinely relevant* retrievals: a docstring reading "Execute gerrit
command with retry if it fails" matched against the retry-policy note; another
about truncating to the correct timezone matched against the timezone note. At
the band's upper edge (1.2337) only 1 of 24 admissions is an unrelated
household note.

Two consequences to carry forward:

1. **The threshold is biased conservatively low.** The calibration scores
   correct retrievals as false positives, so the gate is better than its own
   numbers say, and criterion A is being met with room to spare rather than
   exactly.
2. **A later re-run must not read a rising admission count as regression.** It
   may be recall. Anyone re-running this must inspect the admitted pairs, not
   only count them.

### Resolution

Every step is a deterministic embed and a distance, so the retrieval harness's
run-to-run noise does not constrain this choice at all. That is not a happy
accident; see *On the harness*.

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
so a weight means tuning two coupled constants against an instrument whose
same-condition Recall@10 spread is 0.0180.

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
   fails. Those two bounds are measured, not asserted — they are what pins the
   constant to [1.1822, 1.2337]. A wrong cross-corpus score merge — the thing
   ADR-081 forbids — fails *silently*, which is the entire reason for the
   prohibition. Different failure mode, different treatment.
3. **Its calibration is deterministic and does not ride the noisy instrument.**
4. **It is one constant, in one place, with its provenance in its doc comment.**

The residual assumption, stated plainly: after gating, a surviving memory rank 1
is still interleaved at parity with code rank 1. That is the same parity
assumption ADR-081 made; this ADR makes it *conditional* rather than removing it.
If parity turns out to be wrong for entries that clear the bar, that is a
separate finding and needs the joint benchmark described above.

### What the dead lexical path means

The memory FTS half matches through `fts5_quote_literal` — the whole query as
one contiguous phrase — whereas the **code** FTS half uses `fts5_match_query`,
which ORs the query's terms. The calibration measured what that asymmetry
costs: **0 of 500** negative queries and **0 of 20** positive ones produce any
memory FTS match at all. The memory hybrid's BM25 half is not merely weak for
multi-word natural-language questions; it is inert. That is why the memory list
observed in PR #44 is effectively pure KNN, and why this ADR's gate is
vector-only.

Two things follow that are not this ADR's to fix but must be on the record:

- **ADR-081 §*Partial-index semantics* (5) is fictional in practice.** Its claim
  that an unembedded note "stays reachable" through `memory_fts` is true of the
  code path and false of the outcome: for realistic queries that path returns
  nothing. Memory coverage is effectively binary on embedding, unlike the code
  corpus, whose FTS half genuinely does degrade continuously.
- **`--only-text` already returns zero memory results**, so the half-the-page
  problem never existed on that path.

**Binding constraint on whoever fixes the matcher, now stronger than an
estimate.** Switching the memory side to `fts5_match_query` admits **7,146 of
10,000** negative pairs — a mean of **14.3 notes per query**, i.e. effectively
the entire store on every query. A lexical door opened onto that is not a door,
it is the removed wall. The change that loosens the matcher must, in the same
change, gate the lexical path (a BM25 floor or a minimum term-coverage
requirement) and re-run acceptance criterion A. Neither the matcher change nor
its gate is in scope here.

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
- **Entirely irrelevant store, the motivating case.** Every candidate fails the
  floor, memory contributes nothing, and the fused list is the code list.
  Measured on the 500-query benchmark: mean memory in the top 10 falls from
  5.0000 to 0.0200.
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

A reviewer can check every one of these. **A and C jointly determine the
constant** (see *The threshold is a two-sided fit*) — they are not only pass/fail
gates, they are the calibration. A, C, D and E are **exactly reproducible**: the
memory pipeline is KNN + FTS with no PageRank stage, so it carries none of the
non-determinism that affects code recall. Only B touches the noisy instrument.

Measured results at `MEMORY_MAX_QA_DISTANCE = 1.2032`, 3 paired repeats, are
given alongside each criterion.

**A. Suppression (primary gate, noise-free).** Same 500 queries, same
fully-embedded 575-chunk index, same 20 unrelated entries. Mean memory results
in the top 10 of the unified default: **≤ 0.05** (baseline 5.0), identical
across 3 runs. **Measured: 5.0000 → 0.0200, byte-identical across all three
runs. Passes.**

**B. Code recall restored (bounded by the noise floor).** Over **≥ 3 paired
runs**, comparing medians of unified default against `--only-code`:

| metric | required | pre-gate gap | measured |
|---|---|---|---|
| Recall@10 | \|Δ\| ≤ 0.020 | 0.294 | 0.6460 vs 0.6520 → 0.006 |
| Recall@5 | \|Δ\| ≤ 0.020 | 0.172 | passes |
| MRR@10 | \|Δ\| ≤ 0.010 | 0.074 | 0.1928 vs 0.1927 → 0.0001 |

**Read the limit of this criterion, not only its result.** The same-condition
spread is now measured properly at n = 120: Recall@10 **0.0180**, MRR@10
**0.0044**. The MRR threshold at 0.010 is safely outside its band. The recall
threshold at 0.020 is **not** — it can distinguish "no regression" from "the
0.294 regression" and nothing finer, so **criterion B cannot certify the absence
of a regression smaller than about 0.015.** More repeats do not fix this: most
of the spread is non-deterministic tie-breaking rather than sampling, so it does
not average down. A passing B is evidence that the 45% cliff is gone, not proof
that nothing was lost. That limit is tracked as its own defect.

**C. Memory still reachable (the counter-gate — without this, deleting the
memory corpus passes A and B).** With the committed positive fixture of ≥ 20
(question, entry) pairs loaded alongside a code index, the unified default
places the target entry in the **top 10 for ≥ 90%** of the questions and in the
**top 3 for ≥ 70%**. **Passes**, and it is this criterion that sets the band's
lower bound at L2 1.1822.

**D. Empty-store invariance.** With zero memory entries, unified default output
is byte-identical to `--only-code` for the same query, and issues exactly one
embed — asserted on request count against a mock, as
`corpus_filters_elide_the_redundant_query_embed` already does.

**E. Tiny-store invariance.** With a single entry that fails the floor, output
is byte-identical to the empty-store case.

**F. Calibration is reproducible.** The calibration script and both labelled
sets are committed. Re-running reproduces the negative and positive
distributions, the [1.1822, 1.2337] band, and the same
`MEMORY_MAX_QA_DISTANCE`. Per *the negative set is contaminated in its tail*, a
re-run must inspect the admitted pairs rather than only counting them.

**G. The cross-corpus invariant is intact.** ADR-081's existing guards still
pass unmodified — in particular `orders_by_rank_not_by_incomparable_distance`
and `fused_order_interleaves_rank_positions_not_raw_distances`. The gate is
upstream of `fuse()`; if a change to `fuse()` is required to implement this,
that is a signal the gate has been put in the wrong place.

## On the harness

**The current harness is sufficient for this decision, and its known
non-determinism is not a prerequisite.** Stated plainly because the opposite
would make the PageRank fix a blocker:

- The threshold is calibrated on embeddings and distances, not on recall@k, so
  the harness's noise does not constrain it at all.
- The primary gate (A) and the counter-gate (C) — which between them *determine*
  the constant — both come from a pipeline with no PageRank stage and are exact.
  A was byte-identical across three runs.
- Only B rides the noisy path, and the failure it had to detect was 0.294
  against a 0.0180 spread — a factor of ~16.

The two asks made of the harness in the draft of this record **have been met**:
repeats are now paired and reported, and the noise floor is measured at n = 120
rather than resting on n = 2. The numbers are Recall@10 spread **0.0180** and
MRR@10 **0.0044**, replacing the earlier anecdotal ±1.4pt.

**What remains, and is a real limit rather than a wish:** criterion B's recall
threshold (0.020) sits barely above that 0.0180 spread, so the harness cannot
resolve a regression smaller than ~0.015, and repeats do not help because the
spread is non-deterministic tie-breaking rather than sampling. Certifying a
*small* retrieval regression needs the underlying non-determinism fixed. That is
not required for this decision, whose effect size is an order of magnitude
larger, and it is tracked as its own defect.

The root cause — `HashMap`-ordered edge-list construction and f32 accumulation
in the PageRank stage of `search/rag.rs` — is out of scope here.

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

Docs-only decision; not yet implemented in the product. **The falsification test
this record set itself has been run and passed** — a spike measured the two
distributions, found 0.13% mass overlap, fitted the threshold against criteria A
and C, and confirmed all three acceptance criteria on 3 paired repeats. The
numbers throughout this record are that spike's.

Implementation is therefore gated only on sign-off of this record. The spike's
calibration script and both labelled sets ship with the implementation (criterion
F), and its recommended gate placement — on `MemoryStore::search()`'s output,
upstream of `search_hybrid`'s `1 / rrf_score` overwrite — is decision 1.
