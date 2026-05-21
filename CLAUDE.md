# CLAUDE.md — spelunk

Developer guide for AI agents (and humans) working on this codebase.

---

## Agent workflow — use spelunk on this codebase

This project is indexed with spelunk. Use it — don't just use Read/Grep/Glob.

**At the start of every session:**
```bash
spelunk check                                    # verify index is fresh
spelunk memory list --kind decision --limit 10   # review prior decisions
spelunk memory list --kind handoff --limit 3     # pick up where last session left off
spelunk memory list --kind question              # check open questions
```

**Before reading any file, search first:**
```bash
spelunk search "<topic>"          # find relevant chunks by meaning
spelunk graph <symbol>            # trace callers/callees when needed
```

spelunk retrieves context — you synthesise the answer.

**Store decisions as you make them** — don't wait until the end:
```bash
spelunk memory add --kind decision --title "..." --body "why, what alternatives, what breaks"
spelunk memory add --kind requirement --title "..." --body "..."   # when user states a constraint
spelunk memory add --kind note --title "..."                       # surprising/non-obvious facts
```

**At the end of every session:**
```bash
spelunk memory add --kind handoff --title "Handoff: <summary>" --body "what's done, what's next, open questions"
spelunk index .                   # re-index if project uses semantic search (hook does this on commit)
```

Full reference: `SKILL.md` and `docs/agent-guide.md`.

---

## What This Project Is

`spelunk` (`spelunk`) is a Rust CLI and context retrieval engine for AI agents.

**Core (no server required):** git-notes memory, full-text search, code graph (AST + call edges), tree-sitter chunking.

**With inference server** (any OpenAI-compatible endpoint, default `http://127.0.0.1:1234`): semantic search via embeddings (EmbeddingGemma 300M by default), `spelunk explore`, `spelunk memory harvest`, LLM summaries, `spelunk plan create`. Chat model is also optional and enables harvest/plan.

**With remote server** (`memory_server_url`): team-shared memory, `spelunk memory watch`, conflict detection.

You search with spelunk, then reason over the results yourself.

---

## Workspace Structure

This is a Cargo workspace with three crates:

```
Cargo.toml                    — workspace root; [workspace.dependencies] for shared versions

crates/
  spelunk-core/               — library: storage, indexer, embeddings, LLM, search, config, registry
  spelunk-cli/                — `spelunk` binary; depends on spelunk-core
  spelunk-server/             — `spelunk-server` binary + lib; depends on spelunk-core
```

## Module Map

### spelunk-core (`crates/spelunk-core/src/`)

```
lib.rs           — crate root; re-exports public modules
error.rs         — SpelunkError enum
config.rs        — Config struct; load from ~/.config/spelunk/config.toml
backends.rs      — re-exports ActiveEmbedder / ActiveLlm (LM Studio)
utils/
  mod.rs         — strip_ansi(), misc helpers
  dates.rs       — date parsing helpers
registry.rs      — global project registry (~/.config/spelunk/registry.db)

embeddings/
  mod.rs         — EmbeddingBackend trait, vec_to_blob/blob_to_vec helpers
  openai_compat.rs — OpenAiCompatEmbedder: calls /v1/embeddings

llm/
  mod.rs         — LlmBackend trait, Message struct, Token type
  openai_compat.rs — OpenAiCompatLlm: calls /v1/chat/completions (SSE streaming)

indexer/
  mod.rs         — re-exports Chunk, ChunkKind, SourceParser
  chunker.rs     — Chunk / ChunkKind structs; sliding_window fallback
  docparser.rs   — document-level parsing helpers
  pagerank.rs    — PageRank over the code graph
  pdf.rs         — PDF text extraction
  secrets.rs     — contains_secret(): regex scanner, drops credential chunks
  summariser.rs  — LLM-based chunk summarisation
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
  search.rs      — KNN search queries against sqlite-vec
  graph.rs       — graph_edges CRUD
  snapshots.rs   — snapshot save/restore
  specs.rs       — spec record CRUD
  stats.rs       — aggregate statistics queries
  note_record.rs — NoteRecord struct (memory entry)
  git_notes.rs   — git-notes read/write backend
  git_meta.rs    — git metadata helpers
  memory/
    mod.rs       — NoteStore: memory entries CRUD + list_filtered
    edges.rs     — memory relationship edges CRUD
    notes.rs     — note insert/fetch/delete
    search.rs    — memory FTS + semantic search
  backend.rs     — StorageBackend trait (local vs remote)
  remote.rs      — remote storage backend (HTTP)

search/
  mod.rs         — SearchResult struct
  rag.rs         — RagPipeline<E,L>: search + ask (dead code, kept for future)
  explore.rs     — interactive exploration pipeline
  tokens.rs      — token-budget helpers
  tools.rs       — tool-call helpers for LLM search

migrations/  (crates/spelunk-core/migrations/)
  001_initial.sql – 018_graph_edges_compound_idx.sql — incremental DB schema
```

### spelunk-cli (`crates/spelunk-cli/src/`)

```
main.rs          — entry point: parse CLI, dispatch to commands

cli/
  mod.rs         — clap structs (Cli, Command, *Args)
  cmd/
    mod.rs       — re-exports one pub fn per subcommand
    check.rs     — `spelunk check` handler
    context.rs   — `spelunk context` handler (agent session entry point)
    explore.rs   — `spelunk explore` handler
    graph.rs     — `spelunk graph` handler
    helpers.rs   — shared output / progress helpers
    hooks.rs     — `spelunk hooks` handler
    init.rs      — `spelunk init` handler
    link.rs      — `spelunk link/unlink/autoclean` handlers
    links.rs     — `spelunk links` handler
    misc.rs      — `spelunk chunks` / `spelunk languages` handlers
    search.rs    — `spelunk search` handler
    status.rs    — `spelunk status` handler
    ui.rs        — TUI helpers (private)
    index/
      mod.rs         — `spelunk index` entry point
      embed_phase.rs — embedding phase of indexing
      parse_phase.rs — parse/chunk phase of indexing
      summaries.rs   — AI summary generation during index
      worktree.rs    — git worktree handling for index
    memory/
      mod.rs         — `spelunk memory` dispatch
      add.rs         — memory add subcommand
      archive.rs     — memory archive subcommand
      graph_cmd.rs   — memory graph subcommand
      harvest.rs     — memory harvest (LLM extraction)
      list.rs        — memory list subcommand
      push.rs        — memory push subcommand
      search.rs      — memory search subcommand
      show.rs        — memory show subcommand
      supersede.rs   — memory supersede subcommand
      timeline.rs    — memory timeline subcommand
    plumbing/
      mod.rs         — PlumbingArgs/PlumbingCommand; dispatch; exit-2 on error
      cat_chunks.rs  — emit indexed chunks for a file as NDJSON
      embed_cmd.rs   — read stdin lines, emit embedding vectors as NDJSON
      graph_edges.rs — emit code graph edges as NDJSON
      hash_file.rs   — blake3 hash a file; check index currency
      knn.rs         — KNN vector search, NDJSON output
      ls_files.rs    — list indexed files as NDJSON; exit 1 if no results
      parse_file.rs  — parse a file and emit chunks as NDJSON (no DB write)
      read_memory.rs — emit memory entries as NDJSON
```

### spelunk-server (`crates/spelunk-server/src/`)

```
main.rs      — entry point: parse args, register sqlite-vec, start Axum server
lib.rs       — AppState, router, auth_middleware, AppError, ApiDoc (utoipa)
db.rs        — ServerDb: SQLite schema, memory CRUD, KNN search, embedding dim guard
handlers.rs  — Axum route handlers for all /v1/ endpoints

migrations/  (crates/spelunk-server/migrations/)
  server_001.sql — projects + server memory schema
  server_002.sql — server memory FTS
```

---

## Inference Backend

The only backend is the **OpenAI-compatible API** client (`backend-lmstudio`
feature flag, always enabled). Both `ActiveEmbedder` and `ActiveLlm` are
unconditional re-exports in `crates/spelunk-core/src/backends.rs`.

To add a new backend:
1. Add a feature flag in `crates/spelunk-core/Cargo.toml`
2. Implement `EmbeddingBackend` and `LlmBackend` in new submodule files under `spelunk-core`
3. Gate the re-exports in `crates/spelunk-core/src/backends.rs` behind the feature flag

Nothing outside `spelunk-core/src/embeddings/`, `spelunk-core/src/llm/`, and
`spelunk-core/src/backends.rs` should import a concrete backend type.

---

## Key Design Decisions

### Chunking strategy
Tree-sitter extracts **named semantic nodes** (functions, structs, impls, etc.)
rather than naive line splits. Sliding-window (120 lines, 15-line overlap) is
the fallback for unsupported file types. Markdown uses ATX heading-based
chunking (each `# Heading` + body = one `ChunkKind::Section`).

### Embedding input format
EmbeddingGemma's recommended document retrieval format:
```
title: {name | "none"} | text: {content}
```
Query-side prefix: `task: code retrieval | query: {q}`

See `Chunk::embedding_text()` in `crates/spelunk-core/src/indexer/chunker.rs`.

### SQLite + sqlite-vec
No separate vector DB. The sqlite-vec extension adds a `vec0` virtual table
for KNN queries. The extension is registered via `sqlite3_auto_extension`
before any connection is opened (see `crates/spelunk-cli/src/main.rs` and
`crates/spelunk-server/src/main.rs`).

### Incremental indexing
Each file is hashed with blake3. On re-index, unchanged files are skipped.
Changed files: delete old chunks + embeddings, reparse, re-embed.

### Multi-project registry
`~/.config/spelunk/registry.db` tracks all indexed projects and their
dependency links. `spelunk search` automatically queries all linked project DBs
and merges results by distance.

### Secret scanning
`crates/spelunk-core/src/indexer/secrets.rs` runs before each chunk is stored. Chunks matching
known credential patterns (AWS keys, PEM headers, GitHub PATs, etc.) are
silently dropped and a warning is logged — content is never echoed.

### Prompt structure
The ask prompt uses XML-style delimiters to separate untrusted RAG context
from the user's question, mitigating prompt injection:
```xml
<code_context>
{retrieved chunks}
</code_context>

<question>
{user question}
</question>
```

---

## Supported Languages

Rust, Go, Python, TypeScript, JavaScript, JSX, TSX, Java, C, C++, Ruby,
Swift, Kotlin, JSON, HTML, CSS, HCL, Proto, SQL, Markdown, plain text.

---

## Common Commands

```bash
# Build all crates
cargo build
cargo build --release

# Build specific binaries
cargo build -p spelunk-cli
cargo build -p spelunk-server

# Run the CLI
cargo run -p spelunk-cli -- index ./some/project
cargo run -p spelunk-cli -- search "how does authentication work"
cargo run -p spelunk-cli -- status
cargo run -p spelunk-cli -- status --all
cargo run -p spelunk-cli -- graph <symbol>
cargo run -p spelunk-cli -- chunks src/some/file.rs
cargo run -p spelunk-cli -- languages

# Run the server
cargo run -p spelunk-server -- --port 7777

# Verbose logging
RUST_LOG=debug cargo run -p spelunk-cli -- index .

# Tests (all crates)
cargo test

# Tests for a specific crate
cargo test -p spelunk-core
cargo test -p spelunk-cli
cargo test -p spelunk-server

# Security audit (requires cargo-audit)
cargo audit
```

---

## Dependency Notes

- Tree-sitter language crate versions must be compatible with the `tree-sitter`
  core. If you bump the core, check all `tree-sitter-*` crates too.
- `sqlite-vec` is loaded at runtime via `sqlite3_auto_extension` (see
  `crates/spelunk-cli/src/main.rs` and `crates/spelunk-server/src/main.rs`).
  The extension binary is bundled by the crate — no system install needed.
- `regex` is used only by `crates/spelunk-core/src/indexer/secrets.rs`. Patterns
  are compiled once via `OnceLock` at the start of `spelunk index`.
- `ignore` respects `.gitignore`, `.ignore`, and global gitignore rules during
  file traversal. Sensitive file patterns (`.env*`, `*.pem`, etc.) are
  excluded unconditionally via `OverrideBuilder`.
- Shared dependency versions are declared in the workspace root `Cargo.toml`
  under `[workspace.dependencies]`. Individual crates inherit them with
  `{ workspace = true }` — bump versions there, not in each crate's `Cargo.toml`.
