# ADR-082: Unified search — collapse the search command surface

**Date:** 2026-08-08
**Deciders:** founder (Johan); architect
**Relationship to prior ADRs:** pairs with
[ADR-081](081-unified-search-rank-fusion.md), which decides *how* code and
memory are ranked into one list; this record decides *what command surface*
exposes it. It continues the clean-break removal policy of
[ADR-079](079-deprecate-explore-command-and-route.md) (the `explore` command,
removed the same way the earlier `ask` command was) and applies it to the search
family.

## Context

The product thesis for `search` is **one command, always the best available
result, no mode to choose**. Today the search capability is spread across
several surfaces that each contradict that thesis:

- **`inkentry search --mode {auto,text,semantic,hybrid,ast-grep}`** — a mode
  selector (`SearchArgs.mode: String`, default `auto`) that asks the user to
  pick the retrieval strategy.
- **`inkentry graph <symbol>`** — a separate top-level code-graph porcelain
  (`cli/cmd/graph.rs`) that traces callers/callees, with its own ast-grep live
  fallback (`graph_live`).
- **`inkentry memory search <q>`** — a parallel search command over the memory
  corpus (`cli/cmd/memory/search.rs`).

ADR-081 collapses the code and memory corpora into one ranked list. This ADR
collapses the *command surface* that exposes it. Keeping the old surfaces as
deprecated aliases would carry dead surface pre-revenue, against the release's
clean-break policy.

## Decision

1. **One `search` command, no modes.** `--mode` is removed entirely (all of
   `auto`/`text`/`semantic`/`hybrid`/`ast-grep`). Corpus selection becomes a set
   of filters over the single pipeline: `--only-code`, `--only-memory`,
   `--only-text` (FTS, no embed). `--only-code` and `--only-memory` are mutually
   exclusive. `--graph` enrichment stays (append 1-hop call-graph neighbours
   after the ranked members). The old `--mode text` maps to `--only-text`; the
   old `semantic`/`hybrid`/`auto` all map to the default best-available search.

2. **The in-process ast-grep live-search engine is removed.** `--mode ast-grep`
   goes with the rest of `--mode`, and `crate::search::live`
   (`crates/inkentry-core/src/search/live.rs`, backed by `ast-grep-core`) loses
   all non-test callers. A grep for `search::live` / `search_live` /
   `graph_live` / `live::` across all crates finds non-test callers **only** in
   `cli/cmd/search.rs` and `cli/cmd/graph.rs` — both of which this ADR removes
   or rewrites — so the file is deleted along with the `search_live`/`graph_live`
   helpers. The zero-coverage / embedder-unavailable / stale-empty paths that
   previously fell back to ast-grep degrade to **FTS** instead (ADR-081), which
   covers every chunk from parse time. An uninitialised directory is a runtime
   funnel to `inkentry init`, not a fallback.

3. **`inkentry graph` (the top-level code porcelain) is removed.** The graph
   capability ships as **`search --graph`** (porcelain enrichment) and
   **`plumbing graph-edges`** (JSONL, for scripts and agents). Exact-symbol
   lookup splits cleanly and needs no new flag:
   - *The code of a symbol* is found by the default hybrid's FTS component —
     `chunks_fts` indexes `name`, so an exact identifier ranks the named chunk
     at or near the top on BM25.
   - *Its connections* come from `search <symbol> --graph` (the symbol's chunk
     plus its 1-hop neighbours) and `plumbing graph-edges --symbol <name>`
     (exact `source_name`/`target_name` match, for a guaranteed-exact machine
     result).

   `inkentry memory graph` is a **different** command — it navigates
   relationships *from a memory entry*, not the code call-graph — and stays,
   along with `memory show`, `memory timeline`, `memory list`, `memory add`,
   `memory supersede`, and the rest of the `memory` family.

4. **`inkentry memory search` folds into `search`.** The fold-in touches only
   `memory search`; `--as-of <date>` (point-in-time over the memory corpus) and
   `--expand-graph` (memory relates-to 1-hop) carry onto `search` as memory-only
   modifiers.

5. **`--only-graph` is deliberately not in v1.** The graph capability is
   delivered through `search --graph` + `plumbing graph-edges`. A `--only-graph`
   that emitted edge records would introduce a **third** output shape into a
   command whose selling point is one unified result envelope (ADR-081).
   Edges-as-a-search-result is a possible post-v1 task, not v1.

### Removal is a hard cut — bare clap errors, no stubs

Consistent with the clean-break policy already applied across this release —
`explore` (ADR-079) and the earlier `ask`; the removal of the `check`
subcommand; the collapse of the one-way/streaming memory-sync porcelain
(`push`/`pull`/`watch`/`since`) into a single `sync` — a removed surface is
deleted **outright**, with no deprecation stub, hidden alias, or "has moved
to…" signpost. Invoking a removed surface falls through to clap's standard
unknown-subcommand / unexpected-argument error, exactly as `inkentry explore`
does after ADR-079:

- `inkentry graph …` → clap's unknown-subcommand error.
- `inkentry memory search …` → clap's unknown-subcommand error (under
  `memory`).
- `inkentry search --mode …` (any value) → clap's unexpected-argument error,
  because the flag no longer exists.

(Amended at implementation: these three still exit 2 and are still absent from
the command tree, but the message names the replacement. See the amendment at
the end of this record.)

The pre-v1 rename window is the migration; the `--help` output and the docs
carry the current surface. The **one** message that is not a removal error is
the **uninitialised-directory funnel**: `search` still exists, so running it
without an index is a runtime condition, not a removed surface, and it emits the
actionable "run `inkentry init`" funnel rather than a bare clap error or a
silent empty result.

### Output shape is a hard cut

`inkentry search --format json` changes from a top-level array of `SearchResult`
to the nested unified envelope defined in ADR-081
(`{type, fused_rank, fused_score, corpus_rank, code|memory}`). **No legacy flat
shape is preserved** for one release under `--only-code`; the shape is hard-cut
like the rest of the release. Consumers parsing the old array break and must
move to the envelope.

## What breaks

- Scripts and agent skills invoking `inkentry graph …`, `inkentry memory search
  …`, or `inkentry search --mode …` — all now error via clap. Replacement
  paths: `search --graph` / `plumbing graph-edges`; `search --only-memory`; and
  the default best-available `search` (with `--only-text` for the old `--mode
  text`).
- Anything parsing the flat code-search JSON array — now the nested envelope
  (ADR-081).
- The index-free, zero-setup "returns results with nothing indexed" affordance
  is gone: `search` requires an index. FTS is available immediately once `init`
  has parsed the tree, while embeddings build in the background; a directory
  with no index funnels to `init`. Advertising copy that promises zero-index
  search needs updating.
- `crate::search::live` and the `search_live`/`graph_live` helpers are deleted.

## Alternatives considered

- **Keep the old surfaces as deprecated aliases for one release.** Rejected:
  dead surface carried pre-revenue, and inconsistent with the clean-break policy
  already applied to `explore`/`ask`/`check`/`sync`. The rename window is the
  migration.
- **Signposted "has moved to…" errors on each removed surface.** Rejected in
  favour of bare clap errors, matching the rest of the release. A maintained
  per-surface migration message is itself a stub to build, ship, and later
  delete; the clean-break precedent (`explore` falling through to clap's
  unknown-subcommand error) is the established shape. **Partly revised at
  implementation — see the amendment below.**
- **Add `--only-graph` (edges as a result type) in v1.** Rejected: a third
  output shape in a command whose value is one envelope; speculative until a
  consumer needs edges-as-a-result. A post-v1 task.
- **Keep ast-grep live search as the zero-index fallback.** Rejected: it
  contradicts "search requires an index," and FTS (which covers every chunk from
  parse time) is the correct degrade. The engine's only non-test callers are the
  surfaces removed here.

## Consequences

- **One search command.** The corpus and the enrichment are flags, not modes or
  sibling commands; there is a single place to look for "how do I search."
- **The ast-grep *search* role ends.** Tree-sitter grammars from
  `ast-grep-language` stay (they back parsing/chunking); only the
  `ast-grep-core` structural-search engine in `search/live.rs` is removed.
- **A file-scan surface is removed.** The in-process ast-grep live search walked
  the working tree directly; deleting it leaves the surviving read paths (FTS,
  hybrid, graph) index-backed. The relevant threat-model rows for the
  live-search working-tree scan are updated to reflect the removed surface.

## Prerequisites

Docs-only decision; not yet implemented. Implementation is gated on sign-off of
this ADR and ADR-081, plus the shared retrieval-benchmark baseline noted in
ADR-081.

## Amendment (2026-08-10, at implementation): the three removed surfaces name their replacement

The decision above rests on a premise that the implementation did not hold up:
"the `--help` output and the docs carry the current surface." They did not.
`SKILL.md` — the file an agent reads to learn how to drive this tool — shipped 17
invocations of the removed surfaces, and `README.md` 11, including the multi-hop
loop that `docs/agent-guide.md` redirects readers to. An agent following that
loop hit a bare `unrecognized subcommand` with no way to tell that its guidance,
not its invocation, was wrong.

Worse, one of the errors was not bare. `search` is one edit from `archive`, so
clap's did-you-mean answered `inkentry memory search` with

```
tip: a similar subcommand exists: 'archive'
```

pointing at an unrelated command. A wrong pointer is worse than no pointer, and
it is not the "clap's standard unknown-subcommand error" this record specified.

So: the docs are fixed, **and** the three surfaces this ADR removes exit 2 with a
message naming their replacement. What the original rejection was protecting
against is still avoided — nothing is registered with clap, nothing appears in
`--help`, there is no runnable stub, and no per-surface code path. The whole
migration surface is one table matched against argv at the single parse site
(`cli/removed.rs`), which is deleted in one edit when the rename window closes.

This amendment covers only `graph`, `memory search` and `search --mode`. The
earlier clean-break removals (`explore`, `ask`, `check`, the memory-sync
porcelain) keep their bare clap errors; nothing here reopens them.
