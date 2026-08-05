# AGENT.md: inkentry-powered project

> **Template for end users of inkentry.**
> Copy this file to the root of your project (rename to `AGENT.md` or `CLAUDE.md`),
> adjust the project-specific sections, and commit it. This instructs AI agents
> to use inkentry for context retrieval rather than brute-force file reads.

---

## Context retrieval with inkentry

This project uses [inkentry](https://github.com/spelunk-cloud/spelunk) for code graph traversal, memory, and search.

```bash
# Trace a symbol's callers and callees (no server needed)
inkentry graph verify_token

# Full-text search (no server needed)
inkentry search "error handling" --mode text

# Semantic search: finds code by meaning (requires embedding server + index)
inkentry search "how does authentication work"

# Answer an open question by iterating search + graph (requires server + LLM)
inkentry explore "what does the retry logic do when the upstream times out?"
```

**Rule:** run `inkentry graph <symbol>` and `inkentry search "<topic>" --mode text` before opening files you haven't read this session. Fall back to `Read`/`Grep`/`Glob` when these return nothing useful.

---

## Recorded decisions and context

Past architectural decisions, requirements, and open questions are stored in
inkentry memory. Check them at the start of every session:

```bash
inkentry memory list --kind decision --limit 10   # prior design decisions
inkentry memory list --kind handoff --limit 3     # where last session left off
inkentry memory list --kind question              # open questions
inkentry memory search "topic you care about"    # semantic search over memory
```

Store new decisions as you make them; don't wait until the end:

```bash
inkentry memory add --kind decision \
  --title "Why we use X instead of Y" \
  --body "reason, alternatives considered, what breaks if changed"

inkentry memory add --kind requirement \
  --title "Must support offline mode" \
  --body "user stated this as hard requirement on 2026-04-01"
```

---

## Plumbing commands (for scripting and pipelines)

inkentry exposes machine-readable plumbing commands for use in scripts:

```bash
# Stream all indexed chunks for a file as JSONL
inkentry plumbing cat-chunks src/auth.rs

# Parse a file and emit AST chunks without writing to the DB
inkentry plumbing parse-file src/auth.rs

# Embed text and pipe into vector search
echo "token refresh flow" | inkentry plumbing embed --query \
  | inkentry plumbing knn --limit 5

# Check if a file has changed since last index
inkentry plumbing hash-file src/auth.rs

# Stream raw graph edges for a symbol
inkentry plumbing graph-edges --symbol verify_token
```

All plumbing commands emit JSONL. Exit 0 = results, 1 = no results, 2 = error.

---

## Re-indexing (if project uses semantic search)

inkentry indexes are incremental. Re-run after significant changes:

```bash
inkentry index .            # index the current directory
inkentry check              # verify the index is fresh
```

A post-commit hook can do this automatically; see `inkentry hooks install`.

---

## Project-specific notes

<!-- Customise this section for your project -->

**Tech stack:** <!-- e.g. Rust, PostgreSQL, React -->  
**Key entry points:** <!-- e.g. src/main.rs, src/api/routes.rs -->  
**Test command:** <!-- e.g. cargo test / pytest / npm test -->  
**Build command:** <!-- e.g. cargo build --release -->  

---

## What inkentry cannot do

- It cannot run your tests or build the project; use shell commands for that
- Semantic search results are only as fresh as the last `inkentry index` run
- `inkentry explore` requires a running `inkentry-server` with an LLM backend configured
- `inkentry search` (semantic) requires an embedding server and a built index; use `--mode text` for full-text search without either
