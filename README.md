# inkentry

[![CI](https://github.com/inkentries/inkentry/actions/workflows/ci.yml/badge.svg)](https://github.com/inkentries/inkentry/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust edition 2024](https://img.shields.io/badge/rust-2024-orange.svg)](https://doc.rust-lang.org/edition-guide/rust-2024/)

**git tracks what changed. inkentry remembers why.**

inkentry is your project's memory. It works in the background, keeping the
decisions that shape your codebase next to the code they shaped, and hands them
back at the moment they matter. Built for people and for the agents working
alongside them.

```bash
inkentry search "why does retry wait 800ms"         # the code, and the decision behind it
inkentry search "validate_token" --graph            # a result plus its callers and callees
inkentry memory add --kind decision \
  --title "Chose sqlite-vec" --body "..."           # a decision, written down as it is made
```

Memory is stored in git notes, so it clones with the repository and travels with
your code. Semantic search runs from a local `inkentry-server` that inkentry
starts on demand and that bundles a native embedder, so there is no external
inference server to run. Point a team at a shared server to share decisions
across everyone's clones.

## Quick start

**1. Install**

```bash
curl -fsSL https://get.inkentry.com/install.sh | sh
```

> Also available via Homebrew (`brew install inkentries/inkentry/inkentry`,
> then `brew trust inkentries/inkentry` so upgrades keep working), a
> Debian `.deb`, or a tarball from the
> [releases page](https://github.com/inkentries/inkentry/releases). See
> [Getting Started](docs/getting-started.md) for all install paths.

**2. Initialise the project**

From the root of your git repository:

```bash
inkentry init                                       # index + autostart server in one step
```

`inkentry init` indexes your project and starts the bundled server, so semantic
search works with no extra setup. Full-text results are available as soon as the
tree is parsed, while semantic ranking builds in the background.

**3. Use it**

```bash
inkentry search "error handling in the HTTP layer"  # code and memory, best available ranking
inkentry search "validate_token" --graph            # with callers and callees
inkentry search "error handling" --only-text        # full-text search, no server needed
inkentry memory add --kind decision \
  --title "Chose token bucket for rate limiting" \
  --body "Simpler than sliding window; sufficient for <1k RPS"
inkentry memory list --kind decision
inkentry context                                    # agent session entry point
```

## Why inkentry?

AI coding agents lose context between sessions. The reasoning behind a piece of
code, why the retry waits 800ms, why sqlite-vec over pgvector, is rarely written
in the code itself, so it gets re-derived every session or lost for good.
inkentry keeps that why-layer beside the code and hands it back on demand.

- **Memory that travels with the repo** — decisions, requirements, and context are stored in git notes, so they clone with the repository and reach your teammates the way code already does. Retrieve them next session with `inkentry context` or `inkentry search`.
- **One search, both layers** — a single `search` interleaves code chunks and the decisions behind them into one ranked list. Add `--graph` and each result brings its callers and callees, so you trace how code connects without opening every file.
- **Runs on your machine** — full-text search, memory, and the call graph work with just the binary and a local index (`inkentry init`). No API keys, no configuration, no server to operate.
- **Semantic search built in** — a local `inkentry-server` starts on demand with a bundled native embedder (codefuse-ai/F2LLM-v2-330M, 896-dim, GPU-accelerated on macOS), so there is no external inference server to run. You can still point inkentry at your own OpenAI-compatible endpoint (LM Studio, Ollama, vLLM) if you prefer.
- **Your code stays local** — your code never leaves your machine; only memory does, and only when you point at a team server. The server is self-hosted and binds to loopback by default. This is enforced, not just asserted: `crates/inkentry-cli/tests/egress_containment.rs` traps every outbound connection across the local-tier command surface and fails loudly, naming the destination, on any escape past loopback.
- **Agent-native** — JSON output (`AGENT=true`), git hooks, and a structured memory system built for the agent workflow loop.

### When to use inkentry vs grep

| You want to... | Use |
|---|---|
| Find an exact function name | `rg "fn validate_token"` |
| Find code related to a concept | `inkentry search "request authentication"` |
| See what calls a function | `inkentry search validate_token --graph` |
| Remember why a decision was made | `inkentry search "why sqlite-vec" --only-memory` |
| Store a design decision for future sessions | `inkentry memory add --kind decision ...` |
| Share context across a team | `inkentry-server` + `server_url` |

## Core features

### Project memory

Store decisions, requirements, and context that persist across sessions — in git notes, no server needed:

```bash
inkentry memory add --kind decision --title "Chose sqlite-vec over pgvector" \
  --body "Must run without a Postgres server. Revisit if we need filtering + ANN."
inkentry memory list --kind decision --limit 10
inkentry search "why did we choose this database" --only-memory
inkentry harvest          # auto-extract decisions from recent commits (server with LLM backend)
inkentry sync             # two-way sync of local memory with the configured server (push + pull)
```

Memory is stored in local SQLite and written through to git notes by default
(`store_in_git_notes`), so it stays with your clone. Sharing it over git is
opt-in: `inkentry init` sets up the fetch side, so teammates' entries reach you
automatically, but your own stay local until you run `inkentry hooks install
--pre-push` to publish them on `git push`. Set `server_url` instead to share
through a team server.

### Code graph

The call graph rides on search rather than standing alone: `--graph` expands a
result with its 1-hop neighbours, and `plumbing graph-edges` exposes the exact
edges underneath for scripts and agents.

```bash
inkentry search linearrag_search --graph                 # the symbol's chunk + its 1-hop neighbours
inkentry plumbing graph-edges --symbol linearrag_search  # exact edges as JSONL
inkentry plumbing graph-edges --file crates/inkentry-core/src/storage/db.rs   # every edge in a file
```

inkentry extracts import, call, extends, and implements edges from the AST at index time. No server needed.

The symbol and path above are real ones from this repository, so the commands
work as written once you have indexed a clone of it. `graph-edges --file` matches
the path as it is stored in the index, so pass the full repo-relative path rather
than a suffix.

### Search

One command over both corpora: code chunks and memory entries interleaved into a
single ranked list, with no mode to choose.

```bash
inkentry search "how are errors propagated"       # code and memory, best available ranking
inkentry search "handleRequest" --only-text       # full-text only, no server needed
inkentry search "auth middleware" --graph         # expand with 1-hop callers/callees
inkentry search "why sqlite-vec" --only-memory    # memory corpus only
```

`search` needs the index built by `inkentry init`; an uninitialised directory
funnels you there. With `--format json`/`jsonl` each result is an envelope
naming the corpus it came from: `{type, fused_rank, fused_score, corpus_rank,
code|memory}`.

### Multi-hop exploration (run the loop yourself)

There is no `explore` command. inkentry retrieves context; your agent reasons over
it. For a question that needs tracing across files, loop over the primitives
yourself — `search` (add `--graph`), `plumbing graph-edges --symbol <symbol>`,
`chunks <file>` — refining the query each pass. See the "Exploring: multi-hop
retrieval" section of [the skill](https://github.com/inkentries/agent-plugin/blob/main/skills/inkentry/SKILL.md).

### Multi-project search

```bash
inkentry link ../shared-utils
inkentry search "connection pooling"   # searches both projects, merges by relevance
```

### Agent integration

Set `AGENT=true` for JSON output on every command:

```bash
AGENT=true inkentry memory list --kind decision
AGENT=true inkentry plumbing graph-edges --symbol validate_token
AGENT=true inkentry search "auth flow" | jq -r '.[] | select(.type=="code") | .code.file_path'
```

Results interleave code and memory, so select on `.type` rather than indexing
into the array: `.[0]` is just as likely to be a memory entry.

Install git hooks to auto-harvest memory on every commit:

```bash
inkentry hooks install
```

inkentry ships as an [Agent Plugins](https://agent-plugins.org/) plugin from [inkentries/agent-plugin](https://github.com/inkentries/agent-plugin), so the skill installs into Claude Code and any other client implementing the standard. [How to install it](docs/plugin.md), and the longer [agent guide](docs/agent-guide.md).

## Supported languages

Tree-sitter AST-aware chunking for: **Rust**, **Go**, **Python**, **TypeScript**, **JavaScript**, **JSX**, **TSX**, **Java**, **C**, **C++**, **PHP**, **Ruby**, **C#**, **Swift**, **Kotlin**, **JSON**, **HTML**, **CSS**, **HCL**, **Proto**, **SQL**.

Purpose-built chunkers, without tree-sitter, for **Markdown** (split on headings), **Jupyter notebooks** (`.ipynb`: one chunk per cell, markdown and code kept apart) and **plain text** (`.txt`, `.rst`, `.adoc`, `.asciidoc`, and extensionless `README`, `CHANGELOG` and siblings).

A file whose type is in neither group is skipped, not indexed as text.

**`inkentry languages` prints a build-dependent list.** The languages above are
the ones every build parses, notebooks included. The `rich-formats` feature,
on by default, adds **DOCX**, **spreadsheets** and **PDF** on top, and
`languages` lists those three as well. Every published release binary has it,
and so does a plain `cargo build` from source; only `--no-default-features`
leaves them out. See [building from source](docs/building.md).

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

This is a Cargo workspace with four crates:

| Crate | Path | Purpose |
|---|---|---|
| `inkentry-core` | `crates/inkentry-core` | Library — storage, indexer, embeddings, LLM, search, config, registry |
| `inkentry-cli` | `crates/inkentry-cli` | `inkentry` binary — CLI commands; depends on `inkentry-core` |
| `inkentry-embed` | `crates/inkentry-embed` | Library — native F2LLM-v2-330M embedder (candle); depends on `inkentry-core` |
| `inkentry-server` | `crates/inkentry-server` | `inkentry-server` binary + lib — shared memory server; depends on `inkentry-core` + `inkentry-embed` |

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
