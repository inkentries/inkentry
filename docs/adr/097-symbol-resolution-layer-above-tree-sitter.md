# ADR-097: A symbol-resolution layer above tree-sitter — `locals.scm` intra-file resolution, and a resolved-target column on the code graph

**Date:** 2026-09-15
**Deciders:** Founder (Johan); Architect
**Relationship to prior ADRs:** the code graph this record resolves is the same
one that
[ADR-080](080-structural-summaries-pagerank-tiered-embed-queue-in-place-reembed.md)
ranks with PageRank and that `search --graph` reads; PageRank is the primary
in-tree beneficiary of better identity. It does not touch memory identity
([ADR-078](078-uuidv7-memory-entry-identity.md),
[ADR-093](093-entity-id-is-the-portable-handle-for-memory-entries.md)) — this is
about *code-symbol* identity, a different axis. It adds one nullable column to
`graph_edges` (the table [ADR-080](080-structural-summaries-pagerank-tiered-embed-queue-in-place-reembed.md)'s
PageRank and the LinearRAG mentions graph both read). The column arrives by a
forward migration of `index.db`, not by a rebuild (section 1): a rebuild
discards the embeddings, and re-embedding a large repository is hours of work
that every team member would repeat.

## Context

inkentry's code graph is **lexical**. `EdgeExtractor`
(`crates/inkentry-core/src/indexer/graph/`) walks each file's tree-sitter tree
and, for every call site, emits `graph_edges(source_file, source_name,
target_name, kind, line)` where `target_name` is the **bare callee token**: a
call to `Foo::new(...)`, `obj.method(...)`, or `self.run()` is stored as `new`,
`method`, `run`. No scope, no receiver, no defining file is recorded. Every
consumer then joins `target_name = chunks.name`:

- PageRank over `graph_edges_all()` (`storage/graph.rs`) — feeds embed ordering
  and ranking (ADR-080).
- `search --graph` via `edges_for_symbol()`, and `graph_neighbor_chunks()`.
- The LinearRAG mentions graph (`chunks_mentioning_symbols`).

Because the join is on a bare name, an edge to `new` points at **every** chunk
named `new` in the repo. The identity of a call is "which definition does this
token refer to?", and the index cannot answer it.

The deductive-engine POC (standalone crate `code/inkentry-v2-poc`) quantified
what that costs. On lago's 22,959 intra-repo call edges, 12,908 are **multi-def**
(callee name defined in >1 file) — resolvable *without types* by scope. A crude
locality heuristic (same-file → same-dir → same-package) resolves **32%** of
them; rebuilding the module graph with those resolved edges swings the
coupling/cycle answer **2.2×** (5,554 → 7,232 module edges; 67 → 150 cycles).
Every derived property — impact, coupling, cycles, and PageRank centrality — is
only as trustworthy as this resolution. **Correctness is gated on identity, not
on the engine.** (See handbook `v2-journey/poc-results.md`.)

The intra-file tier has one dependency constraint, and the obvious cross-file
library is now foreclosed:

- **The grammars come from `ast-grep-language` 0.45 on tree-sitter 0.26.13**
  (`ts_walker::ts_language`). That crate exposes each grammar's
  `tree_sitter::Language`, but **ships no `locals.scm`/query files** and no scope
  analysis (the `ast-grep-core` structural engine was removed — see CLAUDE.md,
  "Dependency Notes"). So the intra-file tier vendors the query files; it needs
  no new runtime.
- **`tree-sitter-stack-graphs` is foreclosed.** It was the obvious hermetic,
  no-build cross-file resolver, but GitHub **archived the `github/stack-graphs`
  monorepo on 2025-09-09**. Its releases are terminal at `tree-sitter ^0.24`
  with exact old grammars (`-python =0.23.5`, `-typescript =0.23.2`,
  `-javascript =0.23.1`) — a semver-incompatible line from our 0.26 that will
  never advance, and no Ruby crate ever shipped. Adopting it would mean owning a
  year-dead framework across two tree-sitter runtimes. So **cross-file resolution
  is deferred** (see Deferred), and this ADR commits v1.2 to the intra-file tier.

The offline/deterministic/single-binary promise (enforced by
`crates/inkentry-cli/tests/egress_containment.rs`) also rules out a live language
server as a runtime dependency.

## Decision

Keep tree-sitter as the parser. Add a **resolution layer** above it that binds a
call edge's callee to a **defining file**, and persist that binding. v1.2 ships
the **intra-file** tier; cross-file resolution is deferred (see Deferred).

### 1. `graph_edges` gains a nullable `target_file` column

`target_file TEXT` is the repo-relative path of the file that defines the callee
this edge resolved to.

- **`NULL` means unresolved** — the resolution layer found no unique binding.
  This preserves today's behaviour exactly: consumers fall back to the bare-name
  join. Resolution is therefore strictly *additive*; nothing regresses for edges
  we cannot yet resolve.
- **Non-`NULL` means a resolved binding.** A consumer disambiguates by joining
  `chunks.name = target_name` **and** the chunk's file `= target_file`, hitting
  the specific definition instead of every same-named chunk.
- **Cardinality.** One lexical call site produces one row today. Resolution may
  (a) keep it one row with a filled `target_file` (the common case), or (b) split
  it into several rows — same `target_name`, different `target_file` — when a
  resolver legitimately binds a reference to more than one definition (re-exports,
  conditional definitions). The per-file dedup key in `replace_edges` therefore
  extends to include `target_file`. Unresolved stays one row, `target_file` NULL.

**The column is migrated in, not rebuilt in.** Today `Database::open`
(`crates/inkentry-core/src/storage/db.rs`) treats any `user_version` below
`CURRENT_SCHEMA_VERSION` as an index it cannot read: it deletes the file,
recreates it empty and asks for `inkentry index`. That is affordable for shapes
from before the 1.0 reset and is kept for them (stamps at or below
`LAST_LEGACY_SCHEMA_VERSION`). It is not affordable here. Everything in
`index.db` can be rederived from source, but the embeddings are by far the
expensive part to rederive: on a large repository a rebuild is hours of
embedding, paid again by every team member, to add one nullable column that
touches neither chunks nor vectors.

So this change is a forward step on `index.db`, applied in place on open, using
the same stamped, one-transaction-per-step pattern `memory.db` uses:

> **Corrected (2026-09-23):** the two bullets below describe a fresh index
> as built directly from `index_001_initial.sql` at the current version, with
> that file updated on each shape change. That is not the model. Both initial
> schema files are **frozen** at the shape 1.0 and 1.1 shipped (`index.db` at
> 17, `memory.db` at 11): the ladder below them was collapsed into them once,
> at the rename, and is not collapsed again. A fresh index is created from the
> frozen file at 17 and climbs the `INDEX_MIGRATIONS` registry to the current
> version, the same road an existing index takes. So `target_file` lands as
> **one** thing, a registered step, and `index_001_initial.sql` is not touched.

- The shape change is **a new ladder step plus an update to the current-shape
  file**, the way `memory.db` evolves. `index_001_initial.sql` is the schema a
  *fresh* store is built from directly, so it gains the column and
  `CURRENT_SCHEMA_VERSION` bumps (17 → 18); the step (next bullet) replays the
  same `ALTER TABLE graph_edges ADD COLUMN target_file TEXT` (plus the index
  backing the extended dedup key) onto an *existing* store. Existing rows get
  `NULL`, which already means "unresolved, use the bare-name fallback", so a
  migrated index is correct before any resolution runs. The append-only,
  never-renumbered part is the **ladder-step registry**, not the fresh-shape
  file.
- It **reuses the store-agnostic forward ladder** `storage::migration_ladder::apply_ladder`
  (the ladder `memory.db` already drives via `memory::migrate`): a
  `&[(version, MigrationStep)]` registry whose steps each commit atomically with
  their own `user_version` stamp under `BEGIN IMMEDIATE`, so a crash leaves the
  store at the last completed version and the next open resumes. `index.db` gets
  its own `INDEX_MIGRATIONS` registry — the runner was written to be reused with a
  second store's own constants — whose first entry is the `target_file` step,
  applied to any stamp above `LAST_LEGACY_SCHEMA_VERSION` (16). A **fresh** index
  is built straight from `index_001_initial.sql` at the current version, with no
  ladder replay; an **existing** index at 17 runs only the new step in place,
  keeping its embeddings — neither rebuilds. A contiguity check and a parity test
  (a pinned pre-column fixture vs a fresh store) hold the two paths to an
  identical schema, mirroring `memory.db`'s own migration discipline.
- Filling `target_file` for existing edges is a **graph-only pass**: re-parse
  and re-extract edges with tree-sitter, write `graph_edges`, touch nothing
  else. No chunking, no embedding. The step records that the pass is owed in
  `index_meta`, and the next `inkentry index` runs it for every file, not just
  changed ones, then clears the marker. Until then consumers see `NULL` and
  behave as they do today.

Rebuild stays as the last resort for a change that genuinely invalidates the
stored data (a different embedding space, a chunking change that makes existing
chunk rows wrong). A step says so explicitly; it is not the default consequence
of a version bump.

`inkentry plumbing graph-edges` JSONL gains an **optional** `target_file` field,
present only when resolved. This is an additive field, not a shape change.

### 2. `locals.scm` intra-file resolution (the v1.2 deliverable)

tree-sitter's own scope queries (`@local.scope`, `@local.definition`,
`@local.reference`) resolve name bindings *within a file*: which definition a
reference denotes in its lexical scope, whether a callee token is a locally-bound
name (parameter, local variable, imported alias) or a free name, and what an
alias re-exports.

- Runs on **inkentry's existing tree-sitter 0.26 tree** — the same parse
  `EdgeExtractor` already produces. **No new runtime, no re-parse.**
- The query files are **vendored** into the repo (per-language `locals.scm`,
  small S-expression files from the upstream grammar repos under their
  MIT/Apache licences), because `ast-grep-language` does not carry them. Vendoring
  keeps the build hermetic and pins the queries to our grammar versions.
- Broadly available across our tree-sitter languages (Rust, Python, JS/TS, Go,
  Ruby, …). Cheap: one extra query pass per file.

This makes the POC's largest locality bucket — same-file bindings (1,952 of the
32% on lago) — **principled** rather than a "a def with this name happens to live
in this file" guess, extends it to lexical scopes, and improves *precision* by
suppressing edges whose callee is a local binding rather than a repo definition.

### 3. Never: live LSP

A live language server is **never** a runtime dependency. It needs a buildable,
stateful, non-hermetic project; it breaks the offline / deterministic /
single-binary promise enforced by `egress_containment.rs`. This is a permanent
boundary, not a scoping choice.

### 4. Deferred (recorded so it is not re-litigated)

- **Cross-file resolution** — the POC's 68% residual — is out of v1.2 and gets
  its own shaping task when picked up. Two maintained directions are on the
  table: resolution over our **own import graph** (`EdgeExtractor` already emits
  `kind = 'imports'` edges with module specifiers; combined with `chunks` this
  needs no new dependency), and **SCIP ingest** as an offline enrichment. Ruby's
  cross-file binding is `autoload`/constant-and-convention driven and effectively
  type-bound, so it is scoped modestly on whichever path is chosen.
- **The type-dependent residual (Mode 2).** ~26% of lago's intra-repo edges are
  *single-def over-match*: one definition, one common method name (`find`,
  `success?`, `perform_later`) matched at every same-named call; only receiver
  types can split those. The maintained tool is **SCIP ingest** (scip-typescript,
  scip-python, scip-ruby/Sorbet, rust-analyzer SCIP) consumed *offline* — but
  SCIP indexers need the language toolchain and a buildable project, so they are
  not hermetic and can only be an opt-in tier, never core indexing. Noted so the
  ceiling is on the record; a separate future track, not v1.2 work.

### 5. Measurement contract

The POC probe (`code/inkentry-v2-poc`, `src/probe.rs`) is extended so a resolver
is a pluggable backend and it reports resolution % plus module-graph edge and
cycle deltas, on lago (`~/opensource/lago`) and inkentry's own repo. v1.2's bar
is the intra-file contribution: `locals.scm` measurably raises *sound* resolution
and precision over the bare-lexical baseline, reported before/after.

**v1.2 does not target the 32% floor.** That floor is a *cross-file* recall
number; beating it is the deferred cross-file tier's job. Intra-file resolution
is higher-precision and narrower-recall than crude locality's unsound
same-dir/same-package guesses — the intended trade, not a regression. The probe
reports both so the split is legible.

## What breaks

- **`graph_edges` widens** by one nullable column and its dedup key. No consumer
  breaks: the bare-name join still works for `NULL` rows, and the two-column join
  is opt-in per consumer.
- **`plumbing graph-edges` JSONL** carries a new optional field. Additive;
  consumers that ignore unknown fields are unaffected.
- Nothing else: `locals.scm` adds vendored query text and one query pass — no new
  runtime, no new dependency.

## Alternatives considered

- **`tree-sitter-stack-graphs` for cross-file resolution.** Rejected: the
  `github/stack-graphs` monorepo was archived 2025-09-09 and is terminal at
  tree-sitter 0.24 / grammars 0.23.x. Adopting it would mean owning a year-dead
  Rust framework across two runtimes, for only the languages it shipped rules
  for. Our own import graph is the maintained path when cross-file is picked up.
- **Cross-file resolution in v1.2.** Deferred, not rejected: v1.2 lands the
  intra-file tier and the `target_file` substrate, and other in-flight work
  carries the release. Cross-file gets its own shaping (Deferred).
- **SCIP ingest now.** Rejected for v1.2 core: SCIP indexers need the language
  toolchain and a buildable project, breaking the hermetic/single-binary promise.
  It is the right tool for the type residual and the harder cross-file cases, so
  it is the designated later enrichment tier.
- **Reuse ast-grep for scope analysis.** Not available: `ast-grep-core` was
  removed and `ast-grep-language` ships only grammars.
- **A separate `edge_resolution` table.** Rejected: `graph_edges` has no stable
  edge id (it is replace-per-file), so a side table buys nothing over a nullable
  column.

## Consequences

- Resolution is **pure local computation** over files the indexer already reads:
  no new external input, no new trust boundary, no network. `target_file` flows
  as a bind parameter exactly like `target_name`, so no SQL-string-formatting
  surface is introduced. `THREAT-MODEL.md` is unchanged, and
  `egress_containment.rs` continues to pass (this tier adds no outbound path).
- The tier is **deterministic and offline**: the same repo yields the same
  `target_file`, satisfying the same promise the engine relies on.
- The realized value is staged. Populating `target_file` and proving the
  resolution-% lift and cycle/impact trustworthiness (via the probe) is the
  mandatory bar. Feeding `target_file` into PageRank and `search --graph` ranking
  is the **stretch** (the roadmap's "ranking-lift" half of acceptance), scoped as
  follow-on once the substrate is proven. Cross-file recall — the 32%-beat —
  waits on the deferred tier.

## Validation the implementation must produce

- On lago and on inkentry: resolution % and precision character under
  `locals.scm` vs the bare-lexical baseline, with the module-graph edge and
  cycle deltas.
- A determinism check: two indexes of the same tree produce byte-identical
  `target_file` assignments.
- `egress_containment.rs` still traps every outbound connection across
  `init`/`index`/`search` — this tier adds none.
