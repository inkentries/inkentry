# Architecture

This document describes inkentry's system design for contributors and anyone integrating with the codebase.

## Overview

inkentry is a Rust CLI that:

1. **Indexes** source trees using tree-sitter AST parsing
2. **Embeds** a subset of the resulting chunks through `inkentry-server`'s bundled embedder; the rest stay full-text only
3. **Stores** chunks, vectors, a full-text index and graph edges in SQLite
4. **Serves** search over code and memory, graph queries, and memory retrieval via CLI

```
Source tree --> Indexer (tree-sitter + chunker) --> index.db
                    |                               chunks, full-text index,
                    | chunk text                    embeddings, graph_edges
                    v
              inkentry-server (embedder)

Memory entries <--> memory.db <--> refs/notes/inkentry

index.db + memory.db --> Search / Graph / Memory --> CLI output (text / JSON)
```

## Module structure

The workspace has four crates. `CLAUDE.md` carries the file-level module map
and is kept current with the code.

```
crates/
  inkentry-core/     storage, indexer, search, config, metrics, registry
  inkentry-cli/      the `inkentry` binary: clap structs, one handler per
                     subcommand, the server client, capability probing
  inkentry-embed/    the F2LLM-v2-330M embedder (llama.cpp) and the
                     EmbeddingBackend trait
  inkentry-server/   the `inkentry-server` binary: HTTP API, embedder host,
                     team memory store
```

Within `inkentry-core`:

```
  config/       Config, sync mode, project id, secret store, caller declaration
  indexer/      parser, chunker, graph edges (tree-sitter + locals queries),
                secret scanner, structural summaries, embed scope
  storage/      index.db and memory.db access, migrations, git-notes carrier,
                remote memory backends
  search/       RRF constant, full-text query building, token budgets
  metrics/      state and events metrics over memory.db and git history
  registry.rs   global project registry (~/.config/inkentry/registry.db)
migrations/     SQL initial schemas and numbered forward steps
```

## Key design decisions

Architectural decisions are recorded in [docs/adr/](adr/). Key ones:

### Chunking: tree-sitter AST nodes, not line splits

Tree-sitter parses source code into an AST and inkentry extracts named semantic nodes (functions, structs, classes, methods, traits, impls) as individual chunks. This means each chunk is a meaningful unit of code with a name, type, and scope — not an arbitrary 100-line window.

Fallback: a token-aware sliding window for unsupported languages and for oversized semantic nodes that need re-windowing. Each window accumulates whole lines up to `MAX_CHUNK_TOKENS` (512), with ~12.5% token overlap between adjacent windows (the ratio behind the historical 120-line/15-line-overlap split); a single line that alone exceeds the budget becomes its own window so the cap always binds. Re-windowed chunks carry the source node's `name`/`docstring`/`parent_scope` so they still embed with their symbol identity rather than `title: none`. Markdown uses heading-based chunking.

### Storage: SQLite + sqlite-vec, nothing else

All data lives in a single SQLite file per project. The sqlite-vec extension adds a `vec0` virtual table for KNN vector search. No separate vector database, no separate search engine.

This is a deliberate constraint — see [ADR-001](adr/001-scope-boundaries.md). SQLite is zero-configuration, single-file, and sufficient for the scale inkentry targets.

### Incremental indexing via blake3

Each file is hashed with blake3. On re-index, unchanged files are skipped entirely. Changed files get their old chunks and embeddings deleted, then re-parsed and re-embedded.

### Embedding format

Chunks are embedded with **codefuse-ai/F2LLM-v2-330M** (Qwen3 decoder, 896-dim),
served by `inkentry-server` via the llama.cpp engine (Metal GPU on macOS, Vulkan
GPU on Windows/Linux, CPU elsewhere). Documents use the format:
```
title: {name | "none"} | text: {content}
```

Queries use an instruction prefix: `Instruct: {instruction}\nQuery: {q}`. For
example, code search uses `Instruct: Given a code search query, retrieve the
relevant code snippets\nQuery: {q}`.

See `Chunk::embedding_text()` in `crates/inkentry-core/src/indexer/chunker.rs`.

Vectors are L2-normalised and stored as sqlite-vec `INT8[896]` (chunk
embeddings); memory-entry embeddings stay `FLOAT[896]`.

#### Why two vector-storage formats (int8 for chunks, float for memory)

This split is **deliberate**, not an oversight. The two vector tables are sized
for different jobs:

| Table | Type | Rationale |
| --- | --- | --- |
| `embeddings` (chunks) | `INT8[896]` | The code index scales with the corpus — thousands to millions of chunks. int8 scalar quantisation is 4× smaller on disk and, because F2LLM vectors are L2-normalised, lossless enough for ranking. The int8 L2 distance comes back ~127× the f32 distance, so the search path rescales by `embeddings::INT8_SCALE` on read. |
| `note_embeddings` (memory) | `FLOAT[896]` | The memory-note table is tiny (tens to low-thousands of rows per project), so the int8 footprint win is negligible. Keeping full-precision f32 avoids the int8 quantise-on-write + distance-rescale-on-read nuance for a table that never grows large, and keeps the memory insert/search path (`MemoryStore::insert_embedding` / `search`, fed by `embeddings::vec_to_blob`) free of `vec_int8(...)` wrapping and `INT8_SCALE` division. |

Concretely, the two paths never mix:
- **int8 path** — `Database::{insert_embedding,search_similar}` goes through
  `embeddings::vec_to_int8_blob` + `vec_int8(?)` on write and divides the raw
  distance by `embeddings::INT8_SCALE` on read (`storage/db.rs`,
  `storage/search.rs`).
- **float path** — memory notes go through `embeddings::vec_to_blob` (raw
  little-endian f32) into `MemoryStore::insert_embedding`, and `MemoryStore::search`
  matches on the raw f32 query blob with no rescale
  (`storage/memory/notes.rs`, `storage/memory/search.rs`). The server-side memory
  store (`inkentry-server/src/db.rs`) mirrors this float layout.

If memory ever grows to corpus scale, migrating `note_embeddings` to int8 would
be the obvious follow-up — but until then the int8 cost (a second quantised path
to maintain, plus a forced memory re-embed/re-harvest on migration) buys nothing.

There is no dimension-upgrade path in either store. Each store has a frozen
initial schema file and stamps `PRAGMA user_version`; a fresh store is created
from that file and climbs the numbered migration steps to the current version,
the same road an existing store takes. Below the version the previous
ladder last stamped, `memory.db` refuses the file and points at `inkentry
import`, and `index.db` discards and rebuilds, carrying only `usage`. Above it,
both migrate forward in place, except that an `index.db` step may ask to
rebuild instead when it invalidates stored data outright, such as a different
embedding space. A vector table from a store this build cannot read therefore
never reaches a read path. See [Stability](stability.md) for the contract.

### Backend abstraction

The `EmbeddingBackend` and `LlmBackend` traits are the only interface between inkentry and inference. `LlmBackend` is defined in inkentry-core (`llm/`); `EmbeddingBackend` is defined in `inkentry-embed` (`backend.rs`) and re-exported by inkentry-core's `embeddings/` at its historical path. inkentry-core ships **no** concrete implementations. The F2LLM embedder engine lives in its own `inkentry-embed` library crate (`crates/inkentry-embed/src/embedder_llama.rs`, `LlamaEmbedder`), which only loads the model from local files already on disk (`LlamaEmbedder::load_from_path`) and carries no download dependency. `inkentry-embed` is a leaf crate: inkentry-core depends on it, not the reverse. `inkentry-server` depends on it too, owns the Hugging Face Hub download path that resolves those local files (`crates/inkentry-server/src/embed_hub/`), and additionally provides the OpenAI-compatible HTTP clients. The CLI reaches inference only through `ServerInferenceClient` in `crates/inkentry-cli/src/server_client.rs`, calling its methods directly — there is no trait adapter on the CLI side. Embedding and LLM inference are routed by separate rules and can resolve to different servers in a single command, so a caller needing both builds two clients; the LLM rule lives in `crates/inkentry-cli/src/capability/llm_route.rs`.

To add a new backend: implement the trait (in `inkentry-embed` for an embedder, or in inkentry-server for an LLM/HTTP backend) and wire it into the server's endpoint handlers. Nothing in inkentry-core imports a concrete backend.

### Secret scanning

`crates/inkentry-core/src/indexer/secrets.rs` runs regex patterns against the full text that will be persisted and embedded for each chunk (docstring + content) before storage, and separately against each composed structural summary when it is produced (summaries don't exist yet at chunk-store time, and the composition can pull a salient literal out of the code into the summary). Chunks matching known credential patterns (AWS keys, PEM headers, GitHub PATs, etc.) are silently dropped in full — including their docstring — and a warning naming only the symbol is logged; a secret-bearing summary is stored as an empty string instead.

This scanner is **best-effort defense-in-depth, not a security boundary** — a finite set of regexes cannot catch every credential format. The actual boundary is that code never leaves the local machine unless a team `server_url` is explicitly configured; the scanner only reduces the chance of a credential being embedded/stored (and, on that explicit-server path, transmitted) by accident. This boundary is enforced by `crates/inkentry-cli/tests/egress_containment.rs`, which traps every outbound connection across local-tier CLI flows and fails loudly, naming the destination, on any escape past loopback.

### Multi-project registry

`~/.config/inkentry/registry.db` tracks all indexed projects. `inkentry link` connects projects so that `inkentry search` queries multiple databases and merges their code results by distance, before the code and memory corpora are fused by rank (ADR-081).

## Data flow: index

```
files on disk
  → SourceParser (tree-sitter AST → Chunk[])
  → SecretScanner (drop credential chunks)
  → Database.store(chunks; each marked text-only or embeddable)
  → EmbeddingBackend.embed(batch of embeddable chunk texts)
  → Database.store(embeddings)
  → EdgeExtractor (AST → graph_edges; callees bound in-file via locals.scm → target_file)
  → Database.store(edges)
```

Every stored chunk lands in the full-text index. Only chunks that are not
text-only (see ADR-104) are embedded.

## Data flow: search

```
query string
  → [code corpus]   vector KNN over embeddings (sqlite-vec)  ┐
                    full-text match over chunks_fts          ┘ fused by RRF
  → [memory corpus] vector KNN over note_embeddings, gated on relevance;
                    full-text match over memory
  → [optional] 1-hop graph expansion; linked project DBs via registry
  → code and memory lists fused by RRF into one ranked list
  → return the result envelope
```

`--only-text` skips the embedding step and the vector lists.

## Adding a new language

1. Enable the grammar from `ast-grep-language`; add a standalone `tree-sitter-{lang}` crate only if it does not ship one
2. Register the language in `crates/inkentry-core/src/indexer/parser/mod.rs` (`detect_language` + `SUPPORTED_LANGUAGES`)
3. Add extraction patterns in `crates/inkentry-core/src/indexer/graph/edges.rs` for graph edge support, and a `locals.scm` under `graph/queries/` to bind calls within a file
4. Add tests
