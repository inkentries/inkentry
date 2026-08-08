# ADR-080: Structural chunk summaries and a PageRank-ordered tiered embed queue, with one in-place re-embed migration

**Date:** 2026-08-08
**Deciders:** founder (Johan); architect
**Relationship to prior ADRs:** extends
[ADR-070](070-init-embed-lifecycle-and-search-warmup-contract.md)'s search
warmup contract — it added a coverage signal (what semantic search can see at
all); this record adds a second, orthogonal signal (whether a chunk's vector
still reflects its current input) and keeps the "a surface that cannot fully
answer says so" posture. It completes the extractive-summary half that
[ADR-079](079-deprecate-explore-command-and-route.md) named but did not build:
ADR-079 declared harvest the sole LLM-backed feature "combined with the move to
extractive chunk summaries," and this decision is that move. It respects the
embedding split of ADR-070 (the server computes vectors, the CLI is the only
persistent index store) and does not reopen it.

## Context

Three defects in the indexing pipeline share one root and one fix, which is why
they are decided together rather than as three records.

**The `summary:` slot is key-gated, non-deterministic, and mostly never
embedded.** `Chunk::embedding_text()` composes the embedding input as
`title: {name} | summary: {summary} | text: {docstring}\n{content}`. The
`summary:` slot exists to bridge retrieval vocabulary — natural-language words a
query would use, sitting next to the code they point at. Today it is filled by
an LLM (`summariser::summarise_batch`), which means three things at once. It is
gated on a configured model and a key: with no LLM there is no `summary:` slot
and retrieval is measurably poorer, on the exact offline/no-key setup the
built-in tier exists to serve. It is non-deterministic: the same chunk yields
different prose across runs, which breaks both the public "same query, same
answer" property and the idempotent-resume guarantee an interrupted index
depends on. And, per the ordering defect below, the LLM summary it does produce
usually never reaches a vector at all.

**Two ordering defects: summaries and PageRank both run *after* the embed they
are supposed to inform.** LLM summarisation runs in a late index phase, after
the embed phase. So a chunk is embedded first with `summary = None`, and the
summary is written afterward; it is folded into a vector only if that chunk
happens to be re-embedded later. On the common path the stored summary never
influences retrieval. PageRank has the identical shape: it is computed in a
phase after embed, so on a cold first index every `graph_rank` is 0 and the
embed queue — ordered `graph_rank DESC, mtime DESC, id` — collapses to
`mtime DESC, id`. The queue is only PageRank-ordered on a *warm* re-index. The
"central code gets embedded first" promise is therefore false for precisely the
case it exists to serve: the first hour on a new machine, where a large repo may
take roughly 90 minutes on Metal and several hours on CPU to embed, and where
what gets embedded first decides what search can answer during that window.

**Title-less chunks embed with weak vocabulary.** Markdown `Section`s and
oversized sliding-window (`Verbatim`) chunks carry no symbol name, so they embed
as `title: none` and a structural summary (below) has little to work from — the
signals it draws on are code-shaped.

Two facts constrain the fix. First, the embedding input is bounded at the wrong
place: `MAX_CHUNK_TOKENS` (512) caps the *chunk*, not the composed
`title | summary | text` string, and the embedder truncates the composed input
at a memory-derived window (~5.8k tokens at a 2 GiB budget, ~8.2k above,
clamped to `MAX_SEQ_LEN`). A summary that grows without a cap displaces the code
tail it was meant to make findable, at the embedder boundary, silently — and
even well short of truncation, a bloated summary dilutes the pooled vector
toward prose and away from code. Second, any change to what text gets embedded
invalidates every existing vector, and the codebase already has a precedent for
that: the FLOAT[768]→INT8[896] upgrade (`db.rs::apply_dim_upgrade_migration`,
marker-gated by `schema_int8_embeddings`) *dropped* the vector table and left
semantic search returning nothing until a full re-index completed. That was
acceptable when the vector *space* itself changed. It is not acceptable here,
where only the input text changes and the whole point is to keep search useful
while the change rolls through.

Finally, the queue machinery this builds on already exists and must be extended,
not rebuilt: `chunks_missing_embeddings` orders the queue by `graph_rank`, a
detached worker resumes from durable DB state with per-batch atomic commit and
hard-exit crash tests, and `search`/`status` already carry a coverage/warmup
contract. What does not exist is any way to re-embed an already-embedded chunk
(the queue only returns rows with no vector), any centroid/MMR selection, and
any signal distinguishing "has a vector" from "has a *current* vector."

## Decision

Four coupled changes, joined by one migration.

**1. Structural extractive summaries replace LLM summaries, in the built-in
tier.** The `summary:` slot is composed deterministically from signals already
present after parse — no model, no key, no network — and the LLM summary path is
retired. Ingredients are assembled in a fixed priority order and appended until
a hard cap is reached:

1. **Docstring, first sentence** — human-written intent, highest signal.
2. **Split symbol name** (`retry_with_backoff` → "retry with backoff") — bridges
   the identifier already in `title:` to natural language, at tiny cost.
3. **Split callee names** — from the code graph's `Calls` edges, emitted in the
   graph's existing deterministic SQL order.
4. **Salient literals** — error/log string literals in the chunk; last, being
   the noisiest and the most likely to carry a secret.

The composed slot is bounded by a named constant, `SUMMARY_TOKEN_CAP`, sized to
the one-sentence LLM summary it replaces (of order ~96 tokens — the same order
of magnitude as one sentence, never several times it). The cap addresses both
failure modes named in Context: the acute one (displacing the code tail at the
embedder boundary) and the chronic one (diluting the pooled vector). When
ingredients would overflow the cap, the lowest-priority ingredient that does not
fit is dropped **whole** — never truncated mid-ingredient — as are all
lower-priority ones. The load-bearing invariant, which a test must pin: for the
largest chunk with a maximum-size summary, the entire `content` still appears
verbatim in `embedding_text()`, and if anything must yield to fit the embedder
window it is the summary slot, never the code text. Silent truncation of code is
not acceptable.

The summary must be **byte-identical** for the same chunk and edges on every
run. This is what underwrites idempotent resume (an interrupted re-embed must
not produce a different vector for the same input) and the public "same query,
same answer." The known hazard is iteration order over a hashed collection of
callees; callees are emitted in the graph's fixed order, never in hash-iteration
order, and a test shuffles insertion order and asserts identical output.

Because the salient-literals ingredient is a new exposure — a literal can carry
a credential the chunk body did not — the **composed** summary is scanned with
`contains_secret` before storage; on a hit the slot is stored as `""` (composed
but suppressed, so it is not recomputed on a plain re-index) with a symbol-only
warning. This is best-effort defense-in-depth, consistent with the existing
secret scanner, not a boundary.

The pass runs **after parse, before the first embed** — it needs only stored
chunks and graph edges, both present once parse completes. On a fresh index this
means a chunk's first and only embedding already includes its summary: **no
re-embed on the fresh path.** Abstractive summaries are no longer produced by
inkentry; a user who wants prose summaries runs their own agent over
`plumbing cat-chunks` output, shipped as a skill. This keeps reasoning with the
caller's model rather than on our infrastructure, consistent with the boundary
ADR-079 drew.

**2. PageRank moves before the embed phase.** It runs after parse and before
embed, so the first embed is genuinely PageRank-central on a cold index. The
cost is negligible in context — `O(iterations × |edges|)`, seconds at repo
scale, against embed times measured in tens of minutes to hours — and there is
no unordered window, because embed never starts until parse and PageRank have
both completed. A repo with no edges yields an empty ranking and falls back to
`mtime DESC, id`, as today, with no error.

**3. The embed queue gains priority tiers, with an MMR refinement tier that
reuses the stored primary vector.** Tiers are priority bands within the one
existing queue, not separate passes:

- **Tier 1** — primary vectors for PageRank-central named chunks (now honest on
  the cold path), each carrying its structural summary.
- **Tier 2** — the remaining never-embedded chunks, same ordering keys.
- **Tier 3** — MMR selection and in-place re-embed for title-less chunks,
  drained last. For each title-less chunk, split it into units (sentences for
  Markdown, statement groups for windowed code), embed the *units*, and select a
  representative subset by greedy maximal-marginal-relevance; the selected units
  become its `summary:` slot. The centroid is the chunk's **already-stored
  primary vector**, not a fresh whole-chunk embed. This is the key cost decision:
  reusing the stored vector means tier-3 never runs a whole-chunk forward pass
  and so never enters the single-chunk seq² path that OOMs on CPU; only short
  units get new embeds, and they batch cheaply. `λ` is a fixed, documented
  constant recorded in the scheme provenance (not a config surface); MMR ties
  break by unit index, so selection is byte-identical for the same units and
  `λ`.

Never-embedded chunks always sort ahead of pending re-embeds (a leading
`(vector present) ASC` key), so coverage is bought before refinement.

**4. One shared in-place re-embed migration.** Both summary changes alter what
text is embedded, so both invalidate existing vectors; the migration is designed
once and covers both. It adds one column and one marker:

- `chunks.embed_pending INTEGER NOT NULL DEFAULT 0` — the stored vector no
  longer reflects the chunk's current `embedding_text()` and must be re-embedded.
- `index_meta['summary_scheme']` — a small version string for the composition
  scheme (e.g. `structural_v1`), stamped alongside `embedding_model` and
  `chunker_config`.

It is a new numbered step in `Database::open`'s migration runner — the same
runner that carries the FLOAT→INT8 upgrade — backed by
`migrations/026_embed_pending.sql`, gated on the `summary_scheme` marker so it
never re-runs on an already-migrated database. It **deliberately departs from
the FLOAT→INT8 precedent in one way**: that migration dropped the vector table,
so search went dark until re-index; this one keeps every existing vector and
re-embeds each chunk in place (delete-then-insert per chunk, in one batch
transaction), so a chunk always has a valid vector and coverage never regresses
to zero. On an existing index the migration marks every chunk `embed_pending`
exactly once, on the `summary_scheme` transition; the next `inkentry index`
drains them in PageRank order. On a fresh index nobody sets the flag. Tier-3
sets it per title-less chunk after writing the MMR slot. The `embed_pending`
clear happens in the **same transaction** as the vector write, so a kill
mid-batch rolls back both and the batch re-queues cleanly — the existing
crash-safety guarantee extends unchanged.

Because a re-embedded chunk keeps its old vector until the new one lands,
coverage (any vector present / total) reads 100% while re-embeds are still
pending. So the migration adds a second, distinct signal — **freshness**, the
count of chunks whose vector reflects the current input (`embed_pending = 0`) —
surfaced as `embedding_refresh_pending` in `status` JSON and named in the
`search` notice when refinement is pending. Coverage answers "what can search
see"; freshness answers "does what it sees reflect the current input." "Same
query, same answer" is guaranteed once freshness reaches 100%; while draining,
rankings may shift and the surface says so. Durable queue position is DB state
(`embeddings` rows plus `embed_pending`), not an in-memory cursor, so a
kill/reboot/OOM resumes by rebuilding the queue from the database and redoing
only chunks that lack a current vector.

## What breaks

- **Every existing index needs one re-embed.** Because the embedding input text
  changes, existing vectors predate the new scheme. The migration marks them
  pending and the next index drains them in place; existing users pay this once.
- **Abstractive summaries are no longer produced by inkentry.** The LLM summary
  path, its `--summary-batch-size` flag, and the `LlmFeature::Summaries`
  capability (with its no-LLM message arm) are removed. The replacement is the
  shipped skill over `plumbing cat-chunks`.
- **Coverage at 100% no longer implies freshness at 100%.** Any consumer of the
  coverage signal that assumed "fully embedded" meant "stable rankings" must now
  also read `embedding_refresh_pending`. This is a documented `status` shape
  addition, not a change to existing fields.
- **An old binary opening a new-scheme database** finds an unknown
  `summary_scheme` stamp and an extra column it ignores. It warns and continues
  — the same policy as `chunker_config` drift, deliberately *not* the hard error
  `embedding_model` drift raises, because the vector space (model and dimension)
  is unchanged; only the input composition differs, which is same-space drift.
- **Re-index recomputes PageRank before draining.** Seconds of added work at the
  head of every index, by construction, so that re-embed order is also
  central-first.

## Alternatives considered

- **Keep LLM summaries and only fix the ordering** (summarise before embed).
  Rejected: it leaves the slot key-gated, so the built-in/offline tier still has
  no summary and worse retrieval, and it leaves summaries non-deterministic, so
  "same query, same answer" and idempotent resume still do not hold. Fixing
  ordering alone treats the least important of the three problems.
- **Drop summaries entirely.** Rejected: the slot measurably bridges retrieval
  vocabulary; removing it makes retrieval worse to avoid building a deterministic
  producer that the parse output already affords for free.
- **Reuse the FLOAT→INT8 drop-and-recreate migration.** Rejected, and this is
  the crux of the migration design: dropping the vector table sends semantic
  search dark for the full re-embed — 90 minutes to several hours on a real
  repo. This work exists to keep search useful *while* the change rolls through,
  so it keeps vectors and re-embeds in place. Drop-and-recreate stays the right
  shape only when the vector space itself changes (see the boundary test).
- **Leave PageRank in its post-embed phase.** Rejected: it makes the
  "central-first" ordering true only on warm re-index and false on the cold
  first index — the one case that matters — for no saving worth the lie.
- **Re-embed the whole chunk to get the MMR centroid.** Rejected: it adds a
  long-sequence forward pass per title-less chunk and re-enters the single-chunk
  seq² path that OOMs on CPU, for a centroid the stored primary vector already
  is. Reusing the stored vector is strictly cheaper and safer.
- **Run MMR selection as a separate post-index pass.** Rejected: it would
  duplicate the resumability, liveness, and atomic-commit machinery the embed
  queue already has. A priority tier in the one queue inherits all of it.
- **Two migrations / two scheme bumps** (one per summary change). Rejected: it
  would make existing users re-embed the whole repo twice. Both changes land
  under one `summary_scheme` bump, so it is one full re-embed plus a small
  tier-3 refinement of only the title-less subset.

## Consequences

- **Implementation is gated on two prerequisites, both hard.** (a) This ADR
  clears its human sign-off. (b) A retrieval baseline (recall@k / MRR) is
  captured in the public `inkentry-bench` harness **before any vector changes**,
  and re-run after, so the effect on retrieval quality is measured rather than
  assumed. The baseline is captured once and shared across both summary changes,
  since it must be taken before the single `summary_scheme` flip. As of this
  decision the baseline has not yet been captured — the bench machine is
  occupied by a running evaluation — so implementation does not begin until it
  frees and the baseline is recorded. This is a sequencing prerequisite, not a
  reason to defer the design.
- **Existing users do one full-repo re-embed, in PageRank order, in place**,
  plus a small tier-3 refinement of the title-less subset. Coverage stays at
  100% throughout; the warmup contract for first-embed is untouched.
- **The cold-index promise becomes true.** The first chunks embedded on a new
  machine are the PageRank-central ones, carrying their summaries — the highest-
  value part of this change, closing a real gap rather than adding a tier.
- **Added CPU cost is bounded and drained last.** Tier-3 touches only the
  title-less subset and embeds short units against a reused centroid; the
  estimated overhead is on the order of ~5–6% of the primary pass, scheduled
  after all first-embed work so it never delays first-hour value. The real
  number is to be confirmed from the `inkentry-bench` performance scripts.
- **Rollback does not revert vectors** — reverting code cannot un-embed. The
  additive column and marker are forward-compatible (an old binary warns, does
  not error). To revert vectors to the pre-summary composition, run
  `inkentry index --force` on the reverted binary. To abort mid-migration, stop
  the worker: the index is a valid mixed-scheme index that search already
  handles via the coverage and freshness signals; re-running `inkentry index`
  resumes from DB state, and determinism guarantees no different vector for the
  same input.
- **After this lands, `memory harvest` is the only feature that calls an LLM**,
  completing the consolidation ADR-079 anticipated.
- **Threat surface.** The changes read and write only the local database and add
  no egress; the composed and MMR-selected summaries pass `contains_secret`
  before storage; new writes are parameterised. Determinism is treated as
  security-adjacent because it underwrites idempotent resume and "same query,
  same answer."

## Boundary test

Two rules generalise from this decision.

**A change to what text is embedded is an in-place, flag-driven re-embed that
preserves coverage — never a drop-and-recreate.** Drop-and-recreate, and the
`embedding_model` hard error, are reserved for a change to the vector *space*
itself (model or dimension), where old and new vectors are not comparable and a
mixed index would be meaningless. A same-space input change (summary
composition, chunking text) keeps every vector until its replacement lands,
signals staleness with a freshness count distinct from coverage, and resumes
from durable DB state.

**A feature that only bridges retrieval vocabulary belongs in the built-in tier
— deterministic, offline, no key.** Reasoning stays with the caller's agent
(the skill path). An LLM earns a place in the pipeline only when it writes
durable memory, which is why harvest keeps its model and summaries do not.
