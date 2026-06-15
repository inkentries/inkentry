# Changelog

All notable changes to spelunk are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).
spelunk uses [Semantic Versioning](https://semver.org/).

---

## [Unreleased]

### Security

- **gix/gitoxide Dependabot alerts verified resolved (P1-2).** All 7 open Dependabot
  security alerts (6 high, 1 medium) for gix-related crates were already cleared by
  previous bumps (PRs #307, #327, #334). Cargo.lock contains patched versions:
  gix 0.84.0 (>=0.83.0), gix-fs 0.21.2 (>=0.21.1), gix-pack 0.71.0 (>=0.69.0),
  gix-transport 0.57.1 (>=0.56.0). `cargo audit` returns zero vulnerabilities;
  one allowed warning (RUSTSEC-2024-0436 paste unmaintained, in audit.toml).
  Affected advisories: GHSA-fr8x-3vfx-f45h, GHSA-pg4w-g64p-qwhj, GHSA-f26g-jm89-4g65,
  GHSA-p3hw-mv63-rf9w, GHSA-f89h-2fjh-2r9q, GHSA-x494-mj8g-cj27, GHSA-9857-6mw7-fq2m.

- **`spelunk memory add` blocks the entire write on secret detection.** When a
  secret is detected at input time, both the SQLite backend write and the
  git-notes write are now aborted — no partial write occurs. Previously only
  the git-notes path was guarded; the SQLite path could still persist a
  credential-containing entry. (#344)

### Added

- **Cross-project memory visibility (`spelunk memory search|list|context`).** When
  projects are linked with `spelunk link`, `memory search`, `memory list`, and
  `context` now also query each linked project's memory store and surface
  `locked` or `cross-project`-tagged `decision` and `requirement` entries
  alongside local results. Each cross-project result is tagged with its source
  project (`[from: <project>]` in text mode; `source_project` /
  `source_project_path` fields in JSON) so conflicting decisions remain
  attributable. `handoff` and `question` entries remain strictly project-local.
  (ADR-003)

- **`--local-only` flag for `memory search`, `memory list`, and `context`.**
  Suppresses the cross-project dep pass and queries only the primary project's
  memory store -- matching the existing `spelunk search --local-only` behaviour.
  (ADR-003)

- **`spelunk memory reconcile`** — imports notes from a local `spelunk-server`
  database (`server.db`) into the project's `memory.db` by content-hash dedup.
  Reads `server.db` in read-only mode; never writes to it. Supports `--dry-run`
  (report candidates without importing), `--json` (NDJSON summary), and
  `--all-projects` (reconcile every project slug found in `server.db`). Exits
  with code 0 when there is nothing to import. Key flag: `--source-db <path>`
  overrides the default source path (`~/.local/state/spelunk/server.db`).
  (#391, ADR-004 follow-up)

- **`spelunk memory reconcile`** — import memory entries from a `spelunk-server` SQLite
  database into the local project database without running the server. Flags:
  `--source-db <path>`, `--dry-run` (preview without writing), `--all-projects`
  (import across all server projects), `--format json` (machine-readable output).

### Fixed

- **SQLite LIKE queries now escape metacharacters in file paths and symbol names.** File
  paths and symbol names containing `%` or `_` were causing over-matching in
  `file_paths_under`, `chunks_for_file`, `symbol_history`, and `stale_specs` queries.
  An `escape_like()` helper now escapes these characters before binding, with
  `ESCAPE '\\'` on the SQL clause. ([#406](https://github.com/spelunk-cloud/spelunk/issues/406))

- **`spelunk memory watch --help` now references `server_url` correctly.** The
  subcommand doc-comment previously said `requires memory_server_url` (the
  deprecated alias); corrected to `requires server_url`.
  ([#400](https://github.com/spelunk-cloud/spelunk/issues/400))

- **`explore` now appears in `spelunk --help`.** The `explore` subcommand was
  inadvertently hidden from the top-level help output; the hide attribute has
  been removed so users can discover it alongside the other subcommands.
  ([#400](https://github.com/spelunk-cloud/spelunk/issues/400))

---

## [0.8.1] — 2026-06-10

### Fixed

- **`spelunk search` honors auto-discovered loopback server.** Explicit
  `--mode semantic`/`--mode hybrid` no longer error with "requires
  spelunk-server", and `--mode auto` no longer silently falls back to
  ast-grep, when no `server_url` is configured but a local `spelunk-server`
  was auto-discovered via the loopback probe (the default v0.8.0 UX).

- **Native embedder: fixed memory spike, CPU saturation, and index timeout.**
  Indexing large projects no longer triggers a ~20 GB memory spike, ~750%
  CPU usage across 31 threads, or HTTP timeouts during the embed phase.
  Adds a new `--embed-threads` CLI arg (default 4, env
  `SPELUNK_EMBED_THREADS`). Verified: 124-file / 1330-chunk index completes
  in 7m30s with stable ~3.5 GB memory and ~350-400% CPU.

- **Native embedder: reduced CoreML activation footprint and added compiled-model
  cache** for hardware EP builds (`embed-coreml` / `embed-xnnpack` /
  `embed-directml`), cutting peak memory from ~4 GB to ~1 GB and avoiding
  CoreML recompilation on every server start. These hardware EP features
  remain experimental and are not recommended over the default CPU EP — see
  `docs/server.md`.

### Security

- **Auto-spawned `spelunk-server` now binds to `127.0.0.1` only.** Previously
  the server started by `spelunk init` / `ensure_server_running` defaulted to
  `0.0.0.0`, making the unauthenticated local server LAN-reachable.
  (THREAT-MODEL req #9, decision #88)

### Added

- **`spelunk status`/`check --format json`** now include a `memory_backend`
  field (`"sqlite"`, `"remote"`, or `"git-notes"`); `spelunk status` text mode
  shows a "Memory backend: <kind>" line. (#308)

### Changed

- **NDJSON terminology renamed to JSONL** throughout the CLI, docs, and tests.
  The `--format` flag value `ndjson` is now `jsonl` for `search`, `graph`, and
  `memory` commands. (#348)

- **Internal refactor:** `storage::remote` and `storage::git_notes` split into
  module directories to stay under the 400-line file limit. No public API
  changes.

- **Homebrew tap moved to a separate repo** (`spelunk-cloud/homebrew-spelunk`);
  the release workflow now publishes the formula there directly.

---

## [0.8.0] — 2026-06-08

### Breaking changes — migration required

**All AI inference commands now route through `spelunk-server`.**

The following commands previously called LM Studio (or another
OpenAI-compatible endpoint) directly via `api_base_url`. They now require a
running `spelunk-server` reachable at `server_url` in your config:

| Command | Previously needed | Now needs |
|---|---|---|
| `spelunk explore` | `api_base_url` + `llm_model` | `server_url` |
| `spelunk search` (semantic/hybrid) | `api_base_url` + `embedding_model` | `server_url` |
| `spelunk memory search` (semantic) | `api_base_url` + `embedding_model` | `server_url` |
| `spelunk memory timeline` | `api_base_url` + `embedding_model` | `server_url` |
| `spelunk memory add` (auto-embed) | `api_base_url` + `embedding_model` | `server_url` (optional, degrades gracefully) |
| `spelunk index` (embed phase) | `api_base_url` + `embedding_model` | `server_url` |
| `spelunk index` (summaries) | `api_base_url` + `llm_model` | `server_url` |
| `spelunk plumbing embed` | `api_base_url` + `embedding_model` | `server_url` |
| `spelunk memory harvest` | `api_base_url` + `llm_model` | `server_url` (unchanged since #310) |

**Migrating from `lm_studio_url` / `api_base_url`:**

If you previously ran a local LM Studio and set `api_base_url` in your config,
you now need to run `spelunk-server` in front of it:

```toml
# ~/.config/spelunk/config.toml

# Old config (no longer used for inference):
# api_base_url = "http://127.0.0.1:1234"

# New config:
server_url = "http://127.0.0.1:7777"   # spelunk-server address
project_id = "your-org/your-project"   # required when server_url is set
```

Start `spelunk-server` and point it at your LM Studio instance:

```sh
spelunk-server \
  --embedding-url http://127.0.0.1:1234 \
  --embedding-model text-embedding-embeddinggemma-300m-qat \
  --llm-url http://127.0.0.1:1234 \
  --llm-model google/gemma-3n-e4b \
  --port 7777
```

Commands that do **not** need inference (parse, graph, FTS search, status,
memory list/show/archive) continue to work offline without `server_url`.

### Changed

- **`spelunk-core` no longer contains embedding or LLM implementations.**
  `OpenAiCompatEmbedder`, `OpenAiCompatLlm`, and `backends.rs` have been
  removed from `spelunk-core`. The `EmbeddingBackend` and `LlmBackend` traits
  remain in `spelunk-core` for use by `spelunk-server`'s `AppState`. (#260, #312)

- **Capability module moved from `spelunk-core` to `spelunk-cli`.** The tier
  detection logic (`get_tier`, `require_tier1`) is now internal to the CLI
  binary. Nothing outside spelunk-cli should depend on it. (#312)

---

## [0.7.1] — 2026-05-27

### Added

- **`spelunk-server` HTTP API** — Axum-based REST server with AuthProvider
  trait, `/v1/embed`, `/v1/explore`, `/v1/plan` endpoints, and an OpenAPI spec
  committed alongside the binary. Server-side embedding is optional; pass
  `SPELUNK_EMBEDDING_URL` to enable it. Prompt-injection patterns are rejected
  server-side before storage. (#261, #221, #222)

- **`spelunk status --format json`** — stable machine-readable schema for
  status output, suitable for CI dashboards and agent health checks. (#269)

- **Heuristic convention extraction** — `spelunk index` now detects and stores
  project conventions (naming patterns, async style, test coverage, doc
  coverage) derived from the AST. Results are surfaced in `spelunk context`
  output. (#268)

- **Compatibility tier model** — `spelunk check` reports a capability tier
  (Local / Embedded / Full) so agents can adapt their strategy to the available
  inference backend at runtime. (#259)

- **`spelunk graph --live`** — passes the query to ast-grep as a fallback when
  the indexed call graph has no results, giving live symbol resolution for
  unindexed or recently changed code. (#216)

### Changed

- **3-crate Cargo workspace** — the codebase is now split into `spelunk-core`
  (library), `spelunk-cli` (binary), and `spelunk-server` (binary + lib) under
  a shared workspace root. `CLAUDE.md` and `README.md` updated accordingly.
  (#220)

- **`gix` status API** — subprocess calls to `git status` replaced with
  `gix::status` API, removing a shell dependency and improving reliability
  inside IDE integrations. (#215)

- **`spelunk explore` now requires a configured server** — the command is gated
  behind the Tier 2/3 capability check (`server_url` must be set and reachable).
  The previous check for `llm_model` has been removed in line with decision #47
  (no LLM inference in the CLI without a server). Run `spelunk status` for
  guidance if the command is unavailable.

### Fixed

- `spelunk-server` OpenAPI spec gaps: `SearchRequest` missing `text` field,
  JSON error shapes aligned to `application/json` responses, CI step added to
  gate spec drift. (#288)

- `.spelunk` symlink replaced with runtime worktree-root resolution, fixing an
  infinite-symlink issue when indexing inside a git worktree. (#266)

- `spelunk memory harvest` now swallows per-entry errors and continues rather
  than aborting the entire run on a single bad entry. (#270)

### Dependencies

- `serde_json` 1.0.149 → 1.0.150
- `tree-sitter` 0.26.8 → 0.26.9
- `tower-http` 0.6.10 → 0.6.11

---

## [0.7.0] — 2026-05-17

### Added

- **`spelunk context`** — new agent session entry point command that surfaces
  index health, recent memory, and open questions in a single structured
  output, making it easier for AI agents to orient at the start of a session.

- **`spelunk memory harvest --source entire`** — mines Entire.io checkpoint
  files (stored on the `refs/entire/checkpoints/v1` branch) for decisions,
  notes, and requirements. The structured `Summary` in each checkpoint is used
  directly; an LLM fallback is used only for checkpoints that lack one. Secret
  scanning is applied on all paths so credentials are never stored.

- **`GitMetaBackend`** — second memory backend backed by `git-meta-lib`, available
  alongside the existing git-notes and server backends.

- **`git-notes` memory backend** (`GitNotesBackend`) — store and retrieve memory
  entries using git's native notes mechanism, with no server or external database
  required. Supports optional embedding for semantic memory search. `NoteRecord`
  now carries a `schema_version` field for forward-compatible detection of future
  record formats.

- **Benchmark suite** (`bench/`) — five benchmarks now ship with the repo to
  measure and document retrieval quality and performance: Decision Archaeology
  (memory recall), Cross-Session Handoff (agent continuity), SWE-bench
  (patch resolution rate via Docker harness), Code-Graph (grep vs search vs
  graph), and Perf-at-Scale (indexing and search latency at 50k–500k LOC).
  A benchmarking report (`tmp/benchmarking-report.md`) summarises current
  results.

### Changed

- **Removed legacy commands** — `ask`, `plan`, `spec`, `snapshot`, `history`,
  and `verify` subcommands have been removed. The core spelunk workflow is now
  index-free by default; docs and SKILL.md have been updated to reflect this.

- **`gix::discover` replaces `git rev-parse`** — subprocess calls to
  `git rev-parse` for repository discovery are replaced with `gix::discover`,
  removing a shell dependency.

- **Dependency updates** — `gix` bumped to 0.83.0 (fixes worktree exclude
  handling so `.gitignore` rules are correctly respected in git worktrees);
  `git-meta-lib` bumped to 0.1.10 with API adaptation.

### Fixed

- `GitNotesBackend` unsupported methods now return a typed `BackendUnsupported`
  error instead of panicking.

- `GitNotesBackend` list capped at 500 entries, preventing an O(n) hang on
  repositories with many notes.

- `spelunk index` no longer creates a self-referential `.spelunk` symlink when
  run inside a git worktree checked out at the same path as the main repo.

### Security

- Server audit checklist (`docs/security/`) added as groundwork for the upcoming
  v1.0 server release.

---

## [0.6.0] — 2026-04-26

### Added

- **LinearRAG two-stage retrieval** — a new graph-diffusion retrieval algorithm
  combining personalised PageRank with full-text pre-filtering. Now the default
  for `spelunk search`; multi-hop recall is +33.5 % over the previous baseline
  at ~1.9× median latency. Compound indexes and in-memory Stage 1 propagation
  keep latency within acceptable bounds.

- **`.spelunkignore` files** — place a `.spelunkignore` file anywhere in your
  project tree to exclude paths from indexing, using the same format as
  `.gitignore`.

- **`antipattern` memory kind + `spelunk memory failures`** — record failure
  patterns and anti-patterns as first-class memory entries. `spelunk memory
  failures` lists them; `spelunk memory harvest` can extract them from session
  history.

- **Expanded fuzzer coverage** — fuzzer targets now cover secrets, chunker,
  `escape_xml`, JSONL, and history entry parsing.

### Fixed

- OOM guard added to the parser; `doc_comment` nodes are skipped during AST
  walking to avoid memory exhaustion on files with large doc blocks.

- `spelunk check` was made fully async, fixing a panic caused by calling
  `block_on` inside an existing async context.

- PageRank dangling-node indices are now precomputed before power iterations,
  improving performance on sparse graphs.

---

## [0.5.0] — 2026-04-21

### Added

- **Unix plumbing/porcelain architecture** — 8 new `spelunk spelunk` plumbing
  subcommands emit machine-readable JSONL to stdout and use conventional exit
  codes (0 = ok, 1 = no results, 2 = error). All porcelain commands now accept
  `--format text|json|jsonl` for structured output in scripts and agents.
  Plumbing commands: `cat-chunks`, `embed`, `graph-edges`, `hash-file`, `knn`,
  `ls-files`, `parse-file`, `read-memory`.

- **`spelunk memory harvest --source claude-code`** — mines Claude Code session
  history files (`.claude/projects/*/sessions/*.jsonl`) for decisions, notes,
  and requirements; deduplicates against already-stored entries; stores the
  results directly in the memory index.

- **`intent` memory kind** — agents record work-in-progress intent entries so
  collaborating agents (and humans) can see what is actively being changed.
  `spelunk check` now shows active agent sessions alongside the index health
  summary, and warns when any intent's linked files overlap with files recently
  modified in the current worktree.

- **Server-side conflict detection** — `spelunk-server` runs a KNN similarity
  search before storing each new memory entry; entries that closely contradict
  an existing active entry are flagged with a `contradicts` edge and the HTTP
  response includes a `409 Conflict` status with the conflicting entry IDs.
  A `--conflict-threshold` flag controls the cosine-distance trigger.

- **`spelunk memory since` / `spelunk memory watch`** — incremental memory feed
  (`since`) and a long-running SSE stream (`watch`) for agents that want to be
  notified of new memory entries in real time. _(coming soon in 0.5.x — not
  yet merged as of this release)_

- **Benchmark scripts** (`bench/`) for evaluating search quality across
  indexing configurations.

### Changed

- `--format text|json` standardised across all porcelain commands (`ask`,
  `explore`, `search`, `graph`, `memory list`, `memory search`). The legacy
  `--json` flag is kept as a hidden deprecated alias.

- `storage/memory.rs` split into focused sub-modules
  (`storage/memory/`, `storage/db/`) to reduce file size and improve
  navigability, as part of the broader Unix-architecture refactor.

### Fixed / Security

- **XML escaping in LLM prompts** — spec titles and paths interpolated into
  `<spec_context>` blocks are now escaped with `escape_xml()`, closing a
  prompt-injection vector.

- **Expanded secret scanner** — `src/indexer/secrets.rs` now recognises
  OpenAI, Anthropic, and Stripe API keys; npm automation tokens; and database
  connection URLs containing inline credentials. Patterns compile once via
  `OnceLock`.

- **Atomic memory transactions** — `NoteStore` archive and supersede operations
  now run inside a single SQLite transaction; partial writes on crash are no
  longer possible.

- Resolved all security-audit findings from `cargo audit` (#136, #137, #138,
  #145) by upgrading affected dependency versions.

---

## [0.4.1] — 2026-03-21

Initial public release.
