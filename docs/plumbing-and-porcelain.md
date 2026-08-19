# Plumbing and Porcelain

## What is plumbing vs porcelain?

Git popularised the distinction: *porcelain* commands are polished, human-friendly interfaces (coloured output, progress bars, readable prose), while *plumbing* commands are low-level, composable building blocks designed for scripts and pipelines. inkentry follows the same pattern. Porcelain commands like `inkentry search` and `inkentry memory list` format output for reading in a terminal; plumbing commands under `inkentry plumbing` emit raw JSONL to stdout and are designed to be piped into other processes.

## When to use plumbing

Use plumbing commands when you are:

- **Writing agent scripts** — parse JSONL directly rather than scraping human-readable text.
- **Composing pipelines** — chain plumbing commands with `jq`, `xargs`, or other plumbing commands.
- **Running in CI** — exit codes are unambiguous (see table below); no ANSI codes pollute logs.
- **Building reproducible queries** — the same plumbing invocation always produces the same schema, regardless of terminal width or colour settings.
- **Integrating inkentry output into another tool** — JSONL is trivially parsed in any language.

## When to use porcelain

Use porcelain commands for:

- **Day-to-day developer use:** `inkentry search`, `inkentry memory list` are readable and interactive.
- **Multi-hop exploration** — loop `inkentry search` + `inkentry search <symbol> --graph` + `inkentry chunks` yourself for questions that span files (see `SKILL.md`).
- **Quick status checks** — `inkentry status` gives a human-readable health report.

## Exit code convention

| Exit code | Meaning |
|-----------|---------|
| `0` | Command succeeded; one or more results were emitted. |
| `1` | No results found (not an error — treat as empty set). |
| `2` | Hard error — a flag was missing, the DB was not found, or an I/O failure occurred. Diagnostics are written to stderr. |

Scripts should distinguish `1` (empty) from `2` (broken) rather than treating any non-zero exit as fatal.

These codes, and the JSONL field names and types below, are covered by the
[stability contract](stability.md): they are semver-bound, evolve additively
only, and are enforced by tests rather than by convention. `hash-file`, `embed`,
and `publish-notes` cannot return `1`; the contract explains why.

`push` and `pull` are the exception to "exit 1 means stdout is empty": they
always emit their one report object on a completed run, so exit `1` there means
the run completed with an **empty delta** (nothing new pushed, or nothing new
pulled) and the report is still on stdout. Only their exit `2` (the run did not
complete) leaves stdout empty. Everything else about the codes is unchanged.

## Output format

All plumbing commands write **one JSON object per line** (JSONL) to **stdout**. Errors and warnings go to **stderr** only — stdout is always machine-parseable. There are no progress bars, no ANSI escape codes, and no trailing commas or array wrappers.

Example: reading five results from `knn` into a shell array:

```bash
mapfile -t results < <(
  echo "auth flow" \
    | inkentry plumbing embed --query \
    | inkentry plumbing knn --limit 5
)
# Each element of $results is a self-contained JSON object.
```

## Composition examples

### Semantic search via embed + knn

Embed a query string and pipe the vector directly into KNN search:

```bash
echo "auth flow" \
  | inkentry plumbing embed --query \
  | inkentry plumbing knn --limit 5 \
  | jq -r '"\(.score | . * 100 | round)%  \(.file_path):\(.start_line)  \(.name // "(anon)")"'
```

`embed --query` prepends the F2LLM instruction prefix expected for queries (`Instruct: …\nQuery: {q}`), producing a JSON object with a `vector` field. `knn` reads that object from stdin and emits one result object per line, sorted by similarity score descending.

### List stale files and re-index only those

```bash
inkentry plumbing ls-files --stale --root . \
  | jq -r '.path' \
  | xargs -I{} inkentry index {}
```

`ls-files --stale` exits `1` if nothing is stale (safe to check `$?` before proceeding). Each emitted object's `.path` field is the project-relative path stored in the index.

## All 11 plumbing commands

Every command below emits JSONL. All but `publish-notes`, `push`, and `pull` are
read-only: those three write or talk to a remote, so the namespace as a whole is
not safe-by-construction for scripting or sandboxing. `push` and
`pull` additionally require an explicitly-configured team `server_url` (never the
inference loopback), the same guard the two-way `inkentry sync` uses.

| Command | Synopsis | Description |
|---------|----------|-------------|
| `cat-chunks` | `inkentry plumbing cat-chunks <file>` | Emit all indexed chunks for a file as JSONL. Exits `1` if the file has no indexed chunks. |
| `ls-files` | `inkentry plumbing ls-files [--prefix <p>] [--stale] [--root <dir>]` | List every indexed file as JSONL. `--stale` restricts output to files whose on-disk hash differs from the stored hash. Exits `1` if no files match. |
| `parse-file` | `inkentry plumbing parse-file <file>` | Parse a file using tree-sitter and emit chunks as JSONL without writing to the index. |
| `hash-file` | `inkentry plumbing hash-file <file>` | Compute the blake3 hash of a file and compare it to the stored hash, reporting whether the index is current for that file. |
| `knn` | `inkentry plumbing knn [--limit N] [--min-score F] [--lang <lang>]` | Read a JSON embedding object from stdin and return the *N* nearest indexed chunks by cosine similarity. Exits `1` if no results pass the filters. |
| `embed` | `inkentry plumbing embed [--query]` | Read lines from stdin and emit one JSONL embedding vector per line. Pass `--query` to apply the query retrieval prefix (use this before piping into `knn`). |
| `graph-edges` | `inkentry plumbing graph-edges --file <f> \| --symbol <s>` | Emit code graph edges (imports, calls, extends) for a file or symbol as JSONL. At least one of `--file` or `--symbol` is required. Exits `1` if no edges found. |
| `read-memory` | `inkentry plumbing read-memory [--kind <k>] [--id <n>] [--limit N]` | Emit memory entries as JSONL. Filter by kind (`decision`, `question`, `note`, etc.) or fetch a single entry by id. |
| `publish-notes` | `inkentry plumbing publish-notes [remote] [--best-effort]` | Publish memory notes (`refs/notes/inkentry`) to `remote` (default `origin`): fetch onto the tracking ref, union-merge with `cat_sort_uniq`, push. Never force-pushes. **Writes and performs network I/O.** `remote` must be a configured remote *name*; a URL is not resolved but reported as `{"published":false,…,"skipped":"no_such_remote"}` at exit `0`. If another process holds the notes lock the publish is skipped rather than run unmerged, reported as `{"published":false,…,"skipped":"lock_unavailable"}` on stdout and as a warning on stderr, at exit `0` with or without `--best-effort`: nothing is lost, and the records publish on your next push. `--best-effort` warns on stderr and exits `0` instead of failing, which is what the pre-push hook uses. Reach for `inkentry hooks install --pre-push` rather than calling this directly. |
| `push` | `inkentry plumbing push [--source <path>] [--include-archived]` | One-way local→server memory push for seeding or CI. **Writes to the team server.** Emits one report object: `{attempted, created, skipped, failed, already_synced, edges_pushed, without_local_vector, embedded_locally, interrupted}`. Exit `0` when ≥1 entry was created; exit `1` on an empty delta (nothing local to push, or everything already present) — the report is still emitted; exit `2` when the run did not complete (no `server_url`/`project_id`, unreachable/auth, a total failure where nothing landed, or an interruption), with stdout empty and a diagnostic on stderr. `--source` pushes from a specific `memory.db`; `--include-archived` propagates tombstones. For everyday two-way convergence use `inkentry sync`. |
| `pull` | `inkentry plumbing pull` | One-way server→local memory delta pull (cursored on the last-synced remote id). **Reads from the team server, writes locally.** Emits one report object: `{applied, embedded_locally, without_local_vector}`. Applied entries are also embedded locally so `inkentry memory search` can surface them; `without_local_vector` counts the ones no embedder could be reached for, which the next pull retries. Exit `0` when ≥1 entry was applied; exit `1` on an empty delta (nothing new) — the report is still emitted; exit `2` on a setup/network/auth error, with stdout empty and a diagnostic on stderr. For everyday two-way convergence use `inkentry sync`. |

---

See also: [Agent Guide](agent-guide.md) for the broader context on using inkentry in agentic workflows.
