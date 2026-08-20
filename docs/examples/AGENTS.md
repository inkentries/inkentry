# AGENTS.md: inkentry-powered project

> **Template for end users of inkentry.**
> Copy this file to your project root as `AGENTS.md`, adjust the
> project-specific sections, and commit it. This instructs AI agents to use
> inkentry for context retrieval rather than brute-force file reads.
>
> `AGENTS.md` is the cross-agent convention (see <https://agents.md>): plain
> Markdown, no frontmatter, read by the agent nearest it in the directory tree.
> It is a project convention rather than a way to obtain inkentry's skill. For
> that, install the plugin: see [installing the skill](../plugin.md).

---

## Context retrieval with inkentry

This project uses [inkentry](https://github.com/inkentries/inkentry) for code graph traversal, memory, and search.

```bash
# One search over code and memory, best available ranking (requires index)
inkentry search "how does authentication work"

# Full-text only — no embedding, no server needed
inkentry search "error handling" --only-text

# A symbol's chunk plus its 1-hop callers and callees
inkentry search "verify_token" --graph

# Exact call-graph edges as JSONL
inkentry plumbing graph-edges --symbol verify_token

# Answer an open question by looping search + graph-edges + chunks yourself
inkentry search "what does the retry logic do when the upstream times out?" --graph
```

**Rule:** run `inkentry search "<topic>"` (add `--graph` for a symbol's neighbours) before opening files you haven't read this session. Fall back to `Read`/`Grep`/`Glob` when it returns nothing useful.

---

## Recorded decisions and context

Past architectural decisions, requirements, and open questions are stored in
inkentry memory. Check them at the start of every session:

```bash
inkentry memory list --kind decision --limit 10   # prior design decisions
inkentry memory list --kind handoff --limit 3     # where last session left off
inkentry memory list --kind question              # open questions
inkentry search "topic you care about" --only-memory   # search the memory corpus
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

## Re-indexing

`search` reads the index, so keep it current. inkentry indexes are incremental —
re-run after significant changes:

```bash
inkentry index .            # index the current directory (idempotent — also refreshes a stale index)
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
- Search results are only as fresh as the last `inkentry index` run
- `inkentry harvest` requires a running `inkentry-server` with an LLM backend configured
- `inkentry search` requires an index (`inkentry init`); its semantic ranking also requires the server. Without the server it degrades to full-text, which `--only-text` selects explicitly
