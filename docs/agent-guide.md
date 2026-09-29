# Agent Guide

`inkentry` is designed to work as infrastructure for AI coding agents, not just as a human developer tool. This guide covers the patterns that make agents most effective when paired with `inkentry`.

**The key mental model**: inkentry retrieves context; you reason over it. Use `inkentry search` — with `--graph` to pull in call-graph neighbours — to find the right code, read the results, then synthesise the answer yourself. inkentry is a persistent memory store and code navigation tool, not an oracle.

**What's built-in:** memory (local SQLite `memory.db`, optionally mirrored to git-notes), code graph, full-text search, and extracted conventions work with just the CLI binary — no server needed. A project's memory always lives in its local `memory.db`; that is the canonical store of record for every memory command.

**What's server-backed:** semantic/hybrid search (the default `inkentry search` ranking) and `inkentry harvest` use `inkentry-server` for **inference** (embeddings + LLM). From v0.8.0 the server is autostarted locally on demand and bundles the embedder (codefuse-ai/F2LLM-v2-330M, 896-dim, on the llama.cpp engine: Metal on macOS, Vulkan on Windows and Linux x64, CPU elsewhere) — there is no external embedding server to run by default. The auto-discovered loopback server is **inference-only**: it never stores memory. For the memory corpus of `inkentry search` the CLI sends only the query to the loopback embedder and runs the vector search locally against `memory.db` — note text never leaves the local store. If you force offline mode (`INKENTRY_NO_SERVER=1`), these commands fall back to full-text search or error clearly, and all memory commands operate on `memory.db`.

**Where does memory live?** Always `memory.db` for the active project — **unless** you have *explicitly* configured a team `server_url`, or opted the project into the hosted inkentry cloud (`cloud = true`, mutually exclusive with `server_url`), either of which relocates the store of record to that shared server (the team-memory tier). An auto-discovered loopback server does **not** change where memory lives.

## The core loop

A productive agentic session with `inkentry` looks like this:

1. **Orient** — read memory and bring the index up to date (`inkentry context`, then `inkentry index .` — idempotent and blake3-gated, so it is a no-op when nothing changed)
2. **Search** — find the relevant code before reading or editing it
3. **Execute** — make code changes, delegating sub-tasks as needed
4. **Verify** — re-check the call graph and re-index after changes
5. **Codify** — store decisions, handoffs, and context in memory

This loop compounds: each session leaves better context for the next, whether that's the same agent resuming or a different one picking up.

## The write contract

The read side of inkentry is a search. The write side is the agent: inkentry
stores what it is given and never judges it, so what memory holds is what the
agent chose to write. [The agent contract](agent-contract.md) says what that is:
the nine kinds with an example that belongs and one that does not, how to write
a title and a body, when to write and when not to, how to choose tags and link
files, the reconcile loop (`--reconcile`, exit status `3`, and the four ways to
resolve a candidate), how to record an update, and the environment variables
that declare who is calling. The agent skill carries a copy, so an agent that
has the skill has the contract.

## Machine-readable output

Set `AGENT=true` and every `inkentry` command returns JSON:

```bash
export AGENT=true

inkentry search "error handling"          # → [ { type, fused_rank, fused_score, corpus_rank, code|memory } ]
inkentry status                           # → { files, chunks, embeddings, ... }
inkentry memory list                      # → JSON array of notes
inkentry search "auth decisions" --only-memory   # → the same envelopes, every one type "memory"
```

`search` always returns the envelope, never a bare array of hits: read the
payload under `.code` or `.memory` according to `.type`. `--graph` neighbours
and memory attachments are appended after the ranked members with `fused_rank`,
`fused_score` and `corpus_rank` all `null`.

You can also use `--format json` on individual commands.

### The `search` envelope, and how a consumer gets it wrong silently

This is the one shape worth getting right before you write a parser around it,
because getting it wrong reports **zero results** instead of an error:

- **Reading fields at the top level.** Each result nests its payload under
  `.code` or `.memory`; `.name` and `.file_path` are not at the top level. A
  selector written against the top level matches nothing on every result, at full
  query latency, and looks exactly like a codebase with no match in it.

The minimum a consumer needs:

```bash
# Ranked members only, with the payload pulled up per type.
inkentry search "auth flow" --format json \
  | jq -r '.[] | select(.fused_rank != null)
           | if .type == "code" then "\(.code.file_path):\(.code.start_line)"
             else "memory \(.memory.id) \(.memory.title)" end'
```

- Branch on `.type`; exactly one of `.code` / `.memory` is present, and the
  other key is **absent**, not `null`.
- Keep the emitted order, or sort by `.fused_rank`. Never re-sort by `distance`:
  a code distance and a memory distance come from different embedding
  instructions on different scales, and comparing them across corpora is
  meaningless. This is why fusion ranks on position alone.
- Filter out `fused_rank == null` if you want ranked results only. Those are
  `--graph` and `--expand-graph` attachments, appended after the ranked members.
- Tolerate unknown fields, and tolerate the absence of the conditional memory
  fields (`score`, `valid_at`, `source_project` and the rest are omitted, not
  `null`, when unset).
- No matches emits `[]` (or nothing, under `jsonl`) and exits `0`. `search` is
  porcelain: it does not use the plumbing convention where `1` means empty.
- `--budget` replaces the top-level array with an object under `--format json`:
  `{token_budget, tokens_used, tokens_remaining, results}`. `jsonl` is
  unaffected. Iterating the top level of a budgeted `json` run reads zero
  results.
- Stale-index, server-discovery, coverage and ranking-availability notices go to
  stderr, never stdout, so they never reach a parser reading stdout. `-q` /
  `--quiet` silences them when even a clean stderr matters, which is worth
  reaching for under Windows PowerShell 5.1: it renders any native-command
  stderr as a red error block, so an informational notice there reads as a
  crash. Results and exit codes are the same either way, and `-q` never hides an
  error or the warning about a server started by another user.

Field by field, including every guaranteed and conditional field of both
payloads, see
[JSON output: the envelope contract](commands.md#json-output-the-envelope-contract).
Its stability level is in the
[Stability contract](stability.md#structured-output-from-porcelain-commands).

### Choosing a corpus

Corpus selection is part of reading the output correctly, since it decides which
payload keys appear: plain `search` interleaves both corpora, `--only-code` and
`--only-memory` (mutually exclusive) restrict to one, and `--only-text` drops
the embedding step entirely, so it needs no server and the memory payload then
carries no `score`.

## Managing the local server daemon

If your config does not have a `server_url`, `inkentry` auto-discovers a local
`inkentry-server` running on loopback by reading
`~/.local/state/inkentry/server.port`, and uses whatever answers there only
when the recorded `server.pid` still names an `inkentry-server` process and the
server reports the `instance_id` recorded at start. A daemon started before
these files existed is not auto-discovered until you restart it.  You can start, stop, and inspect that
daemon with the `inkentry server` subcommand. This auto-discovered daemon is an **inference backend only** — it serves embeddings and LLM calls. It is **not** a memory store: your project's memory stays in `memory.db` regardless of whether this server is running. (Memory moves to a server only when you *explicitly* set `server_url` to a team instance, or set `cloud = true`, in your config.)

```bash
# Start inkentry-server on port 4655 (idempotent — no-op if already running)
inkentry server start

# Check whether the daemon is running and get its PID/port/version
inkentry server status

# Tail the last 50 lines of the server log
inkentry server logs

# Stop the daemon gracefully (SIGTERM; waits up to 10 s)
inkentry server stop
```

**State directory:** all runtime files (`server.pid`, `server.port`,
`server.instance_id`, `server.log`) live under `~/.local/state/inkentry/`.

**Idempotency:** `inkentry server start` is safe to call at the beginning of
every session.  If the daemon is already running and healthy it exits 0
immediately.  If the PID is stale (process dead), it starts a fresh instance.

**When to use `status` vs probing `/v1/health` directly:** use
`inkentry server status` for the daemon's health (running state, PID, port,
version, and reachability) — it is the human-readable probe you want during
debugging, so you rarely need to poll `/v1/health` directly.

**Port selection:** `inkentry server start` binds `--port` (default 4655)
exactly, and fails loudly if an unrelated process holds it. The auto-start path
(`inkentry init` on a fresh machine) is the forgiving one: it takes 4655 when
free and otherwise lets the OS assign an ephemeral port. Either way the bound
port lands in `server.port`, which is what auto-discovery reads, alongside the
`server.pid` and `server.instance_id` it checks that port's responder against.

## Starting a session

At the start of a session, orient yourself:

```bash
# Agent session entry point — pulls context from previous sessions
inkentry context

# If you've indexed: bring the index up to date (idempotent — a no-op when nothing changed)
inkentry index .
```

`inkentry context` is designed as the single agent entry point. At session start it first surfaces active agent sessions — other live `intent` entries, plus a warning for any file you have already changed that another active intent claims — then retrieves the most agent-relevant memory sections (handoffs, open questions, decisions, requirements) sorted newest-first, giving the agent a full picture of both in-flight and prior work.

With the Claude Code plugin installed, its session-start hook runs this for you, including after a compaction (see [Hooks](#hooks)).

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

Two or three passes usually suffice: search, trace with `plumbing graph-edges`, read with `chunks` (or your own file-read tool for lines outside a chunk), then decide whether you have enough context or need a sharper query. See the "Exploring: multi-hop retrieval" section of [the skill](https://github.com/inkentries/agent-plugin/blob/main/skills/inkentry/SKILL.md).

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

What to record, when, and how to link it is written once, in
[the agent contract](agent-contract.md). This section is the short version: one
command, with `--reconcile`, which agent surfaces write with.

```bash
inkentry memory add --reconcile --format json \
  --kind decision \
  --title "Chose sqlite-vec over hnswlib for vector search" \
  --body "No C++ dependency, single file, good enough performance for <1M vectors. Revisit if we need ANN at scale." \
  --tags storage,embeddings \
  --files src/storage/search.rs
```

Doing this consistently means future agents (and future you) can retrieve the
rationale:

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

## Hooks

There are two kinds of hook, and they do different jobs.

**The git hook** (`inkentry hooks install`, above) runs in your clone after a
commit: it claims the commit's pending memory entries with
`inkentry memory anchor --commit HEAD`, then re-indexes and harvests. Git does
not clone hooks, so it exists only where someone installed it.

**Agent hooks** run inside the agent, whichever clone it is working in. The
[Claude Code plugin](plugin.md) ships four, and they call the CLI as follows.
Anyone wiring another agent can do the same; each call needs no inference
server.

| When | Command | What comes back |
|---|---|---|
| Session start, including after a compaction | `inkentry context --budget <N> --format text` | The context sections, then a final `tokens used: X/N` line. A store with no entries prints only that line. |
| Before an edit to a file | `inkentry memory list --file <path> --format json` | A JSON array of entries linked to that exact repository-relative path (`id`, `entity_id`, `kind`, `title`, `body`, `tags`, `linked_files`, `created_at`, `status`). With none, the text `No memory entries found.` and exit `0`, not `[]`. |
| After an agent's `git commit` | `inkentry memory anchor --commit HEAD` | Nothing, and always exit `0`. It is the git hook's command, run again for clones without the hook. |
| When the agent is about to stop | `inkentry memory add --reconcile ...` (run by the agent, at the hook's prompt) | See [the reconcile loop](agent-contract.md#7-the-reconcile-loop). |

Two details matter to anyone writing one:

- `--file` matches the path exactly as `memory show` or `--format json` prints
  it, repository-relative with forward slashes. `./src/auth.rs` and an absolute
  path match nothing.
- Outside an inkentry project `context` exits `1` and `memory anchor` exits
  `0`. A hook that must never fail an action swallows the exit status and
  prints nothing on stderr.

An agent hook declares itself with `INKENTRY_TRIGGER=hook INKENTRY_ACTOR=agent
INKENTRY_TOOL=<tool> INKENTRY_SESSION_REF=<session id>`, so the events it
causes are counted apart from what the agent chose to run
([caller declaration](config-reference.md#caller-declaration-adr-098-d5d6)).
The commands the agent runs itself carry `INKENTRY_TRIGGER=explicit`.

## Questions, intents and handoffs

The contract says what each of these entries contains
([question](agent-contract.md#question), [answer](agent-contract.md#answer),
[intent](agent-contract.md#intent), [handoff](agent-contract.md#handoff)). What
follows is how they are picked up again.

- **Questions.** `AGENT=true inkentry memory list --kind question` lists what is
  open. Answer one with an `answer` entry written with `--relates-to <question
  id>`; that link is what closes it.
- **Intents.** Active intents surface at session start in the "Active agent
  sessions" section of `inkentry context`, with a warning for any file you have
  already modified that another active intent claims. When the work is done,
  archive the intent: `inkentry memory archive <id>`.
- **Handoffs.** The next session reads the latest handoffs with
  `inkentry context`.

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

Reconciliation ([ADR-100](adr/100-memory-add-reconciles-against-existing-entries-before-it-writes.md)): a team `server_url` computes the same duplicate/related candidates a local `memory add` does, before storing. Pass `--reconcile` for it to refuse an unresolved duplicate-band write with a 409 (`stored: false`) instead of storing it, the same as the local store; resolve with `--supersedes`/`--relates-to`/`--contradicts`/`--distinct-from <id>` as usual. This needs the server to advertise `memory.reconcile` on `GET /v1/health`; against an older server, `--reconcile` has no effect and a semantically close write is still just stored, with no `contradicts` edge written on its behalf — similarity alone no longer writes one.

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

Now `inkentry search` queries all three indexes and merges their code results by
distance before fusing code and memory by rank. Pass `--local-only` to skip the
linked projects.

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
bundled embedder: codefuse-ai/F2LLM-v2-330M at 896 dimensions.
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

Exit codes: `0` = edges found, `1` = no edges matched, `2` = neither flag supplied, `--file` names a path the index does not hold, or DB error.

Example:

```bash
inkentry plumbing graph-edges --symbol validate_token
```

```json
{"source_file":"src/auth/middleware.rs","source_name":"handle_request","target_name":"validate_token","kind":"calls","line":28,"target_file":"src/auth/middleware.rs"}
```

`target_file`, when present, is the calling file itself: the call was bound to a
definition in that file's own scope. It is omitted otherwise, including for a
callee imported from another file, so the edge may match any definition with
that name.
A call whose callee is a parameter or local variable produces no edge, because
it cannot reach a definition elsewhere in the repository.

---

### read-memory

```
inkentry plumbing read-memory [--kind <kind>] [--id <uuid>] [--limit N]
```

Emit memory entries as JSONL. Use `--kind` to filter by entry type or `--id` to fetch a single entry.

| Flag | Description |
|------|-------------|
| `--kind <kind>` | Filter by memory kind: `decision`, `question`, `note`, `answer`, `requirement`, `handoff`, `antipattern`. |
| `--id <uuid>` | Fetch a single entry by its UUID. Exits `1` if not found. |
| `--limit N` | Maximum number of entries (default: `50`). |

Exit codes: `0` = entries found, `1` = no entries matched, `2` = error.

Example:

```bash
inkentry plumbing read-memory --kind decision --limit 5 | jq '{id, title}'
```

```json
{"id":"01a01a9a-4140-7cbb-8047-c624a5ecb8e4","title":"Chose sqlite-vec over hnswlib for vector search"}
{"id":"01a01a9a-51b7-7d02-9f3e-8ac41d60b95f","title":"Incremental index skips unchanged files via blake3 hash"}
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
# For multi-hop questions, loop search + graph-edges + chunks yourself (see https://github.com/inkentries/agent-plugin/blob/main/skills/inkentry/SKILL.md)

# After changes — refresh the index and verify call sites
inkentry plumbing graph-edges --symbol <symbol>
inkentry index .                                              # incremental, blake3-gated

# Session end — record what was decided (rules: agent-contract.md)
inkentry memory add --reconcile --format json --kind decision --title "..." --body "..."
inkentry memory add --kind handoff --title "Handoff: ..." --body "done, next, open"
```
