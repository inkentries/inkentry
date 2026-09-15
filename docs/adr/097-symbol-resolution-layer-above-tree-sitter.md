# ADR-097: A tiered symbol-resolution layer above tree-sitter, and a resolved-target column on the code graph

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
PageRank and the LinearRAG mentions graph both read); `index.db` rebuilds rather
than migrates, so the column arrives on the next `inkentry index` with no ladder.

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

Two constraints shape any fix:

- **The grammars come from `ast-grep-language` 0.45 on tree-sitter 0.26.13**
  (`ts_walker::ts_language`). That crate exposes each grammar's
  `tree_sitter::Language`, but **ships no `locals.scm`/query files** and no scope
  analysis (the `ast-grep-core` structural engine was removed — see CLAUDE.md,
  "Dependency Notes").
- **`tree-sitter-stack-graphs` 0.10 pins `tree-sitter ^0.24`** and its language
  crates pin *exact* old grammars (`tree-sitter-python =0.23.5`,
  `-typescript =0.23.2`, `-javascript =0.23.1`). Those are a different, semver-
  incompatible tree-sitter line from ours; a `Language` or `Tree` from one line
  cannot cross into the other. There is no published Ruby stack-graphs crate.

The offline/deterministic/single-binary promise (enforced by
`crates/inkentry-cli/tests/egress_containment.rs`) rules out a live language
server as a runtime dependency.

## Decision

Keep tree-sitter as the parser. Add a **resolution layer** above it that binds a
call edge's callee to a **defining file**, and persist that binding. Build it in
tiers, cheapest and broadest first.

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

`index.db` declares its final shape in one schema file and rebuilds on version
mismatch (CLAUDE.md, "SQLite + sqlite-vec"); the column is added to
`index_001_initial.sql` with no migration path and no data carried across.

`inkentry plumbing graph-edges` JSONL gains an **optional** `target_file` field,
present only when resolved. This is an additive field, not a shape change.

### 2. Tier 1 — `locals.scm` intra-file resolution (ships first)

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

Tier 1 makes the POC's largest locality bucket — same-file bindings (1,952 of the
32% on lago) — **principled** rather than a "a def with this name happens to live
in this file" guess, extends it to lexical scopes, and improves *precision* by
suppressing edges whose callee is a local binding rather than a repo definition.

### 3. Tier 2 — stack-graphs cross-file resolution (TS/JS/Python), as a sidecar

The 68% of multi-def edges Tier 1 leaves are cross-file: the callee is defined in
another module reached through imports/exports. `stack-graphs` does exactly this
name binding over tree-sitter — hermetic, no build step, no type checker —
strongest where GitHub ships maintained rules: **TypeScript, JavaScript,
Python**.

Because stack-graphs sits on the incompatible tree-sitter 0.24 / 0.23.x line
(Context), it is adopted as a **self-contained resolution sidecar**:

- It owns its own tree-sitter 0.24 runtime and its own pinned grammars, and
  **re-parses** the TS/JS/Python subset it resolves. The two tree-sitter lines
  coexist in the binary and **never exchange `Language`/`Tree`/node values**; the
  sidecar's only output is data — `(reference site → defining file)` bindings —
  which the resolution pass writes into `target_file`.
- The cost is explicit and accepted: a second tree-sitter line in the binary, a
  double-parse of the resolved subset, and two grammar lines to track. It buys
  the cross-file bulk of Mode-1 identity for the three languages where it is
  strongest.

**Ruby gets Tier 1 only** in v1.2: no stack-graphs crate exists for it, and its
remaining gap is dynamic method dispatch — a *type* problem (Mode 2), not a scope
one.

### 4. Out of scope for v1.2 (recorded, not built)

- **Live LSP** — never a runtime dependency. A language server needs a buildable,
  stateful, non-hermetic project; it breaks the offline/deterministic/single-
  binary promise and `egress_containment.rs`. Not now, not later.
- **The type-dependent residual (Mode 2).** ~26% of lago's intra-repo edges are
  *single-def over-match*: one definition, one common method name (`find`,
  `success?`, `perform_later`) matched at every same-named call. Locality and
  stack-graphs cannot split a single definition; only receiver types can. That is
  a later **SCIP-ingest enrichment tier** (scip-typescript, scip-ruby/Sorbet,
  rust-analyzer SCIP consumed *offline*, not a live server). Noted here so the
  ceiling is on the record; it is a separate future track, not v1.2 work.

### 5. Measurement contract

Every child task reports **before/after** on lago (`~/opensource/lago`) and on
inkentry's own repo, against the POC's **32% crude-locality floor**. The POC
probe (`code/inkentry-v2-poc`, `src/probe.rs`) is extended so a resolver is a
pluggable backend and it reports, per tier (crude-locality baseline → `locals.scm`
→ `+ stack-graphs`): multi-def resolution %, and the module-graph edge and
cycle deltas. A tier that does not measurably beat 32% on multi-def resolution
does not ship.

## What breaks

- **Binary grows.** Tier 2 links a second tree-sitter line and three extra
  grammars. Tier 1 adds only vendored query text.
- **Double-parse** of the TS/JS/Python subset during indexing (Tier 2 only), on
  top of the existing parse. Bounded to the resolved subset.
- **`graph_edges` widens** by one nullable column and its dedup key. No consumer
  breaks: the bare-name join still works for `NULL` rows, and the two-column join
  is opt-in per consumer.
- **`plumbing graph-edges` JSONL** carries a new optional field. Additive;
  consumers that ignore unknown fields are unaffected.

## Alternatives considered

- **Wait for stack-graphs to reach tree-sitter 0.26 and unify on one runtime.**
  Rejected: out of our control and blocks the whole track. The sidecar isolates
  the version skew at a data boundary, which is cheaper than waiting.
- **Reuse ast-grep for scope analysis.** Not available: `ast-grep-core` was
  removed and `ast-grep-language` ships only grammars.
- **Intra-file resolution only (skip cross-file).** Insufficient: 68% of the
  multi-def edges are cross-file; Tier 1 alone cannot clear the bar the roadmap
  set.
- **SCIP / receiver-type inference now, to also close Mode 2.** Rejected for
  v1.2: heavier, needs a type signal the index does not carry, and Mode-1 (no
  types) is the cheaper, broader win to land first. Deferred as its own tier.
- **A separate `edge_resolution` table.** Rejected: `graph_edges` has no stable
  edge id (it is replace-per-file), so a side table buys nothing over a nullable
  column and complicates the rebuild.

## Consequences

- Resolution is **pure local computation** over files the indexer already reads:
  no new external input, no new trust boundary, no network. `target_file` flows
  as a bind parameter exactly like `target_name`, so no SQL-string-formatting
  surface is introduced. `THREAT-MODEL.md` is unchanged, and the stack-graphs
  sidecar must resolve from **vendored/bundled** grammars — never fetch at
  runtime — so `egress_containment.rs` continues to pass.
- Both tiers are **deterministic and offline**: the same repo yields the same
  `target_file`, satisfying the same promise the engine relies on.
- The realized value is staged. Populating `target_file` and proving the
  resolution-% lift and cycle/impact trustworthiness (via the probe) is the
  mandatory bar. Feeding `target_file` into PageRank and `search --graph` ranking
  is the **stretch** (the roadmap's "ranking-lift" half of acceptance), scoped as
  follow-on once the substrate is proven.

## Validation the implementation must produce

- On lago and on inkentry: multi-def resolution % under `locals.scm`, and under
  `locals.scm + stack-graphs`, each reported against the 32% crude-locality
  floor, with the module-graph edge and cycle deltas.
- A determinism check: two indexes of the same tree produce byte-identical
  `target_file` assignments.
- `egress_containment.rs` still traps every outbound connection across
  `init`/`index`/`search` — the sidecar adds none.
