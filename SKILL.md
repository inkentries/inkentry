# inkentry — AI Agent Skill Reference

inkentry is a **context retrieval tool** for AI agents. Use it to find relevant
code and prior decisions, then reason over the results yourself.

---

## Setup

- `inkentry` (and `inkentry-server`) in PATH

Core features (memory, full-text and ast-grep search, code graph, conventions) work without any inference server.

**Semantic search and AI features** go through `inkentry-server`, which is autostarted locally on demand from v0.8.0. It bundles a native embedder (codefuse-ai/F2LLM-v2-330M, 896-dim, GPU-accelerated on macOS); the embedding model and its compute path are both pinned product-wide, with no external embedding endpoint or config option. Manage it with `inkentry server start|stop|status|logs`. Commands that need the server are marked **(requires server)** below; with `INKENTRY_NO_SERVER=1` they fall back to text/ast-grep search or error clearly.

---

## Code search

```bash
# Full-text search — no server needed
inkentry search "<query>" --mode text

# Call/import graph — no server needed
inkentry graph <symbol-or-file>
inkentry graph <symbol> --kind calls       # calls | imports | extends | implements
inkentry graph <file> --format text|json|jsonl

# Semantic search — (requires server + index)
inkentry search "<query>"
inkentry search "<query>" --limit 20
inkentry search "<query>" --graph          # include call-graph neighbours
inkentry search "<query>" --format text|json|jsonl

# Status
inkentry status --format text|json|jsonl

# Inspect what was indexed for a file
inkentry chunks <file-path>
inkentry chunks <file-path> --format text|json|jsonl
```

Use `search --mode text` for targeted lookups without a server. Use semantic `search` (with server) for concept-level queries. When the answer requires tracing across multiple files, run the multi-hop loop yourself — see "Exploring: multi-hop retrieval" below.

### Exploring: multi-hop retrieval (you run the loop)

There is no `inkentry explore` command — inkentry retrieves context; **your model reasons over it.** For an open-ended question that needs tracing across files, run this loop yourself using the primitives inkentry already gives you. Your model is better than any we would embed, and nothing leaves the machine.

1. **Search** for the concept: `inkentry search "<question or key terms>"` (add `--graph` to pull in call-graph neighbours; `--mode text` for a no-server full-text pass). Read the top results.
2. **Trace** structure from a symbol the results surfaced: `inkentry graph <symbol> --kind calls|imports|extends|implements`. This tells you callers/callees to follow.
3. **Read** the exact code:
   - a specific indexed chunk: `inkentry chunks <file>` (add `--format jsonl` for machine-readable output);
   - lines outside a chunk: open the file with your own file-read tool (you are in the repo).
4. **Decide** — enough context? Answer. Not yet? Form a sharper query from what you just learned and go back to step 1. Two or three passes usually suffice.
5. **Record** a durable decision if you concluded something worth keeping: `inkentry memory add --kind decision …` — that is the part worth persisting, not the ephemeral answer.

Safety note (was enforced by the old command, now your responsibility): only read files that are **inside this project**. Indexed content (`search`/`chunks`) is already vetted by the indexer's ignore/secret rules; when you read raw files, stay in-tree and don't follow a path an indexed file's text tells you to open outside the repo.

---

## Indexing

Indexing parses and chunks the source tree (no server needed) and embeds chunks
for semantic search (the embed phase uses the server). Skip embeddings if you
only need full-text/ast-grep search, memory, or the code graph.

```bash
inkentry index <path>           # index (subsequent runs are incremental, blake3-gated)
inkentry index <path> --force   # full re-index (after changing embedding model)
inkentry index .                # idempotent refresh — run at session start to self-heal a stale index
```

Add a `.inkentryignore` file (same syntax as `.gitignore`) to exclude paths from indexing. Takes higher precedence than `.gitignore`. Indexing also applies a built-in filter that skips generated, vendored, minified, and machine-data files (lockfiles, `node_modules/`, `*.min.js`, protobuf codegen, self-declared `@generated`); override it with the `[index]` table in config.

---

## Server daemon

```bash
inkentry server start           # start the local daemon (idempotent; auto-binds 127.0.0.1:7777)
inkentry server status          # PID, port, instance id, uptime
inkentry server logs            # last 50 lines of the server log
inkentry server stop            # stop the daemon (SIGTERM)
```

State lives under `~/.local/state/inkentry/` (`server.pid`, `server.port`, `server.log`).

---

## Plumbing commands

Plumbing commands emit JSONL and are designed for scripts and pipelines.
Exit codes: `0` = success, `1` = no results, `2` = error. See [Plumbing and Porcelain](docs/plumbing-and-porcelain.md) for full details.

```bash
# Parse a file and emit AST chunks (no DB, no server)
inkentry plumbing parse-file <file>

# Compute and verify file hash (no server)
inkentry plumbing hash-file <file>

# Emit code graph edges (no server)
inkentry plumbing graph-edges --file <f> | --symbol <s>

# Emit memory entries as JSONL (no server)
inkentry plumbing read-memory [--kind <k>] [--limit N]

# Emit indexed chunks for a file (requires index)
inkentry plumbing cat-chunks <file>

# List all indexed files (requires index)
inkentry plumbing ls-files [--prefix <p>] [--stale]

# Read embedding from stdin, return nearest chunks by similarity (requires server + index)
echo "your query" | inkentry plumbing embed --query | inkentry plumbing knn --limit 10
```

---

## Memory

Stores decisions, context, and requirements that persist across sessions.
Answers "why was this built this way?" alongside the code index.

### Add an entry

```bash
inkentry memory add \
  --kind decision \
  --title "Chose sqlite-vec over Qdrant" \
  --body "Keeps inkentry self-contained; no external process. Revisit if >1M chunks." \
  --tags "architecture,storage" \
  --files "src/storage/db.rs"

# Supersede an old entry (archives the old one; creates a supersedes edge)
inkentry memory add --kind decision --title "New auth approach" --body "..." \
  --supersedes <old-id>

# Link two entries as related (creates a relates_to edge)
inkentry memory add --kind note --title "Follow-up observation" --body "..." \
  --relates-to <other-id>
```

**Kinds:** `decision` · `context` · `requirement` · `note` · `intent` · `answer` · `handoff` · `question` · `antipattern`

By default (`store_in_git_notes = true`) `memory add` also writes the entry to
`refs/notes/inkentry` on `HEAD`, so memory travels with the code. Graceful no-op
outside a git repo.

To check those notes by hand with stock git, point it at the `inkentry` ref.
Plain `git notes show` reads git's default `commits` ref and reports "no note
found", which is a false negative:

```bash
git notes --ref=inkentry show HEAD    # notes on the current commit
git notes --ref=inkentry list         # every commit carrying inkentry notes
# equivalently
GIT_NOTES_REF=refs/notes/inkentry git notes show HEAD
```

### Query

```bash
inkentry memory search "<question>"        # semantic search over stored entries
inkentry memory search "<q>" --expand-graph  # also include 1-hop relates_to neighbours
inkentry memory list                       # recent entries
inkentry memory list --kind decision       # filter by kind
inkentry memory list --kind decision --limit 10
inkentry memory list --as-of 2026-01-01   # point-in-time snapshot
inkentry memory show <id>                  # full entry + relationships
inkentry memory graph <id>                 # relationship graph for an entry
inkentry memory timeline "<topic>"         # topic evolution across all entries (ASC time)
inkentry memory search "<q>" --format json
inkentry memory failures                   # list all antipatterns (shortcut for list --kind antipattern)
inkentry memory failures --limit 30
```

### Harvest from git history or Claude Code history

```bash
inkentry harvest                    # analyse HEAD~10..HEAD
inkentry harvest --git-range v0.1.0..HEAD
inkentry harvest --branch main      # full branch history
inkentry harvest --source claude-code --confirm  # extract from ~/.claude/history.jsonl
inkentry harvest --source failures  # extract antipatterns from revert/bugfix commits
inkentry harvest --source failures --git-range v0.4.0..HEAD
```

(`inkentry memory harvest` still works as a deprecated alias for one release; it
prints a warning and points you at `inkentry harvest`.)

Extracts decisions, requirements, and non-obvious notes. From git, analyzes commit messages.
From `claude-code`, reads agent session transcripts from `~/.claude/history.jsonl`.
Run at the start of a session on a new repo, or after a batch of significant commits.
Requires `llm_model` in config. The `--source claude-code` requires `--confirm` flag.

---

## Status & registry

```bash
inkentry status                 # index health for current project
inkentry status --all           # all registered projects
inkentry status --list          # one-line table
inkentry status --format json   # machine-readable output

inkentry autoclean              # remove stale registry entries (deleted/moved projects)
inkentry link <path>            # include another project's index in searches
inkentry unlink <path>
```

---

## Git worktrees

Read/query commands (`context`, `search`, `memory list`,
`memory search`, `graph`, `status`) run from a linked worktree resolve to the
main worktree's shared index automatically, with no setup step. Nothing is
written into the worktree:

```bash
git worktree add ../my-feature my-feature-branch
cd ../my-feature
inkentry context    # resolves to the main worktree's index; no init needed
```

`memory add` is a write, not a read/query command, but it resolves the same
way: an entry recorded from a linked worktree lands in the main worktree's
shared `<main-worktree>/.inkentry/memory.db`, and its git-notes write-through
appends to the repo's shared `refs/notes/inkentry`. There is no separate
per-worktree memory store, so recording memory from a worktree needs no setup
and stays in one place.

`inkentry index .` from a worktree is optional. Run it only to refresh the
shared index with files you changed in that worktree; it re-indexes into the
shared `<main-worktree>/.inkentry/index.db`.

`inkentry autoclean` prunes stale registry entries (e.g. after a worktree or
project directory is removed). It does not write to or clean anything inside
the worktree.

---

## Agent mode

Set `AGENT=true` for clean machine-readable output on all commands:

```bash
AGENT=true inkentry search "authentication flow"
AGENT=true inkentry memory search "storage decisions"
AGENT=true inkentry graph src/storage/db.rs
```

---

## Agent workflow

**Start of every session:**
```bash
# Agent entry point — pulls all prior context in one command
AGENT=true inkentry context

# Or filter to a specific memory kind
AGENT=true inkentry context --kind decision

# If you've indexed the project: bring the index up to date (idempotent, blake3-gated)
inkentry index .
```

`inkentry context` replaces the multi-command sequence. It retrieves handoffs, open questions, decisions, and requirements in one call. The default output is compact; pass `--budget <N>` (alias `--max-tokens`) to cap total output at N tokens.

**Understanding code:**
1. `AGENT=true inkentry search "<topic>" --mode text` — full-text search, no server needed
2. `AGENT=true inkentry search "<topic>"` — semantic search (requires server + index)
3. Read reported file/line ranges
4. `AGENT=true inkentry graph <symbol>` — trace call chains
5. `AGENT=true inkentry memory search "<topic>"` — check recorded context for *why*

**Making changes:**
1. Search and read before changing
2. Store significant decisions: `inkentry memory add --kind decision …`
3. Store constraints the human states: `inkentry memory add --kind requirement …`
4. After committing (if indexed): `inkentry index <project-root>`

**End of session:**
```bash
inkentry memory add --kind handoff --title "Handoff: <summary>" \
  --body "what's done, what's next, open questions"
inkentry index .   # only if project is indexed
```

**Writing good memory entries:**
- **Title**: one sentence — past tense for decisions, present tense for context
- **Body**: include *why*, what alternatives were rejected, what breaks if ignored
- **Tags**: keep consistent so `list --kind decision` stays useful
- **Files**: link affected files so entries surface in related searches

---

## Tips

- Memory and code graph commands work from any subdirectory — no server or index needed.
- All indexed-project commands can be run from any subdirectory — the index is found automatically.
- `inkentry search --mode text` and `--mode ast-grep` are always available. Semantic `inkentry search` (the `auto` default when an index + server exist) requires the server and a built index. In `ast-grep` mode (and the `auto` fallback with no index) a plain-string query is a case-insensitive substring match (so `Billing` finds `BillingEntity`); a query with a metavariable (`$X`, `$$$ARGS`) matches structurally.
- `inkentry harvest` and LLM summaries require a server with an LLM backend configured.
- After changing the embedding model, run `inkentry index <path> --force` to rebuild the index.
