# CLAUDE.md — inkentry

Developer guide for AI agents (and humans) working on this codebase.

---

## Agent workflow — use inkentry on this codebase

This project is indexed with inkentry. Use it — don't just use Read/Grep/Glob.

**At the start of every session:**
```bash
inkentry context                                   # pull prior decisions, handoffs, questions, requirements (compact by default; --budget <N> caps output tokens)
inkentry index .                                   # bring the index up to date (idempotent, blake3-gated; only if indexed)
```

**Before reading any file, search first:**
```bash
inkentry search "<topic>"                           # unified search over code + memory (best available)
inkentry search "<symbol>" --graph                  # a symbol's chunk plus its 1-hop call-graph neighbours
inkentry search "<topic>" --only-text               # full-text only (no embedding, no server needed)
inkentry plumbing graph-edges --symbol <symbol>     # exact call-graph edges as JSONL (for scripts/agents)
```

inkentry retrieves context — you synthesise the answer.

**Store decisions as you make them** — don't wait until the end:
```bash
inkentry memory add --kind decision --title "..." --body "why, what alternatives, what breaks"
inkentry memory add --kind requirement --title "..." --body "..."   # when user states a constraint
inkentry memory add --kind note --title "..."                       # surprising/non-obvious facts
```

**At the end of every session:**
```bash
inkentry memory add --kind handoff --title "Handoff: <summary>" --body "what's done, what's next, open questions"
# Optional: re-index if you've indexed the project
inkentry index .
```

Full reference: `docs/agent-guide.md`, and the agent skill at https://github.com/inkentries/agent-plugin.

---

## What This Project Is

`inkentry` is a Rust CLI and context retrieval engine for AI agents.

**Built-in (no inference server or cloud dependency):** git-notes memory, full-text search, code graph (AST + call edges), tree-sitter chunking. `search` requires an index: an uninitialised directory funnels to `inkentry init`. Once `init` has parsed the tree, full-text search is available immediately (over both code and memory) while semantic ranking builds in the background; the call graph is surfaced through `search --graph` and `inkentry plumbing graph-edges`.

**Semantic search via inkentry-server:** from v0.9.0 the default UX runs a local `inkentry-server` (auto-bound on `127.0.0.1`). The server bundles a native embedder (codefuse-ai/F2LLM-v2-330M, 896-dim, candle runtime, Metal/GPU on macOS) — no external embedding endpoint required. Semantic search and `inkentry harvest` route through the server's inference endpoints (`harvest` is the only feature that needs an LLM at all; chunk summaries are composed offline with no model); the CLI talks to it via `server_client.rs`. Manage the daemon with `inkentry server start|stop|status|logs`. This **auto-discovered loopback server is an inference backend only** — it embeds queries and runs LLM calls, but it is **never** a memory store. A project's memory always lives in its local `memory.db`; the loopback server holds no authoritative memory.

**Optional: team memory server** (`server_url` *explicitly* set in config, pointing at a shared instance): share memory (decisions, requirements) across a team. Setting an explicit `server_url` is the **only** way memory moves off the local `memory.db`, and how it moves is governed by the `mode` config (see `SyncMode` in `sync_mode.rs`): the default `local_first` keeps reads and writes in the local store with the server as a converging replica; `mode = "cloud_first"` relocates the store of record to the shared server, and reads/writes fail loudly when it is unreachable (no silent local fallback). Each developer's code stays local. (Note the distinction: an auto-discovered loopback server provides inference and never owns memory; an explicit team `server_url` does own memory. They must not be conflated.) `project_id` is sent to the server exactly as configured, slug or UUID: both a self-hosted inkentry-server and the hosted cloud API accept either, so there is no resolution step and nothing is cached (see ADR-005).

You search with inkentry, then reason over the results yourself.

---

## Workspace Structure

This is a Cargo workspace with four crates:

```
Cargo.toml                    — workspace root; [workspace.dependencies] for shared versions

crates/
  inkentry-core/               — library: storage, indexer, embeddings, LLM, search, config, registry
  inkentry-cli/                — `inkentry` binary; depends on inkentry-core
  inkentry-embed/              — library: native F2LLM-v2-330M embedder (candle); depends on inkentry-core
  inkentry-server/             — `inkentry-server` binary + lib; depends on inkentry-core + inkentry-embed
```

## Module Map

### inkentry-core (`crates/inkentry-core/src/`)

```
lib.rs           — crate root; re-exports public modules
error.rs         — InkentryError enum
config/
  mod.rs         — Config struct; load from ~/.config/inkentry/config.toml
  sync_mode.rs   — SyncMode enum: offline / local_first / cloud_first mode selection
  project_id.rs  — project-id derivation from git remote / local fallback
  paths.rs       — config-dir + project/db discovery
  persist.rs     — config.toml / secret-store read-write
  predicates.rs  — URL/UUID/env predicates
  tls.rs         — custom CA trust-anchor application
  secret_store.rs — OS keychain / file secret-store backend
  server_keys.rs — per-origin server-key map + bearer_for() resolution (ADR-071)
  llm_key.rs     — LLM endpoint credential: INKENTRY_LLM_KEY / secret-store resolution,
                   plus the INKENTRY_LLM_URL / INKENTRY_LLM_MODEL variable names. Deliberately
                   not a Config field and never read by Config::load; only the daemon-spawn
                   path resolves it
utils/
  mod.rs         — strip_ansi(), misc helpers
  dates.rs       — date parsing helpers
registry.rs      — global project registry (~/.config/inkentry/registry.db)

conventions/
  mod.rs         — ConventionRecord type; re-exports ConventionExtractor
  extractor.rs   — ConventionExtractor: heuristic AST pass over stored chunks
  rules/
    mod.rs       — per-language rule dispatch
    generic.rs   — language-agnostic convention rules
    rust.rs      — Rust-specific convention rules
    typescript.rs — TypeScript-specific convention rules

embeddings/
  mod.rs         — EmbeddingBackend trait, vec_to_blob/blob_to_vec helpers

llm/
  mod.rs         — LlmBackend trait, Message struct, Token type

indexer/
  mod.rs         — re-exports Chunk, ChunkKind, SourceParser
  chunker.rs     — Chunk / ChunkKind structs; sliding_window fallback
  docparser.rs   — document-level parsing helpers
  pagerank.rs    — PageRank over the code graph
  pdf.rs         — PDF text extraction
  secrets.rs     — contains_secret(): regex scanner, drops credential chunks
  summariser.rs  — deterministic structural chunk summaries (no model, no key, no network)
  graph/
    mod.rs       — re-exports EdgeExtractor
    edges.rs     — EdgeExtractor: import/call/extends edges via tree-sitter
    builtins.rs  — built-in symbol skip-list
  parser/
    mod.rs       — SourceParser; detect_language; SUPPORTED_LANGUAGES
    text.rs      — plain-text / sliding-window parser
    ts_walker.rs — tree-sitter AST walker

storage/
  mod.rs         — re-exports Database
  db.rs          — Database struct; open/migrate; connection setup
  files.rs       — file record CRUD (insert, lookup, delete)
  chunks.rs      — chunk CRUD (insert, fetch, delete by file)
  conventions.rs — conventions table CRUD (no dependency on conventions/)
  search.rs      — KNN search queries against sqlite-vec
  graph.rs       — graph_edges CRUD
  specs.rs       — spec record CRUD
  stats.rs       — aggregate statistics queries
  note_record.rs — NoteRecord struct (memory entry)
  git_notes/
    mod.rs         — GitNotesBackend struct + helpers; append_to_git_notes free function
    backend_impl.rs — MemoryBackend trait impl for GitNotesBackend
  memory/
    mod.rs       — NoteStore: memory entries CRUD + list_filtered
    edges.rs     — memory relationship edges CRUD
    notes.rs     — note insert/fetch/delete
    search.rs    — memory FTS + semantic search
    tests.rs     — integration tests for NoteStore
  backend.rs     — StorageBackend trait (local vs remote)
  remote/
    mod.rs         — RemoteMemoryBackend struct + URL helpers + MemoryBackend impl
    wire_types.rs  — HTTP request/response structs (AddNoteRequest, NoteResponse, etc.)
    tests.rs       — #[cfg(test)] tests for URL encoding and search wire format

search/
  mod.rs         — SearchResult struct; RRF_K, the one reciprocal-rank-fusion
                   constant shared by both within-corpus hybrid fusions and the
                   cross-corpus code+memory fusion (ADR-081)
  rag.rs         — linearrag_search: LinearRAG two-stage retrieval (entity
                   activation + personalised PageRank)
  tokens.rs      — token-budget helpers

migrations/  (crates/inkentry-core/migrations/)
  index_001_initial.sql  — index.db at its final shape (no ladder; see storage/db.rs)
  memory_001_initial.sql — memory.db at its final shape
```

### inkentry-cli (`crates/inkentry-cli/src/`)

```
main.rs          — entry point: parse CLI, dispatch to commands
capability/      — Tier 0/1 capability detection (server reachable probe, cached per-process)
  mod.rs         — module doc + re-exports
  state.rs       — Capabilities / EmbedderState / ServerLimits: data parsed from `/v1/health`
  tier.rs        — the Tier enum itself
  probe.rs       — loopback auto-discovery, explicit server_url health probing, the Tier cache
  diagnostics.rs — probe-failure classification + TLS error rendering
  guard.rs       — require_tier1 / require_explicit_server_url: feature-gating checks
  llm_route.rs:    LlmRoute + resolve_llm_route: where LLM calls go (local tier /
                   explicit server_url / nowhere-with-a-reason). Separate from embed
                   routing; never consults Config::resolve_inference_url
  llm_message.rs:  no_llm_message: the user-facing text over (NoLlmReason x LlmFeature),
                   used by harvest, the only LLM-backed feature
server_client.rs:  ServerInferenceClient, the single HTTP client for inkentry-server's
                   inference endpoints, plus ServerLlmAdapter, a thin LlmBackend trait
                   adapter over an Arc of it (embedding is issued directly via embed_text,
                   with no adapter). Embedding and LLM can resolve to different base URLs,
                   so a caller needing both builds two clients

cli/
  mod.rs         — clap structs (Cli, Command, *Args)
  cmd/
    mod.rs       — re-exports one pub fn per subcommand
    auth.rs      — `inkentry auth set-key/list-servers` handlers (ADR-071); `--llm` stores
                   the LLM endpoint credential. `remove-key` removes any stored
                   credential: one origin, all of them, or the LLM key (ADR-090)
    context.rs   — `inkentry context` handler (agent session entry point)
    daemon_llm.rs — LlmSpawn: resolves the spawned daemon's LLM url/model/credential and
                   splits them across argv (url, model) and the child environment (all
                   three, pinned so nothing is left to inheritance)
    fusion.rs    — cross-corpus rank fusion + the unified code/memory result envelope (ADR-081)
    harvest.rs   — `inkentry harvest` handler (top-level; capture memory from
                   git history + session logs). Shares its implementation and
                   memory-store resolution with the deprecated `memory harvest`
    helpers.rs   — shared output / progress helpers
    hooks.rs     — `inkentry hooks` handler
    init.rs      — `inkentry init` handler
    link.rs      — `inkentry link/unlink/autoclean` handlers
    links.rs     — `inkentry links` handler
    misc.rs      — `inkentry chunks` / `inkentry languages` handlers
    search.rs    — `inkentry search` handler (unified code+memory, RRF fusion, corpus filters)
    server.rs    — `inkentry server start/stop/status/logs` daemon management
    status.rs    — `inkentry status` handler
    ui.rs        — TUI helpers (private)
    index/
      mod.rs         — `inkentry index` entry point
      embed_phase.rs — embedding phase of indexing
      mentions.rs    — mention stopword filter used during indexing
      parse_phase.rs — parse/chunk phase of indexing
      summaries.rs   — composes each chunk's structural summary during index (offline)
      worktree.rs    — git worktree handling for index
    memory/
      mod.rs          — `inkentry memory` dispatch
      add.rs          — memory add subcommand
      archive.rs      — memory archive subcommand
      failures.rs     — `inkentry memory failures` handler
      graph_cmd.rs    — memory graph subcommand
      harvest.rs      — deprecated `memory harvest` alias + shared harvest
                        implementation (harvest_clients), reused by the top-level
                        `inkentry harvest`
      harvest_claude.rs — harvest from ~/.claude/history.jsonl (Claude Code sessions)
      corpus.rs       — memory-corpus retrieval for unified search (the folded-in
                        former `memory search`: hybrid/FTS + expand-graph + cross-project)
      list.rs         — memory list subcommand
      reconcile.rs    — memory reconcile subcommand (import from server.db)
      show.rs         — memory show subcommand
      supersede.rs    — memory supersede subcommand
      timeline.rs     — memory timeline subcommand
    plumbing/
      mod.rs               — PlumbingArgs/PlumbingCommand; dispatch; exit-2 on error
      cat_chunks.rs        — emit indexed chunks for a file as JSONL
      embed_cmd.rs         — read stdin lines, emit embedding vectors as JSONL
      graph_edges.rs       — emit code graph edges as JSONL
      hash_file.rs         — blake3 hash a file; check index currency
      knn.rs               — KNN vector search, JSONL output
      ls_files.rs          — list indexed files as JSONL; exit 1 if no results
      parse_file.rs        — parse a file and emit chunks as JSONL (no DB write)
      pull.rs              — one-way server->local memory pull; JSONL report
      push.rs              — one-way local->server memory push; JSONL report
      read_memory.rs       — emit memory entries as JSONL
```

### inkentry-server (`crates/inkentry-server/src/`)

```
main.rs            — entry point: parse args, register sqlite-vec, start Axum server
lib.rs             — AppState, router, auth_middleware, AppError, ApiDoc (utoipa)
db.rs              — ServerDb: SQLite schema, memory CRUD, KNN search, embedding dim guard
handlers/
  mod.rs           — shared validation/rate-limit helpers, module re-exports
  health.rs        — GET /v1/health
  projects.rs      — list_projects, project_stats
  notes.rs         — note CRUD wire types + add/list/get/search/delete/archive/supersede handlers
  batch.rs         — POST /memory/batch (wire parity with cloud-api)
  sync.rs          — harvested_shas, GET /memory/since, GET /memory/stream (SSE)
  index.rs         — POST /index/embed (server-side embedding, not stored)
  search.rs        — POST /search (query-embedding proxy for CLI-side KNN)
  llm.rs           — POST /llm/complete (generic streaming completion primitive)
  tests/           — #[cfg(test)] suite, split by theme; see mod.rs for the file list
    support.rs     — shared app/router builders + HTTP helpers used by every theme
    *_tests.rs     — one file per theme (notes, health, embed, search, llm, batch,
                     batch dedupe, sync, timeout, concurrency, liveness-under-embed,
                     wire shape)
server_llm.rs      — ServerLlm: the external chat-completions HTTP shim behind `--llm-url`,
                     plus resolve_llm_key (--llm-key / --llm-key-file / INKENTRY_LLM_KEY) and
                     check_llm_transport, which refuses to start when a credential would
                     travel in the clear
embed_hub.rs       — Hugging Face Hub download path for the bundled native embedder (gated by
                     `embed-native`); fetches the pre-quantized GGUF/tokenizer/config to disk, then
                     calls inkentry-embed's `NativeEmbedder::load_from_path`. The only place in the
                     workspace that depends on `hf-hub`.

migrations/  (crates/inkentry-server/migrations/)
  server_001.sql — projects + server memory schema
  server_002.sql — server memory FTS
```

The native embedder engine lives in the `inkentry-embed` crate (below). The
server's `embed-native` feature enables the optional `inkentry-embed` dep (and
the server's own hf-hub download path); `metal` forwards to `inkentry-embed`'s
`metal` feature. `inkentry-embed` gates candle/tokenizers behind its own
default-on `native` feature. `inkentry-core` depends on it with
`default-features = false` to get only the `EmbeddingBackend` trait + `MODEL_ID`
(no candle): inkentry-cli only ever calls inference over HTTP via
`server_client.rs`, never constructs a `NativeEmbedder`, so it has no reason to
statically link candle. inkentry-server keeps `native` on, since it's the one
binary that actually constructs one.

### inkentry-embed (`crates/inkentry-embed/src/`)

```
lib.rs             - crate root; re-exports NativeEmbedder + DIM behind the default-on
                     `native` feature (candle/tokenizers gated with it); trait +
                     MODEL_ID stay available with `default-features = false`
embedder_native.rs — native embedder (F2LLM-v2-330M via candle, 896-dim, Metal/GPU on macOS).
                     NativeEmbedder::load_from_path(gguf, tokenizer, config) loads local files
                     already on disk with zero network access — the crate's only load entry
                     point, and it carries no download/fetch dependency. Implements
                     inkentry-core's EmbeddingBackend. inkentry-server's embed_hub module (above)
                     resolves those local files via the Hugging Face Hub before calling it.
```

---

## Inference Backend

All AI inference goes through **inkentry-server**. The CLI calls the server via
`ServerInferenceClient` in `crates/inkentry-cli/src/server_client.rs`: the only
place in inkentry-cli that issues AI inference requests. `ServerLlmAdapter` in
the same file is a thin `LlmBackend` trait adapter over an `Arc` of that client;
embedding is issued directly through the client's `embed_text`, so there is no
embed adapter. (There is no `ServerLlmClient` or `ServerEmbedClient` either;
those names are long gone.)

`inkentry-core` defines the `EmbeddingBackend` and `LlmBackend` traits
(`embeddings/mod.rs`, `llm/mod.rs`) but ships **no concrete implementations**.
The native embedding *engine* lives in the `inkentry-embed` crate
(`NativeEmbedder`, local-path load only); inkentry-server's `embed_hub` module
owns the Hugging Face Hub download path that resolves the (pre-quantized)
model artifacts before handing them to it. There is no external embedder
backend: embedding always runs through the bundled native engine. The LLM
backend (with its own external HTTP shim, `--llm-url`) lives in inkentry-server
(`server_llm.rs`). The endpoint, model and credential that shim runs on are
resolved client-side by `inkentry-cli`'s `cli/cmd/daemon_llm.rs` and handed to
the spawned daemon: url and model in argv, the credential in the child
environment only, because the detached daemon must never open the keychain
itself.

`capability/` probes server availability at startup and exposes a `Tier`
enum so commands degrade gracefully when no server is configured.

**Embed routing and LLM routing are separate rules and can resolve to different
servers in one command.** Embedding uses `Config::resolve_inference_url` plus
`capability::get_inference_tier`, unchanged: under the default `local_first`
mode it prefers the local tier even when `server_url` is set. LLM inference uses
`capability::resolve_llm_route` (`capability/llm_route.rs`), which never
consults `resolve_inference_url`. Its order is: explicit offline gives nothing
and probes nothing; a local tier advertising `llm.complete` wins; a set
`llm_url` whose local server does not serve an LLM **stops** rather than falling
through to the remote (the privacy guard, which by construction does not apply
in `cloud_first`, where the inference tier already is `server_url`); otherwise an
LLM-capable `server_url`; otherwise nothing. `Capabilities.llm_complete`
(`capability/state.rs`) is the availability signal, parsed from `/v1/health`.
Keying on a coarser capability would misfire across version skew, since a server
can advertise legacy capabilities without serving `/llm/complete`. A call site
needing both concerns builds two clients: see
`cli/cmd/memory/harvest.rs::harvest_clients` for the shape. The
user-facing text for every no-LLM outcome comes from
`capability::no_llm_message`, never from an ad-hoc string at the call site.

**Inference vs. memory storage are separate concerns.** Reaching the server for inference does **not** mean memory is stored there. For an auto-discovered loopback server, memory CRUD (`add`, `list`, `search`, `timeline`, `context`, `harvest`, `read-memory`) resolves to the project's local `memory.db`; the server is used only to embed the query for `memory search`, with the vector KNN run locally against `memory.db`. Memory lives on a server **only** when an explicit team `server_url` is configured with `mode = "cloud_first"`; under the default `local_first` mode, reads and writes stay in `memory.db` and the server is a converging replica. See `docs/adr/004-unified-memory-storage.md` and the sync-mode table in `crates/inkentry-core/src/config/sync_mode.rs`.

---

## Key Design Decisions

### Chunking strategy
Tree-sitter extracts **named semantic nodes** (functions, structs, impls, etc.)
rather than naive line splits. A token-aware sliding window is the fallback
for unsupported file types and for re-windowing oversized semantic nodes:
whole lines accumulate up to `MAX_CHUNK_TOKENS` (512) with ~12.5% token
overlap between windows, and the source node's `name`/`docstring`/
`parent_scope` are copied onto every window it produces (so a re-windowed
function still embeds with its symbol name instead of `title: none`). Markdown
uses ATX heading-based chunking (each `# Heading` + body = one
`ChunkKind::Section`).

### Embedding input format
F2LLM-v2-330M (Qwen3 decoder, 896-dim) uses:
- **Documents:** raw text — `title: {name | "none"} | text: {content}` (no instruction prefix)
- **Queries:** `Instruct: <instruction>\nQuery: {q}`
  - Code search: `Instruct: Given a code search query, retrieve the relevant code snippets\nQuery: {q}`
  - Memory/QA: `Instruct: Given a question, retrieve passages that answer the question\nQuery: {q}`

Document format is produced by `Chunk::embedding_text()` in
`crates/inkentry-core/src/indexer/chunker.rs`. Query prefixes are applied by
`handlers.rs` (server-side code search), `embed_query_vec()` in `helpers.rs`
(the CLI-side memory/QA embed), and `embed_cmd.rs` (plumbing embed --query).
Unified `search` embeds the one query under **both** prefixes — the code prefix
(server `/search`) and the QA prefix (`embed_query_vec`) — and fuses the two
per-corpus ranked lists by reciprocal rank fusion (ADR-081); the second embed is
elided when `--only-code`/`--only-memory`/`--only-text` makes it redundant.

### SQLite + sqlite-vec
No separate vector DB. The sqlite-vec extension adds a `vec0` virtual table
for KNN queries. The extension is registered via `sqlite3_auto_extension`
before any connection is opened (see `crates/inkentry-cli/src/main.rs` and
`crates/inkentry-server/src/main.rs`).

Chunk embeddings are stored as `INT8[896]` (F2LLM vectors are
L2-normalised, so int8 is lossless enough for ranking and ~4x smaller on disk);
the int8 L2 distance is rescaled back to the f32 scale by `INT8_SCALE` on read
(`storage/search.rs`). Memory-entry embeddings stay
`FLOAT[896]`.

Neither store migrates. Each declares its final shape in one schema file and
stamps `PRAGMA user_version`; anything else is refused or rebuilt, never
converted. `memory.db` **refuses** an earlier product's store and points at
`inkentry import`, because its rows are authored. `index.db` **rebuilds** —
discarding the old file and recreating it empty, carrying only `usage` across —
because everything else in it is derived from the source tree and `inkentry
index` reproduces it. Both constants sit above the highest version their old
ladders ever stamped, so a store from an older build reads as older rather than
as one from the future.

### Incremental indexing
Each file is hashed with blake3. On re-index, unchanged files are skipped.
Changed files: delete old chunks + embeddings, reparse, re-embed.

### Multi-project registry
`~/.config/inkentry/registry.db` tracks all indexed projects and their
dependency links. `inkentry search` automatically queries all linked project DBs
and merges results by rank. Additionally, `inkentry search` (its memory corpus),
`inkentry memory list`, and `inkentry context` surface `locked`- or
`cross-project`-tagged `decision` and `requirement` entries from linked
projects' memory stores (ADR-003). Each cross-project result is tagged with its
source project so decisions remain attributable. Pass `--local-only` to any of
these commands to query only the primary project's memory. See
`docs/memory.md#cross-project-visibility`.

### Secret scanning
`crates/inkentry-core/src/indexer/secrets.rs` runs before each chunk is stored, scanning the full
text that will be persisted and embedded (docstring + content; each composed structural summary is scanned
separately when generated, since summaries don't exist yet at store time and the
composition can lift a salient literal out of the code). Chunks matching known
credential patterns (AWS keys, PEM headers, GitHub PATs, etc.) are silently dropped — including
their docstring, so nothing lands in stored metadata either — and a warning naming only the
symbol is logged; a secret-bearing summary is replaced with an empty string instead of being
stored. **This is best-effort defense-in-depth, not a security boundary**: a finite regex list
cannot catch every credential format. The real boundary is that code never leaves the local
machine unless a team `server_url` is explicitly configured (see above); the scanner only reduces
the chance of a credential being embedded/stored (and, on that explicit-server path, transmitted)
by accident. That boundary is enforced by `crates/inkentry-cli/tests/egress_containment.rs`, which
traps every outbound connection across local-tier CLI flows (`init`, `index`, `search`, `memory`,
`plumbing graph-edges`, plumbing) and fails loudly, naming the destination, on any escape past loopback.

---

## Supported Languages

Rust, Go, Python, TypeScript, JavaScript, JSX, TSX, Java, C, C++, PHP, Ruby,
C#, Swift, Kotlin, JSON, HTML, CSS, HCL, Proto, SQL, Markdown, plain text,
Jupyter notebooks (`notebook`, `.ipynb`).

That is the set a default `cargo build` parses; `notebook` is in it and not
behind a feature, being a custom JSON parser rather than a tree-sitter grammar
or a binary-document reader. `SUPPORTED_LANGUAGES`
(`crates/inkentry-core/src/indexer/parser/mod.rs`) is `cfg`-gated, so the
optional `rich-formats` feature adds `docx`, `spreadsheet` and `pdf` to it, and
`inkentry languages` prints whichever set the running binary was built with.
Release binaries are built with `rich-formats` (see `.github/workflows/release.yml`),
so a released `inkentry languages` lists all three; a default source build does
not. Compare output across builds accordingly.

---

## Code Comment Conventions

Default to no comment. Add one only when it earns its place.

- **Why, never what.** A comment states something the code itself cannot: a
  hidden constraint, a non-obvious invariant, the specific bug a piece of
  logic guards against, or the reason one approach was chosen over another.
  If a comment only restates the next line in English, delete it.

  ```rust
  // Bad: restates the line below
  // Insert the row into the notes table.
  self.conn.execute("INSERT INTO notes ...", params)?;

  // Good: the invariant isn't visible from the code alone
  // Losers are deleted in id order; clearing their `superseded_by` first
  // avoids a live FK reference regardless of that order.
  self.clear_superseded_by(&loser_ids)?;
  ```

- **Self-documenting code first.** If a comment exists only to explain what a
  poorly-named variable, function, or type actually holds or does, rename it
  instead and delete the comment. Reach for a comment only after naming has
  been tried.

- **No `///`/`//!` doc-comments in test code.** Doc-comment syntax exists for
  rustdoc, which is never generated for `#[cfg(test)]` modules or `#[test]`
  functions — using it there is a category error, not a style choice. Use a
  plain `//` comment if a note is genuinely needed, but prefer a descriptive
  test name and clear assertions over a comment at all.

- **Even a real comment should be terse.** State the invariant or constraint
  directly; cut the surrounding narration, the "here's why we're telling you
  this" preamble, and any restated history. A comment that takes three
  sentences to say one thing is a candidate for a one-clause rewrite, not a
  trim.

---

## Common Commands

```bash
# Build all crates
cargo build
cargo build --release

# Build specific binaries
cargo build -p inkentry-cli
cargo build -p inkentry-server

# Run the CLI
cargo run -p inkentry-cli -- index ./some/project
cargo run -p inkentry-cli -- search "how does authentication work"
cargo run -p inkentry-cli -- status
cargo run -p inkentry-cli -- status --all
cargo run -p inkentry-cli -- search <symbol> --graph
cargo run -p inkentry-cli -- plumbing graph-edges --symbol <symbol>
cargo run -p inkentry-cli -- chunks src/some/file.rs
cargo run -p inkentry-cli -- languages
cargo run -p inkentry-cli -- sync              # two-way: push local memory to server, pull teammates' entries down
cargo run -p inkentry-cli -- plumbing push     # one-way: seed the server from local memory only

# Run the server
cargo run -p inkentry-server -- --port 4655

# Verbose logging
RUST_LOG=debug cargo run -p inkentry-cli -- index .

# Tests (all crates)
cargo test

# Tests for a specific crate
cargo test -p inkentry-core
cargo test -p inkentry-cli
cargo test -p inkentry-server

# Security audit (requires cargo-audit)
cargo audit
```

---

## Dependency Notes

- Tree-sitter grammars come from **`ast-grep-language`** (a single crate,
  ABI-aligned to the `tree-sitter` core). Bump that one crate instead of many
  `tree-sitter-*` deps. `proto` and `sql` are the only exceptions —
  ast-grep-language doesn't ship them, so they stay on the standalone
  `tree-sitter-proto` / `tree-sitter-sequel` crates. If you bump the
  `tree-sitter` core, check that `ast-grep-language` (and the two standalone
  grammars) still resolve to the same runtime line. (The `ast-grep-core`
  structural-search engine is gone: `search` degrades to full-text, never a
  working-tree scan.)
- `sqlite-vec` is loaded at runtime via `sqlite3_auto_extension` (see
  `crates/inkentry-cli/src/main.rs` and `crates/inkentry-server/src/main.rs`).
  The extension binary is bundled by the crate — no system install needed.
- `regex` is used only by `crates/inkentry-core/src/indexer/secrets.rs`. Patterns
  are compiled once via `OnceLock` at the start of `inkentry index`.
- `ignore` respects `.gitignore`, `.ignore`, and global gitignore rules during
  file traversal. Sensitive file patterns (`.env*`, `*.pem`, etc.) are
  excluded unconditionally via `OverrideBuilder`.
- Shared dependency versions are declared in the workspace root `Cargo.toml`
  under `[workspace.dependencies]`. Individual crates inherit them with
  `{ workspace = true }` — bump versions there, not in each crate's `Cargo.toml`.
