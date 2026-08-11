# Agent Guide

`inkentry` is designed to work as infrastructure for AI coding agents, not just as a human developer tool. This guide covers the patterns that make agents most effective when paired with `inkentry`.

**The key mental model**: inkentry retrieves context; you reason over it. Use `inkentry search` — with `--graph` to pull in call-graph neighbours — to find the right code, read the results, then synthesise the answer yourself. inkentry is a persistent memory store and code navigation tool, not an oracle.

**What's built-in:** memory (local SQLite `memory.db`, optionally mirrored to git-notes), code graph, full-text search, and extracted conventions work with just the CLI binary — no server needed. A project's memory always lives in its local `memory.db`; that is the canonical store of record for every memory command.

**What's server-backed:** semantic/hybrid search (the default `inkentry search` ranking) and `inkentry harvest` use `inkentry-server` for **inference** (embeddings + LLM). From v0.8.0 the server is autostarted locally on demand and bundles a native embedder (codefuse-ai/F2LLM-v2-330M, 896-dim, GPU-accelerated on macOS via candle) — there is no external embedding server to run by default. The auto-discovered loopback server is **inference-only**: it never stores memory. For the memory corpus of `inkentry search` the CLI sends only the query to the loopback embedder and runs the vector search locally against `memory.db` — note text never leaves the local store. If you force offline mode (`INKENTRY_NO_SERVER=1`), these commands fall back to full-text search or error clearly, and all memory commands operate on `memory.db`.

**Where does memory live?** Always `memory.db` for the active project — **unless** you have *explicitly* configured a team `server_url`, which relocates the store of record to that shared server (the team-memory tier). An auto-discovered loopback server does **not** change where memory lives.

## The core loop

A productive agentic session with `inkentry` looks like this:

1. **Orient** — read memory and bring the index up to date (`inkentry context`, then `inkentry index .` — idempotent and blake3-gated, so it is a no-op when nothing changed)
2. **Search** — find the relevant code before reading or editing it
3. **Execute** — make code changes, delegating sub-tasks as needed
4. **Verify** — re-check the call graph and re-index after changes
5. **Codify** — store decisions, handoffs, and context in memory

This loop compounds: each session leaves better context for the next, whether that's the same agent resuming or a different one picking up.

## Machine-readable output

Set `AGENT=true` and every `inkentry` command returns JSON:

```bash
export AGENT=true

inkentry search "error handling"          # → interleaved code + memory results (JSON envelopes)
inkentry status                           # → { files, chunks, embeddings, ... }
inkentry memory list                      # → JSON array of notes
inkentry search "auth decisions" --only-memory   # → memory notes with distance scores
```

You can also use `--format json` on individual commands.

## Managing the local server daemon

If your config does not have a `server_url`, `inkentry` auto-discovers a local
`inkentry-server` running on loopback by reading
`~/.local/state/inkentry/server.port`.  You can start, stop, and inspect that
daemon with the `inkentry server` subcommand. This auto-discovered daemon is an **inference backend only** — it serves embeddings and LLM calls. It is **not** a memory store: your project's memory stays in `memory.db` regardless of whether this server is running. (Memory moves to a server only when you *explicitly* set `server_url` to a team instance in your config.)

```bash
# Start inkentry-server on port 7777 (idempotent — no-op if already running)
inkentry server start

# Check whether the daemon is running and get its PID/port/version
inkentry server status

# Tail the last 50 lines of the server log
inkentry server logs

# Stop the daemon gracefully (SIGTERM; waits up to 10 s)
inkentry server stop
```

**State directory:** all runtime files (`server.pid`, `server.port`,
`server.log`) live under `~/.local/state/inkentry/`.

**Idempotency:** `inkentry server start` is safe to call at the beginning of
every session.  If the daemon is already running and healthy it exits 0
immediately.  If the PID is stale (process dead), it starts a fresh instance.

**When to use `status` vs probing `/v1/health` directly:** use
`inkentry server status` for the daemon's health (running state, PID, port,
version, and reachability) — it is the human-readable probe you want during
debugging, so you rarely need to poll `/v1/health` directly.

**Port walk:** `start` tries ports 7777–7787 in order.  If all are taken it
exits with a clear error.  Use `--port <n>` to override the starting port.

## Starting a session

At the start of a session, orient yourself:

```bash
# Agent session entry point — pulls context from previous sessions
inkentry context

# If you've indexed: bring the index up to date (idempotent — a no-op when nothing changed)
inkentry index .
```

`inkentry context` is designed as the single agent entry point. At session start it first surfaces active agent sessions — other live `intent` entries, plus a warning for any file you have already changed that another active intent claims — then retrieves the most agent-relevant memory sections (handoffs, open questions, decisions, requirements) sorted newest-first, giving the agent a full picture of both in-flight and prior work.

Flags:
- `--format json` — machine-readable output
- `--kind decision` — narrow to one section
- `--path src/auth` — filter by file path tag
- `--limit N` – entries per section (defaults: handoff=3, question=10, decision=10, requirement=10); mutually exclusive with `--budget`
- `--budget N` (alias `--max-tokens`) – cap total output at N tokens; mutually exclusive with `--limit`. Under a tight budget, durable decisions and requirements are kept ahead of open questions.
- `--no-conventions` — skip the extracted-conventions section

`inkentry context` also surfaces a **conventions** section: coding conventions
inferred by a heuristic AST pass over the index (no LLM). It needs an index but
no server.

## Searching before writing

Before modifying any file, search for related code:

```bash
# Trace the call graph around a symbol (no server needed)
AGENT=true inkentry plumbing graph-edges --symbol validate_token

# Full-text search (no server needed)
AGENT=true inkentry search "authentication middleware" --only-text

# Get the raw chunks for a specific file (requires index)
AGENT=true inkentry chunks src/auth/middleware.rs

# Semantic search with call-graph expansion (requires server + index)
AGENT=true inkentry search "authentication middleware" --graph
```

The `--graph` flag appends the symbol's chunk and its 1-hop callers and callees after the ranked results — the right context for understanding blast radius before a change.

## Retrieving targeted context

Use `inkentry search` (with `--graph` for call-graph neighbours) to find relevant code, then read and reason over the results yourself:

```bash
# Trace call chains (no server needed)
AGENT=true inkentry plumbing graph-edges --symbol handle_request
AGENT=true inkentry search "request lifecycle middleware" --only-text --limit 20 --format json

# Semantic search (requires embedding server + index)
AGENT=true inkentry search "embedding format storage" --graph --format json
```

For open-ended questions that require synthesis across multiple code paths, run the multi-hop retrieval loop yourself — inkentry retrieves context; you reason over it. There is no `explore` command; loop over the primitives, refining the query each pass:

```bash
AGENT=true inkentry search "how does incremental indexing decide which files to skip?" --graph
inkentry plumbing graph-edges --symbol <symbol>   # follow callers/callees the results surfaced
inkentry chunks <file>                       # read the exact indexed code
```

Two or three passes usually suffice: search, trace with `plumbing graph-edges`, read with `chunks` (or your own file-read tool for lines outside a chunk), then decide whether you have enough context or need a sharper query. See the "Exploring: multi-hop retrieval" section of `SKILL.md`.

## After making changes

```bash
# Confirm call sites still match using the code graph (needs the index built by init)
inkentry plumbing graph-edges --symbol validate_token

# Re-index changed files so search and its --graph view stay current (incremental, blake3-gated)
inkentry index .
```

To exclude files or directories from indexing, add a `.inkentryignore` file (same syntax as `.gitignore`) at any directory. It takes higher precedence than `.gitignore`. Indexing also applies a built-in filter that skips generated, vendored, minified, and machine-data files (lockfiles, `node_modules/`, `*.min.js`, protobuf codegen, and files that self-declare `@generated`); tune it with the `[index]` table in config. See [File filtering](commands.md#file-filtering).

**Note:** `search` (and its `--graph` view) needs the index built by `inkentry init`. After changes, `inkentry index .` refreshes it — incremental and blake3-gated, so it is cheap: full-text and call-graph edges update as files are re-parsed, and the semantic ranking re-embeds in the background.

## Storing decisions

Every non-obvious choice should be stored:

```bash
inkentry memory add \
  --title "Chose sqlite-vec over hnswlib for vector search" \
  --body "No C++ dependency, single file, good enough performance for <1M vectors. Revisit if we need ANN at scale." \
  --kind decision \
  --tags storage,embeddings
```

Doing this consistently means future agents (and future you) can retrieve the rationale:

```bash
inkentry search "why did we choose sqlite-vec" --only-memory
```

**git-notes write-through:** with `store_in_git_notes` enabled (the default),
`inkentry memory add` also appends the entry to `refs/notes/inkentry` on `HEAD`,
so decisions travel with the code through clone/fetch. It is a graceful no-op
outside a git repository. Set `store_in_git_notes = false` to disable.

To inspect that write-through by hand with stock git, name the `inkentry` ref.
Plain `git notes show HEAD` reads git's default `commits` ref and reports "no
note found", a false negative that makes it look like nothing was written:

```bash
git notes --ref=inkentry show HEAD    # notes on the current commit
git notes --ref=inkentry list         # every commit carrying inkentry notes
# equivalently
GIT_NOTES_REF=refs/notes/inkentry git notes show HEAD
```

## Automatic capture (no authoring tax)

Recording decisions by hand is the part that never happens under deadline. The
payoff of wiring an agent to inkentry is that the why-layer fills itself as a
by-product of normal work, with no separate step to sit down and write docs.

Install the git hook once:

```bash
inkentry hooks install
```

The post-commit hook then runs `inkentry harvest` after every commit,
using the LLM to extract decisions, requirements, and context from the commit
messages your agent already writes. Teammates without inkentry installed are
unaffected (the hook is a no-op when `inkentry` is not on `PATH`).

You can also harvest on demand, over a range of history or straight from an
agent's own session log:

```bash
inkentry harvest --git-range HEAD~20..HEAD    # from commit messages (default source)
inkentry harvest --source claude-code --confirm   # from Claude Code session history (reads ~/.claude/history.jsonl)
```

Harvesting needs a server with an LLM backend (the local one autostarts). The
result: every later `inkentry context` / `inkentry search` starts returning the
reasoning behind the code, not just the code, without anyone stopping to author
it. Harvest is additive and idempotent, so re-running it does not duplicate
entries.

## Storing questions for async resolution

When you hit a decision point mid-task:

```bash
inkentry memory add \
  --title "Should verify re-embed from disk or from stored chunk content?" \
  --kind question \
  --tags verify,indexer
```

Pick it up later:

```bash
AGENT=true inkentry memory list --kind question
```

When resolved:

```bash
inkentry memory add \
  --title "verify re-embeds from stored chunk content" \
  --body "Avoids file I/O and keeps behaviour consistent with what was originally indexed. Disk content may have changed since last index." \
  --kind answer \
  --tags verify,indexer
```

## Signalling intent

Use the `intent` kind to broadcast to teammates (human or agent) that you are actively working on a given area. Active intents are surfaced at session start by `inkentry context`, in an "Active agent sessions" section, along with a warning for any file you have already modified that another active intent claims — so collaborators see ongoing work before starting overlapping changes.

```bash
inkentry memory add \
  --title "Refactoring auth middleware to support OAuth2" \
  --kind intent \
  --tags auth,middleware \
  --files src/auth/middleware.rs
```

When the work is done, archive the intent:

```bash
inkentry memory archive <id>
```

## Handing off between sessions

At the end of a session, write a handoff note:

```bash
inkentry memory add \
  --title "Handoff: rate limiting plan 60% done" \
  --body "Implemented token bucket in src/ratelimit/bucket.rs. Next: wire middleware, add tests, update docs. Open question: should limits be per-IP or per-API-key?" \
  --kind handoff
```

At the start of the next session, read it:

```bash
inkentry context
```

## Multi-agent coordination

When using a shared memory server (`server_url` in config), agents converge on
one shared memory by syncing:

```bash
# Two-way: push your local entries and pull teammates' entries down
inkentry sync

# One-way transfer for seeding or CI (emits a JSONL report):
inkentry plumbing pull        # server -> local
inkentry plumbing push        # local -> server
```

Conflict detection: If you write an entry semantically similar to an existing one (cosine ≥ 0.92), the server returns HTTP 409 (advisory). The entry is stored with a `contradicts` edge linking to the conflicting entry. Check `inkentry memory show <id>` to review related entries before proceeding.

## Reconciling memory from a server database

If you have access to a `inkentry-server` SQLite database (e.g. a team server snapshot or a local server DB at `~/.local/state/inkentry/server.db`), you can import its memory entries into your project's local database without running the server:

```bash
# Preview what would be imported (no writes)
inkentry memory reconcile --source-db ~/.local/state/inkentry/server.db --dry-run

# Import memory from the server DB for the current project
inkentry memory reconcile --source-db ~/.local/state/inkentry/server.db

# Import across all projects in the server DB
inkentry memory reconcile --source-db ~/.local/state/inkentry/server.db --all-projects

# Machine-readable output (one JSON object per imported entry)
inkentry memory reconcile --source-db ~/.local/state/inkentry/server.db --format json
```

Reconcile is additive and idempotent — entries already present in the local DB are skipped (matched by content hash). Useful for seeding a fresh checkout with team decisions, or for offline work after a period connected to a shared server.

## Cross-project search

If your project depends on shared libraries you've indexed separately:

```bash
inkentry link ../shared-utils
inkentry link ../api-contracts
```

Now `inkentry search` queries all three indexes and merges results by distance.

## CI integration

```bash
# Fail the build if the index is stale, without re-indexing.
# `plumbing ls-files --stale` emits one JSONL row per out-of-date file and follows
# the plumbing exit-code convention, so it exits 0 when stale files exist and 1
# when the index is fresh — the inverse of a "fresh = success" check. Gate on
# whether it produced any rows:
if inkentry plumbing ls-files --stale | grep -q .; then
  echo "Index is stale — run inkentry index"; exit 1
fi

# Print a GitHub Actions workflow hook
inkentry hooks install --ci
```

## Plumbing Commands

Plumbing commands emit JSONL to stdout and follow a strict exit-code convention, making them safe to use in scripts and pipelines. See [Plumbing and Porcelain](plumbing-and-porcelain.md) for a full explanation of the design philosophy.

Exit codes across all plumbing commands:
- **0** — success, results emitted
- **1** — no results (empty set, not an error)
- **2** — hard error (bad flags, missing DB, I/O failure) — diagnostics on stderr

Commands marked **(requires server)** need a running `inkentry-server` with its embedder ready.

### cat-chunks *(requires index)*

```
inkentry plumbing cat-chunks <file>
```

Emit all indexed chunks for a given file as JSONL.

| Flag | Description |
|------|-------------|
| `<file>` | Project-relative path of the file to retrieve chunks for (required). |

Exit codes: `0` = chunks found, `1` = file has no indexed chunks, `2` = error.

Example:

```bash
inkentry plumbing cat-chunks src/indexer/chunker.rs \
  | jq '{name: .name, lines: "\(.start_line)-\(.end_line)"}'
```

```json
{"name":"sliding_window","lines":"45-78"}
{"name":"Chunk","lines":"12-32"}
```

---

### ls-files *(requires index)*

```
inkentry plumbing ls-files [--prefix <prefix>] [--stale] [--root <dir>]
```

List every indexed file as JSONL. With `--stale`, only files whose on-disk blake3 hash differs from the stored hash are emitted.

| Flag | Description |
|------|-------------|
| `--prefix <prefix>` | Restrict output to files whose path starts with this string. |
| `--stale` | Only emit files that are out of date (on-disk hash ≠ stored hash). |
| `--root <dir>` | Project root for resolving relative paths (defaults to CWD). |

Exit codes: `0` = at least one file emitted, `1` = no files matched, `2` = error.

Example:

```bash
inkentry plumbing ls-files --stale --root .
```

```json
{"path":"src/indexer/chunker.rs","language":"rust","chunk_count":12,"indexed_at":1713528000,"stale":true}
```

---

### parse-file

```
inkentry plumbing parse-file <file>
```

Parse a file with tree-sitter and emit chunks as JSONL without writing anything to the index. Useful for previewing how inkentry will chunk a file.

| Flag | Description |
|------|-------------|
| `<file>` | Path to the file to parse (required). |

Exit codes: `0` = chunks emitted, `1` = unsupported file type or empty parse result, `2` = read error.

Example:

```bash
inkentry plumbing parse-file src/config.rs | jq '{kind, name, start_line}'
```

```json
{"kind":"struct","name":"Config","start_line":8}
{"kind":"impl","name":"Config","start_line":42}
```

---

### hash-file

```
inkentry plumbing hash-file <file>
```

Compute the blake3 hash of a file and check whether it matches the hash stored in the index, emitting a single JSON object.

| Flag | Description |
|------|-------------|
| `<file>` | Path to the file to hash (required). |

Exit codes: `0` = always (unless read error), `2` = file not readable.

Example:

```bash
inkentry plumbing hash-file src/config.rs
```

```json
{"path":"src/config.rs","hash":"a3f1...","indexed_hash":"a3f1...","is_current":true}
```

---

### knn *(requires server + index)*

```
inkentry plumbing knn [--limit N] [--min-score F] [--lang <lang>]
```

Read a JSON embedding object from stdin (as produced by `inkentry plumbing embed`) and return the *N* nearest indexed chunks by cosine similarity.

| Flag | Description |
|------|-------------|
| `--limit N` | Maximum number of results (default: `10`). |
| `--min-score F` | Drop results with cosine similarity below this threshold (0.0–1.0, default: `0.0`). |
| `--lang <lang>` | Restrict results to chunks from files of this language (e.g. `rust`, `python`). |

Exit codes: `0` = results found, `1` = no results pass the filters, `2` = error.

Compose with `embed` for a full semantic search pipeline:

```bash
echo "authentication" | inkentry plumbing embed --query | inkentry plumbing knn --limit 5
```

Example output:

```json
{"chunk_id":42,"file_path":"src/auth/middleware.rs","language":"rust","node_type":"function","name":"validate_token","start_line":18,"end_line":54,"content":"...","distance":0.12,"score":0.88}
```

---

### embed *(requires server)*

```
inkentry plumbing embed [--query]
```

Read lines from stdin and emit one JSONL embedding vector per line. Each output object contains the model name, vector dimensionality, and the float vector.

| Flag | Description |
|------|-------------|
| `--query` | Apply the F2LLM query instruction prefix (`Instruct: …\nQuery: …`). Use this flag when the output will be piped into `knn`. Omit it when embedding document text for storage. |

Exit codes: `0` = at least one vector emitted, `2` = stdin is a terminal (not a pipe) or embedding backend unreachable.

Compose with `knn`:

```bash
echo "authentication" | inkentry plumbing embed --query | inkentry plumbing knn --limit 5
```

Example output:

```json
{"model":"f2llm-v2-330m","dimensions":896,"vector":[0.021,-0.043,...]}
```

(The model name is the pinned model id, and the dimensionality reflects the
bundled native embedder: codefuse-ai/F2LLM-v2-330M at 896 dimensions.
Neither is configurable.)

---

### graph-edges

```
inkentry plumbing graph-edges --file <file> | --symbol <symbol>
```

Emit code graph edges (imports, calls, extends/implements) for a file or symbol. At least one of `--file` or `--symbol` is required. When both are provided, results are merged and deduplicated.

| Flag | Description |
|------|-------------|
| `--file <file>` | Project-relative path; emit all edges originating from this file. |
| `--symbol <symbol>` | Symbol name; emit edges where this name appears as source or target. |

Exit codes: `0` = edges found, `1` = no edges matched, `2` = neither flag supplied or DB error.

Example:

```bash
inkentry plumbing graph-edges --symbol validate_token
```

```json
{"source_file":"src/auth/middleware.rs","source_name":"handle_request","target_name":"validate_token","kind":"calls","line":28}
```

---

### read-memory

```
inkentry plumbing read-memory [--kind <kind>] [--id <n>] [--limit N]
```

Emit memory entries as JSONL. Use `--kind` to filter by entry type or `--id` to fetch a single entry.

| Flag | Description |
|------|-------------|
| `--kind <kind>` | Filter by memory kind: `decision`, `question`, `note`, `answer`, `requirement`, `handoff`, `antipattern`. |
| `--id <n>` | Fetch a single entry by its integer id. Exits `1` if not found. |
| `--limit N` | Maximum number of entries (default: `50`). |

Exit codes: `0` = entries found, `1` = no entries matched, `2` = error.

Example:

```bash
inkentry plumbing read-memory --kind decision --limit 5 | jq '{id, title}'
```

```json
{"id":17,"title":"Chose sqlite-vec over hnswlib for vector search"}
{"id":22,"title":"Incremental index skips unchanged files via blake3 hash"}
```

---

## Summary: agent workflow at a glance

```bash
# Session start — all work out of the box
inkentry context                                              # pull all prior context
inkentry context --budget 4000                               # cap total output at ~4000 tokens
AGENT=true inkentry context --format json                    # machine-readable

# Before writing code — retrieve context, reason yourself
AGENT=true inkentry plumbing graph-edges --symbol <symbol>   # call-graph edges (JSONL)
AGENT=true inkentry search "<topic>" --only-text             # full-text (no server)
AGENT=true inkentry search "<topic>"                         # unified code + memory (best available)
AGENT=true inkentry search "<topic>" --graph                 # ranked results + call-graph neighbours
AGENT=true inkentry search "<topic>" --only-memory           # search prior decisions

# Fit results within a token budget
inkentry search "<topic>" --budget 4000                        # fit within token limit
# For multi-hop questions, loop search + graph-edges + chunks yourself (see SKILL.md)

# After changes — refresh the index and verify call sites
inkentry plumbing graph-edges --symbol <symbol>
inkentry index .                                              # incremental, blake3-gated

# Session end — store decisions for next session
inkentry memory add --title "Decision: ..." --kind decision
inkentry memory add --title "Handoff: ..." --kind handoff
```
