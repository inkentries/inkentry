# inkentry

[![CI](https://github.com/inkentries/inkentry/actions/workflows/ci.yml/badge.svg)](https://github.com/inkentries/inkentry/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust edition 2024](https://img.shields.io/badge/rust-2024-orange.svg)](https://doc.rust-lang.org/edition-guide/rust-2024/)

**Code intelligence for AI agents — zero infrastructure required.** Persistent memory, code graph, and search that work straight from the CLI.

```bash
inkentry graph validate_token                       # trace callers, callees, imports
inkentry search "error handling" --mode text        # full-text search, no server needed
inkentry memory add --kind decision --title "Chose sqlite-vec" --body "..."  # persistent across sessions
```

Semantic search works out of the box: `inkentry` autostarts a local `inkentry-server` that bundles a native embedder — no external inference server to run. Point everyone at a shared `inkentry-server` to share memory across a team.

## Quick start

**1. Install**

```bash
curl -fsSL https://raw.githubusercontent.com/spelunk-cloud/spelunk/refs/heads/main/install.sh | sh
```

> Also available via Homebrew (`brew install spelunk-cloud/spelunk/spelunk`), a
> Debian `.deb`, or a tarball from the
> [releases page](https://github.com/spelunk-cloud/spelunk/releases). See
> [Getting Started](docs/getting-started.md) for all install paths.

**2. Use it immediately — no setup required**

From inside any git repository:

```bash
inkentry graph validate_token                       # trace callers and callees
inkentry search "error handling" --mode text        # full-text search
inkentry memory add --kind decision \
  --title "Chose token bucket for rate limiting" \
  --body "Simpler than sliding window; sufficient for <1k RPS"
inkentry memory list --kind decision
inkentry context                                    # agent session entry point
```

**3. Add semantic search**

`inkentry init` indexes your project and starts the bundled server, so semantic
search works with no extra setup:

```bash
inkentry init                                       # index + autostart server in one step
inkentry search "error handling in the HTTP layer"  # semantic search
inkentry search "database migrations" --graph       # with callers/callees
```

## Why inkentry?

AI coding agents lose context between sessions and can't trace how code connects across files. inkentry solves both with zero infrastructure.

- **Persistent memory** — store decisions, requirements, and context in git notes. Retrieve them next session, or share them via a server with your team.
- **Code graph** — trace callers, callees, and imports across file boundaries without reading every file.
- **Works without any server** — memory, code graph, and full-text/structural (ast-grep) search work with just the binary. No API keys, no configuration.
- **Semantic search built in** — a local `inkentry-server` is autostarted on demand with a bundled native embedder (codefuse-ai/F2LLM-v2-330M, 896-dim, GPU-accelerated on macOS); no external inference server required. You can still point inkentry at your own OpenAI-compatible endpoint (LM Studio, Ollama, vLLM) if you prefer.
- **100% local** — your code never leaves your machine. The server is self-hosted (local by default). This claim is enforced, not just asserted: `crates/inkentry-cli/tests/egress_containment.rs` traps every outbound connection across the local-tier command surface and fails loudly, naming the destination, on any escape past loopback.
- **Agent-native** — JSON output (`AGENT=true`), git hooks, and a structured memory system built for the agent workflow loop.

### When to use inkentry vs grep

| You want to... | Use |
|---|---|
| Find an exact function name | `rg "fn validate_token"` |
| Find code related to a concept | `inkentry search "request authentication"` |
| See what calls a function | `inkentry graph validate_token` |
| Remember why a decision was made | `inkentry memory search "why sqlite-vec"` |
| Store a design decision for future sessions | `inkentry memory add --kind decision ...` |
| Share context across a team | `inkentry-server` + `server_url` |

## Core features

### Project memory

Store decisions, requirements, and context that persist across sessions — in git notes, no server needed:

```bash
inkentry memory add --kind decision --title "Chose sqlite-vec over pgvector" \
  --body "Must run without a Postgres server. Revisit if we need filtering + ANN."
inkentry memory list --kind decision --limit 10
inkentry memory search "why did we choose this database"
inkentry memory harvest   # auto-extract decisions from recent commits (server with LLM backend)
inkentry sync             # two-way sync of local memory with the configured server (push + pull)
```

Memory is stored in local SQLite and written through to git notes by default
(`store_in_git_notes`), so it travels with the repo. Set `server_url` to share
across a team.

### Code graph

```bash
inkentry graph RagPipeline                        # all edges for a symbol
inkentry graph src/storage/db.rs --kind imports   # imports in a file
```

inkentry extracts import, call, extends, and implements edges from the AST. No index or server needed.

### Search

```bash
inkentry search "handleRequest" --mode text       # full-text, no server needed
inkentry search "how are errors propagated"       # semantic (requires server + index)
inkentry search "auth middleware" --graph         # expand with 1-hop callers/callees
inkentry search "request handling" --budget 4000  # fit results within a token budget
```

### Multi-hop exploration (run the loop yourself)

There is no `explore` command. inkentry retrieves context; your agent reasons over
it. For a question that needs tracing across files, loop over the primitives
yourself — `search` (add `--graph`), `graph <symbol>`, `chunks <file>` — refining
the query each pass. See the "Exploring: multi-hop retrieval" section of
[`SKILL.md`](SKILL.md).

### Multi-project search

```bash
inkentry link ../shared-utils
inkentry search "connection pooling"   # searches both projects, merges by relevance
```

### Agent integration

Set `AGENT=true` for JSON output on every command:

```bash
AGENT=true inkentry memory list --kind decision
AGENT=true inkentry graph validate_token
AGENT=true inkentry search "auth flow" | jq '.[0].file_path'
```

Install git hooks to auto-harvest memory on every commit:

```bash
inkentry hooks install
```

inkentry ships with a [Claude Code skill](SKILL.md) and [agent guide](docs/agent-guide.md) for integration with AI coding agents.

## Supported languages

Tree-sitter AST-aware chunking for: **Rust**, **Go**, **Python**, **TypeScript**, **JavaScript**, **JSX**, **TSX**, **Java**, **C**, **C++**, **PHP**, **Ruby**, **C#**, **Swift**, **Kotlin**, **JSON**, **HTML**, **CSS**, **HCL**, **Proto**, **SQL**, **Markdown**.

All other file types are indexed as plain text with a sliding-window chunker.

## Documentation

Start at the **[documentation home](docs/README.md)**, which walks the path from
the first five minutes to running a shared memory server for a team.

- [Getting Started](docs/getting-started.md): install, index your first project, run your first retrieval
- [Memory](docs/memory.md): decisions, context, and requirements across sessions
- [Agent Guide](docs/agent-guide.md): wiring inkentry into AI coding agents
- [Commands](docs/commands.md): full reference for every subcommand
- [Stability contract](docs/stability.md): which surfaces semver freezes, and which are free to change
- [Architecture](docs/architecture.md): system design for contributors
- [Examples](docs/examples/): real-world workflows

## Repository structure

This is a Cargo workspace with three crates:

| Crate | Path | Purpose |
|---|---|---|
| `inkentry-core` | `crates/inkentry-core` | Library — storage, indexer, embeddings, LLM, search, config, registry |
| `inkentry-cli` | `crates/inkentry-cli` | `inkentry` binary — CLI commands; depends on `inkentry-core` |
| `inkentry-server` | `crates/inkentry-server` | `inkentry-server` binary + lib — shared memory server; depends on `inkentry-core` |

```bash
cargo build -p inkentry-cli    # build the CLI
cargo build -p inkentry-server # build the server
cargo test                    # test all crates
```

## Contributing

Contributions welcome — see [CONTRIBUTING.md](CONTRIBUTING.md) for the
workflow and [Building from source](docs/building.md) for setup instructions.
Supported platforms and host requirements are listed in
[docs/support.md](docs/support.md).

## License

[MIT](LICENSE)

inkentry-server bundles a third-party embedding model (Apache-2.0). See
[Model attribution](docs/model-attribution.md) for licensing, or
[Third-party models](docs/third-party-models.md) for configuring an external
LLM or embedding endpoint.
