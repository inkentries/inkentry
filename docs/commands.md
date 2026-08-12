# Commands Reference

Every command accepts `-c, --config <path>` to override the default config file
(`~/.config/inkentry/config.toml`), and `--color <auto|always|never>` to control
colored output (default `auto`: on when stdout is a terminal and `NO_COLOR` is
unset). The flags and defaults below match the installed binary; run
`inkentry <command> --help` to confirm against your version.

A local `inkentry-server` is autostarted on demand and provides embeddings
(native, via the candle-served F2LLM-v2-330M model) and, when a chat model is
configured, LLM inference. Commands that need semantic search or an LLM (`search`,
`harvest`) use that server; the
always-available commands (`search --only-text`, `plumbing graph-edges`,
`memory add/list`, `context`) work with no server.

---

## inkentry init

Initialise inkentry for the current project: register it, start the local server
if needed, parse and chunk the source tree, hand the embedding pass to a
detached background worker, and (when inside a git repo with an `origin` remote)
configure the fetch refspec — then fetch the notes ref once — so a single `init`
after a clone pulls in the team's project memory.

```
inkentry init [options]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--hook` | false | Also install the post-commit git hook |
| `--no-index` | false | Skip the initial index run |
| `--name <slug>` | derived | Explicit project slug. Overrides the git-derived default; use it for projects without a git remote. |

`init` writes the project slug to `.inkentry/config.toml` but takes **no git
action on it** — commit it yourself so the slug travels with the repo and the
whole team shares one identity:

```bash
git add .inkentry/config.toml && git commit -m "Add inkentry project slug"
```

`init` prints a one-line reminder to do this; committing the file is a step you
own, not something `init` performs. The slug defaults to the git-derived value:
`host/owner/repo` when an `origin` remote exists, else `local/<blake3-hex>` of the
canonical path. Pass `--name` to set an explicit slug for a repo without a remote
or to choose your own. Without a committed slug, a fresh clone of a remote-less
repo derives a different per-clone identity, and a `--name` slug cannot be
re-derived at all. An existing `project_id` in config is never rewritten, so
re-running `init` (or running it after a rename) does not change an established
slug.

**Memory notes travel with the repository:** When run inside a git repo with an
`origin` remote, `init` configures `remote.origin.fetch` so teammates'
`refs/notes/inkentry` arrives on `git fetch`, landing on the tracking ref
`refs/notes/origin/inkentry`, and does a one-time best-effort fetch of that ref
so a **single** `init` after a clone hydrates teammates' memory (the fetch is
non-fatal, so `init` still succeeds offline). Thereafter every default read path
— `memory list`, `search`, `memory show`, and `context` — folds the
tracking ref into your own notes and imports it into `memory.db` when the notes
ref has moved since the last import, so *reading* a teammate's newly-fetched
memory needs no re-`init` and no extra step. *Publishing* yours is opt-in: your
memory stays local until you install the pre-push hook, and the init output
names the command that does it. See
[Sharing memory across clones via git-notes](memory.md#sharing-memory-across-clones-via-git-notes).

**Memory survives history rewrites:** `init` also points `notes.rewriteRef` at
`refs/notes/inkentry` in the repository's own config, so memory attached to a
commit is carried onto its replacement by `git commit --amend` and `git rebase`
rather than orphaned on the old sha. This runs even without an `origin`, since
rewrites are local. Note that `git merge --squash` and cherry-picking onto a
divergent base still do not carry notes. See [Surviving history
rewrites](memory.md#surviving-history-rewrites).

If the repo already carries memory on `refs/notes/inkentry` — whether written
locally or just fetched by the step above — `init` also hydrates the new
`memory.db` from those notes: every entry not already present is imported
(idempotent, no embeddings), and it prints `Memory:  imported N entries from git
notes` when any were imported. Because the refspec is configured and the notes
ref fetched *before* this import, one `init` after a clone is enough; later
fetches are picked up automatically by the read paths. See [Project
memory](memory.md) for details.

**Example:**

```bash
cd /path/to/project
inkentry init
inkentry init --hook            # also wire up auto-index/harvest on commit
inkentry init --name acme/tools # explicit slug (e.g. no git remote)
```

---

## inkentry index

Index a codebase directory.

```
inkentry index <path> [options]
```

| Flag | Default | Description |
|------|---------|-------------|
| `-d, --db <path>` | auto | Override database path |
| `--batch-size <n>` | 0 (auto) | Cap on the embedding batch size (chunks per server request); the embed phase calibrates the actual size from measured throughput, up to this cap. 0 leaves the cap at the server's own 256-chunk limit |
| `--force` | false | Force full re-index (ignore change detection) |
| `--recount` | false | Backfill `token_count` for existing chunks and exit |
| `--no-summaries` | false | Skip the structural summary pass (the deterministic, offline composition of each chunk's `summary:` slot, and the tier-3 MMR slot for title-less chunks) |
| `--detach` | false | Re-exec in the background and return immediately (used by git hooks) |
| `--detach-embed` | false | Parse in the foreground, then run the embedding phase in a detached background process and return the prompt (`inkentry init` does this automatically) |

A plain `inkentry index` (no `--force`) re-indexes changed files (blake3 hash)
and also backfills embeddings for any already-parsed chunk that has no embedding
yet – for example if a previous run parsed the tree before the embedder had
finished loading. Unchanged, already-embedded files are skipped, so you no
longer need `--force` just to fill in missing embeddings.

Summaries are the exception: a chunk whose summary failed (say the LLM was
unreachable) is recorded as attempted rather than missing, so a plain re-run
skips it. Use `--force` to retry those.

**Summaries need an LLM, and never fail the index.** If no LLM can be reached,
`inkentry index` prints a notice naming the reason and what to do, then exits 0
with everything else (files, chunks, embeddings) indexed as usual. The three
reasons are: no LLM anywhere, an `llm_url` your running local server was not
started with, and offline mode. See
[Third-party models → How inkentry finds an LLM](third-party-models.md#how-inkentry-finds-an-llm)
for the routing rule and the exact messages. LLM routing is resolved separately
from embedding: which server embeds your code is unaffected by any of this.

If a previous run was interrupted after recording a file's new content hash
but before writing its chunks (a process kill mid-parse, for example), that
file looks up to date by hash alone but has no chunks. A plain `inkentry index`
detects this and reprocesses the file automatically; you don't need `--force`
to recover from it.

Only one `inkentry index` run is allowed per project at a time. If a run is
already in progress, starting a second one fails immediately with `index
already running (pid N), try again once it finishes` instead of writing to
the database alongside the first run.

The index also remembers the chunker configuration (currently just the
`MAX_CHUNK_TOKENS` cap) it was built under. If a plain `inkentry index` detects
that the running build's chunker config differs from what's recorded, it
prints a warning and proceeds anyway rather than failing: unchanged files keep
their old chunk boundaries until re-parsed, so the index temporarily mixes
chunk granularities. Run `inkentry index --force` to re-chunk every file under
the current config and clear the warning.

The embed phase calibrates its own batch size instead of guessing: it times a
1-chunk request, then a 4-chunk request, and sizes subsequent requests (and
their timeouts) from the observed token-weighted rate: smaller batches on slow
hardware, larger ones (up to 256 chunks, or your `--batch-size` cap if lower)
on fast hardware. Sizing by tokens rather than chunk count keeps the deadline
honest when the queue crosses from small chunks into large ones. It keeps re-measuring as the run progresses, so a rate that
drifts partway through is picked up rather than locked to the first sample.
Each batch is written to the database as soon as it completes, so an
interrupted run (timeout, machine sleep, process kill) never loses
already-embedded chunks — re-run `inkentry index` to pick up where it left off.
A batch that can't even reach the server (the local server is momentarily
unresponsive, not just slow) is retried automatically at the same batch size
with backoff, rather than being treated as a request that was too big; only
once those retries are exhausted does the run stop and wait on a manual
re-run. A batch that reaches the server but gets back `429` (the server's
bounded embed queue is already full, e.g. another `index` or a `search` is
mid-embed) is retried the same way, at the same batch size, but sleeping for
the server's own `Retry-After` instead of the fixed backoff schedule; see
`POST /index/embed` in `docs/architecture/server-api.md`.

`inkentry init` always hands the embedding pass to a detached background worker,
and `--detach-embed` opts a manual `inkentry index` run into the same behaviour:
parsing finishes in the foreground (the index is immediately usable for
full-text search) and the long embedding pass continues in the background, with
the worker waiting out a still-loading embedder rather than skipping. A plain
`inkentry index` without the flag embeds in the foreground. Run `inkentry status`
to check a background pass; it shows an "Embedding in progress" line with
searchable chunks and work percentage until every chunk is embedded. If the
background pass is interrupted, re-running `inkentry index` resumes it
(already-embedded chunks are skipped).

Add a `.inkentryignore` file (same syntax as `.gitignore`) to any directory to
exclude files from indexing. It takes higher precedence than `.gitignore`.

### File filtering

Beyond `.gitignore` and `.inkentryignore`, inkentry applies a **built-in default
exclude set** during indexing. These are files that are typically committed to
the repo (so `.gitignore` never catches them) yet carry near-zero retrieval
value while costing real embed and parse wall-clock. The defaults cover:

- **Package lockfiles** – `package-lock.json`, `npm-shrinkwrap.json`,
  `packages.lock.json`.
- **Minified assets** – `*.min.js`, `*.min.css`.
- **Vendored / generated directories** – `vendor/`, `node_modules/`,
  `third_party/`, `dist/`, `generated/`, `__generated__/`.
- **Generated / protobuf codegen** – `*.generated.*`, `*.gen.go`, `*.gen.ts`,
  `zz_generated*.go`, `*.pb.go`, `*.pb.cc`, `*.pb.h`, `*_pb2.py`,
  `*_pb2_grpc.py`, `*_pb.js`, `*_pb.d.ts`.
- **Bulk machine-data** – `schema.json`, `*.schema.json`, and translation /
  locale JSON (`translations/`, `locales/`, `locale/`, `i18n/` globs).

#### The `[index]` config table

Tune the filter with an `[index]` table in your config
(`~/.config/inkentry/config.toml`, or a project-level `.inkentry/config.toml`):

```toml
[index]
# Extra gitignore-syntax lines, layered on top of the built-in defaults.
exclude = ["*.snap", "fixtures/"]
# Whether to apply the built-in default exclude set (above). Default: true.
use_default_excludes = true
# Whether to skip files that self-declare as generated (see below). Default: true.
detect_generated = true
```

`exclude` lines are **appended after** the defaults, and matching is
last-match-wins (gitignore semantics). A project `.inkentry/config.toml`
overrides the global value **per field**: an absent key leaves the global (or
default) value in place, so setting only `exclude` in a project does not reset
`detect_generated`.

#### Re-including a filtered file

A leading `!` re-includes a path the defaults would otherwise drop:

```toml
[index]
exclude = ["!src/api/client.gen.ts"]
```

There is one **git-parity boundary**: a `!file` line cannot re-include a file
that sits under an already-excluded (pruned) parent directory. This matches git
itself. To re-include content under, say, `vendor/`, re-include the
**directory**:

```toml
[index]
exclude = ["!vendor/", "!vendor/**"]
```

Note that this re-include layer cannot reach the sensitive-file exclusion
(`.env*`, `*.pem`, private keys). Those are dropped by a separate,
non-overridable layer before the filter ever runs, so no `[index]` line can
bring them back.

#### Self-declared generated markers

When `detect_generated` is on (the default), inkentry also skips files whose head
self-declares as generated, even when the filename looks ordinary. It reads the
first 5 lines (up to 4 KiB) and looks for either:

- a literal `@generated` token, anywhere in that window; or
- the Go-style header `// Code generated by <tool>. DO NOT EDIT.`

**Known limitation:** a leading UTF-8 byte-order mark (BOM) defeats the anchored
Go `// Code generated ... DO NOT EDIT.` marker, because the BOM sits before the
`//` and breaks the line anchor. The `@generated` token is position-independent
and is unaffected. This is rare in practice and documented here for
completeness; add an explicit `exclude` glob if you hit it.

#### Why a file was skipped

Run `inkentry chunks <path>` on a file that produced no chunks: if the built-in
filter excluded it, the output names the matched pattern (or generated marker)
and prints the `[index]` re-include recipe, instead of a bare "No chunks found".

#### Filtered-count semantics

At the end of the parse phase, indexing prints a line like:

```
Filtered out 7 generated/vendored/data file(s) (built-in index filter; override in [index] of .inkentry/config.toml)
```

That count includes **file-level** excludes (a lockfile, a `*.min.js`, a
generated-marker hit) but **not** files inside a pruned directory. When a
directory such as `node_modules/` is excluded, the walk never descends into it,
so its contents are never enumerated or counted. The number therefore will not
equal the total file count of your vendored trees; it counts what the walk saw
and dropped, not what it skipped by never entering.

**Example:**

```bash
inkentry index ./myproject
inkentry index ./myproject --force --batch-size 16
```

---

## inkentry search

One unified search over **both** the code corpus and the memory corpus,
interleaved into a single ranked list. inkentry picks the best available
ranking — there is no mode to choose. When an index and server are available it
ranks by meaning (semantic/hybrid); full-text results are available immediately
after `inkentry init` parses the tree, while semantic ranking builds in the
background. During embeddings warmup a coverage notice is printed to stderr
naming the percentage and shape of embedded chunks.

`search` **requires an index.** Run in an uninitialised directory, it funnels
you to `inkentry init` rather than returning results; once `init` has parsed the
source tree, full-text search works right away.

```
inkentry search <query> [options]
```

| Flag | Default | Description |
|------|---------|-------------|
| `-l, --limit <n>` | 10 | Number of results (max 100); mutually exclusive with `--budget` |
| `--budget <n>` | — | Return best chunks fitting within this token budget |
| `--format text\|json\|jsonl` | text | Output format |
| `-g, --graph` | false | Append the queried symbol's chunk plus its 1-hop call-graph neighbours after the ranked results |
| `--graph-limit <n>` | 10 | Max graph-expanded results to add (with `--graph`) |
| `--only-code` | false | Code corpus only — the escape hatch when interleaved memory results are unwanted |
| `--only-memory` | false | Memory corpus only |
| `--only-text` | false | Full-text over the in-scope corpora, no embedding, no server needed |
| `--as-of <date>` | — | Memory-only: only entries valid at this date (point-in-time) |
| `--expand-graph` | false | Memory-only: also surface each memory result's 1-hop `relates_to` neighbours |
| `-d, --db <path>` | auto | Override database path |
| `--no-stale-check` | false | Suppress the stale-index warning |
| `--local-only` | false | Skip the cross-project dependency pass (linked projects) |

`--only-code` and `--only-memory` are mutually exclusive. Semantic ranking uses
LinearRAG: a two-stage entity-activation + personalised PageRank pipeline that
improves multi-hop recall over raw KNN. `--only-text` needs no embedding model
or server; it still runs over the full-text index, so it needs `inkentry init`
first like every `search`.

Over the **code corpus**, `--only-text` scores the query's words as
**independent terms** (BM25): a multi-word query ranks chunks that contain the
terms in **any order** — a chunk containing more of the terms ranks above one
containing fewer — rather than requiring them to appear as one contiguous
phrase. Matching is case-insensitive and not stemmed (`bursts` matches `bursts`,
not `burst`), following the FTS tokenizer.

The **memory corpus** does not behave this way. Its text matcher quotes the
whole query as a single FTS5 phrase, so a multi-word query matches only entries
containing those words adjacent and in order: `"error handling"` matches, and
`"handling error"` matches nothing. `memory timeline` shares that matcher. Text
matching over memory is therefore a narrow exact tool, not a general way to
reach an entry — reach for `memory list` or `context`, which take no query and
so return entries regardless of wording, when a phrase would miss.

`--graph` is the porcelain call-graph view: it appends the queried symbol's own
chunk and its 1-hop callers/callees after the ranked results. For exact edges as
JSONL (for scripts and agents), use `inkentry plumbing graph-edges --symbol
<name>` (or `--file <path>`).

**JSON output shape.** With `--format json`/`jsonl`, each result is a nested
envelope naming the corpus it came from — exactly one of `code`/`memory`,
matching `type`:

```json
{"type":"code","fused_rank":1,"fused_score":0.0163,"corpus_rank":1,"code":{"chunk_id":42,"file_path":"src/auth/middleware.rs","language":"rust","node_type":"function","name":"validate_token","start_line":18,"end_line":54,"content":"...","distance":0.41,"from_graph":false,"governing_specs":[],"token_count":0,"project_name":null,"project_path":null,"summary":null}}
{"type":"memory","fused_rank":2,"fused_score":0.0163,"corpus_rank":1,"memory":{"id":17,"kind":"decision","title":"Chose sqlite-vec over hnswlib","body":"...","tags":["storage"],"linked_files":[],"created_at":1786441691,"status":"active","distance":0.38}}
```

Relevance is reported as `distance` (lower is better), not a score. `fused_score`
is `1 / (RRF_K + corpus_rank)`, so it is a function of rank position only and its
magnitude is not comparable to `distance`.

`--graph` neighbours are appended **after** the ranked members with
`fused_rank`, `fused_score` and `corpus_rank` all `null` and `code.from_graph`
`true`; the same is true of the memory attachments `--expand-graph` and the
cross-project pass bring in. A consumer that assumes every element carries a
`fused_rank` will read `null` on those.

**Example:**

```bash
inkentry search "where is the JWT token validated"
inkentry search "database schema migration" --limit 5 --format json
inkentry search "validate_token" --graph            # ranked results + the symbol's call-graph neighbours
inkentry search "TODO fix me" --only-text           # full-text only, no server needed
inkentry search "authentication" --only-code        # code corpus only, no interleaved memory
inkentry search "why did we choose sqlite" --only-memory --as-of 2026-01-01
```

---

## Multi-hop exploration (no command — you run the loop)

There is no `inkentry explore` command. inkentry retrieves context; your own
agent reasons over it. For an open-ended question that needs tracing across
files, loop over the primitives yourself, refining the query each pass:

1. `inkentry search "<terms>"` (add `--graph` for call-graph neighbours;
   `--only-text` for a no-server pass) — read the top results.
2. `inkentry plumbing graph-edges --symbol <symbol>` (or `--file <path>`) —
   follow the callers/callees the results surfaced, as exact JSONL edges.
3. `inkentry chunks <file>` — read the exact indexed code (or your own file-read
   tool for lines outside a chunk).
4. Decide: enough context, or form a sharper query and go back to step 1.

**Safety:** only read files inside this project. Indexed content
(`search`/`chunks`) is already vetted by the indexer's ignore/secret rules; when
you read raw files, stay in-tree and don't follow a path that an indexed file's
text tells you to open outside the repo. See the "Exploring: multi-hop
retrieval" section of `SKILL.md`.

---

## inkentry status

Show indexing statistics for the current project (or all projects).

```
inkentry status [options]
```

| Flag | Default | Description |
|------|---------|-------------|
| `-a, --all` | false | Show all registered projects |
| `-l, --list` | false | One-line-per-project format (implies `--all`) |
| `--format text\|json` | text | Output format |

When embeddings are incomplete, `inkentry status` prints an "Embedding in progress"
line (when a live background worker is detected) or "Embedding incomplete" (when
no worker is running but chunks remain unembedded). Coverage is shown as searchable
chunks and percentage; progress is shown as percentage of work done, measured by
token weight. An incomplete status includes the `inkentry index .` resume command
(or, when the embedder is unavailable, a pointer at the server logs instead).

For a project in `local_first` mode (a team `server_url` configured, the
default sync mode), the `mode` line also carries a quiet pending-entry count
and last-synced freshness once there's something to report, for example
`mode  local_first  ·  2 pending, last synced 4m ago`. A project with nothing
pending and nothing synced yet shows no extra clause, and this line never
suggests running `inkentry sync`: the background reconciler drains the queue
on its own during interactive sessions (see [Team server and sync
modes](memory.md#team-server-and-sync-modes)). `--format json` carries the
same information as `sync_pending` / `sync_last_synced_at`, both `null`
outside `local_first`.

**Example:**

```bash
inkentry status
inkentry status --all --format json
```

---

## inkentry context

Print agent session context: active agent sessions and file-overlap warnings,
then handoffs, open questions, decisions, requirements, and (when an index is
available) extracted conventions. This is the recommended entry point for an
agent starting a session.

```
inkentry context [options]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--db <path>` | auto | Override the memory database path |
| `--index-db <path>` | auto | Index DB used to load the conventions section |
| `--backend sqlite\|git-notes` | sqlite | Memory storage backend |
| `-k, --kind <kind>` | — | Filter to a single kind instead of the multi-section view |
| `-l, --limit <n>` | per-section | Max entries per section (handoff=3, question=10, decision=10, requirement=10); mutually exclusive with `--budget` |
| `--budget <n>` (alias `--max-tokens`) | unlimited | Cap total output to this many tokens; mutually exclusive with `--limit` |
| `--path <path>` | — | Only show entries tagged with this file/directory |
| `--format text\|json` | text | Output format |
| `--no-conventions` | false | Skip the conventions section |
| `--local-only` | false | Skip cross-project dep pass; query only the primary project's memory |

Under a tight `--budget`, durable memory (decisions and requirements) is kept
ahead of ephemeral open questions when trimming to fit; the section display
order is unchanged.

When projects are linked with `inkentry link`, `context` also surfaces `locked`
or `cross-project`-tagged `decision` and `requirement` entries from linked
projects' memory stores, each labelled with its source project. Pass
`--local-only` to suppress this behaviour. See [Memory](memory.md#cross-project-visibility).

`context` opens with an **Active agent sessions** section: the active `intent`
entries other sessions have recorded, and above them a warning
(`⚠  Overlap: <file> is listed in an active intent`) for every file the current
worktree has already modified that one of those intents claims. The section is
shown only when there is a roster or a warning to display. Under `--budget` the
roster packs last (it never displaces decisions, requirements or handoffs) while
the overlap warnings are always emitted and are not counted against the token
budget. Intents are local to the project and are never surfaced across linked
projects. In `--format json` (and under `AGENT=true`) the roster is the
`["intent", …]` entry in `sections` and the warnings are a top-level `overlaps`
array of file-path strings.

**Example:**

```bash
inkentry context
inkentry context --kind decision
inkentry context --local-only      # primary project only, no dep pass
inkentry context --budget 4000     # cap total output at ~4000 tokens
AGENT=true inkentry context        # JSON for machine processing
```

---

## Graph queries (moved)

The top-level `inkentry graph <symbol>` command has been removed. Nothing is
registered with clap and it does not appear in `--help`; invoking it exits `2`
with a migration hint naming its two replacements. The code-graph capability now
lives in those two places:

- **Porcelain:** `inkentry search <symbol> --graph` appends the symbol's chunk
  and its 1-hop call-graph neighbours (imports, calls, extends/implements) after
  the ranked results.
- **Plumbing:** `inkentry plumbing graph-edges --symbol <name>` (or
  `--file <path>`) emits exact edges as JSONL for scripts and agents.

Both read the graph built by `inkentry init`; there is no `--live` scan.

**Example:**

```bash
inkentry search RagPipeline --graph
inkentry plumbing graph-edges --file src/storage/db.rs
inkentry plumbing graph-edges --symbol validate_token
```

---

## inkentry chunks

Show the raw indexed chunks for a file. Useful for debugging or providing
precise context to an agent.

```
inkentry chunks <path> [options]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--format text\|json\|jsonl` | text | Output format |
| `-d, --db <path>` | auto | Override database path |

```bash
inkentry chunks src/indexer/parser.rs
inkentry chunks src/indexer/parser.rs --format json
```

---

## inkentry languages

List all supported languages and their tree-sitter parsers.

```
inkentry languages
```

---

## inkentry link / inkentry unlink / inkentry links

Add or remove a project dependency. When linked, `inkentry search` also queries
the linked project's index, and `inkentry search`, `memory list`, and `context`
surface `locked`/`cross-project`-tagged decisions and requirements from the
linked project's memory store. `inkentry links` inspects existing links.

```
inkentry link <path>
inkentry unlink <path>
inkentry links list      # list all linked projects with status
inkentry links check     # exit 1 if any linked index is stale or missing
```

```bash
inkentry link ../shared-utils       # search this project and shared-utils together
inkentry links list
```

---

## inkentry autoclean

Remove registry entries for projects whose root path no longer exists on disk.

```
inkentry autoclean
```

---

## inkentry hooks

Manage inkentry's git hooks.

```
inkentry hooks install [--ci]
inkentry hooks install --pre-push
inkentry hooks uninstall
```

`install` writes a post-commit hook that runs `inkentry index` and
`inkentry harvest` after each commit (both `--detach` so git is not
blocked). Developers without `inkentry` installed are unaffected. `--ci` prints a
GitHub Actions workflow step instead of writing a hook.

`install --pre-push` writes a pre-push hook that publishes your memory
(`refs/notes/inkentry`) to the named remote you are pushing to, so decisions travel
with the code they describe. It merges the remote's notes into yours before
pushing (a union, so neither side is dropped) and retries a lost race up to three
times. It never blocks your push: on failure it warns on stderr and exits 0, and
it never force-pushes. Publishing is opt-in, so your memory stays local until you
install it. Publishing follows the remote's *name*, so a push that spells out a
URL instead (`git push https://… main`) pushes your code without publishing your
memory; a later `git push origin` publishes it. See
[memory.md](memory.md#sharing-memory-across-clones-via-git-notes).

The hook is a shim around [`inkentry plumbing
publish-notes`](#inkentry-plumbing), with the absolute path of the installing
binary embedded rather than a `PATH` lookup, so it keeps working under GUI git
clients. If you move or reinstall inkentry the hook fails loudly; re-run
`inkentry hooks install --pre-push` to re-resolve the path.

`install` resolves the hooks directory the way git itself does
(`git rev-parse --git-path hooks`), so it honors `core.hooksPath` if you have
one set (as husky, lefthook, and the pre-commit framework do) and follows a
linked worktree back to its shared hooks directory.

Neither hook overwrites one it did not write: if a hook of that name already
exists, `install` reports it and leaves the file alone. If the resolved hooks
directory sits inside the repository's tracked working tree (the husky/lefthook
pattern, where `core.hooksPath` points at a committed directory such as
`.husky/`), `install` refuses instead of writing there: that directory is
shared with every clone, so a silent write would commit inkentry's hook to the
whole team rather than just this machine. Add the hook to that directory
yourself, or point `core.hooksPath` at an untracked location and re-run.
Otherwise, git never clones `.git/hooks`, so installing either hook affects
only your own clone.

`uninstall` removes every hook inkentry installed, leaving any other hooks alone.

---

## inkentry server

Manage the local `inkentry-server` daemon. Runtime state lives under
`~/.local/state/inkentry/` (`server.pid`, `server.port`, `server.log`).

```
inkentry server start [--port <n>] [--bin <path>] [--db <path>]
                     [--llm-url <url>] [--llm-model <name>]
inkentry server stop
inkentry server status
inkentry server logs [-n <lines>]
```

| Subcommand | Notes |
|------------|-------|
| `start` | Idempotent; binds `--port` exactly (default 7777) on `127.0.0.1`. Reclaims a wedged prior daemon of ours instead of drifting to a new port; fails loudly if an unrelated process holds the port. A single-instance guard refuses a second server against a different `server.db`. |
| `stop` | Graceful SIGTERM, then SIGKILL escalation for an unresponsive daemon; reports success only once the process is confirmed gone. |
| `status` | Print PID, port, instance id, and uptime |
| `logs` | Print the last N lines of the server log (`-n`, default 50) |

```bash
inkentry server start
inkentry server status
inkentry server logs -n 100
inkentry server stop
```

### LLM configuration for the daemon

| Flag | Notes |
|------|-------|
| `--llm-url <url>` | Chat-completions endpoint this daemon serves LLM features from. Overrides `INKENTRY_LLM_URL` and `llm_url` in the personal config, for this daemon only. |
| `--llm-model <name>` | Model name this daemon sends to that endpoint. Overrides `INKENTRY_LLM_MODEL` and `llm_model`. Ignored without an endpoint. |

Set `llm_url` in the personal config instead if you want every daemon to get
it; these flags are the per-launch override. A blank flag value (`--llm-url ""`)
is discarded rather than treated as an override, so the configured value still
applies.

**There is no `--llm-key` flag here, deliberately.** Arguments are readable by
any user on the machine through the process table, so the endpoint's credential
is never accepted or emitted as one. Store it with
[`inkentry auth set-key --llm`](#inkentry-auth) or set `INKENTRY_LLM_KEY`; the CLI
resolves it at spawn time and passes it to the daemon in its environment. The
daemon never reads the OS secret store itself.

A running daemon keeps the configuration it was started with. `inkentry server
stop && inkentry server start` after any change. See
[Third-party models](third-party-models.md#configuring-an-external-llm-endpoint)
for the full precedence table.

---

## inkentry login

Authenticate with spelunk.cloud using a browser-based device login. `inkentry
login` prints a verification URL and a short user code; open the URL, enter the
code, and approve the sign-in in your browser. On success, short-lived tokens
are stored in your config and refreshed automatically in the background, so you
do not need to log in again until the refresh token expires.

```
inkentry login [--org <slug>] [--cloud-url <url>]
```

| Flag | Notes |
|------|-------|
| `--org <slug>` | After the device login yields a token, silently re-scope the session to this org (login-then-switch). If you are already logged in with a stored refresh token, re-scopes without a new device login. Multi-org accounts choose their org on the browser-hosted approval page during the device flow itself. |
| `--cloud-url <url>` | Override the cloud API URL (default `https://api.spelunk.cloud`; also settable via `INKENTRY_CLOUD_URL`). |

```bash
inkentry login
inkentry login --org acme
```

**No `--org`, and the device login itself didn't scope you to an org** (WorkOS
doesn't auto-select an org even for single-org accounts): `inkentry login`
resolves one for you instead of leaving a session that needs a follow-up
`inkentry org switch`.

- Exactly one org on your account → selected silently.
- Multiple orgs, on a TTY → an interactive `name (slug)` selector.
- Multiple orgs, non-TTY (CI/agent shell) → errors with an actionable "pass
  `--org <slug>`" message and a non-zero exit; never hangs on a prompt.
- Zero orgs → a clear onboarding message and a non-zero exit; no dangling
  no-org session is persisted.

Tokens are written to the `[auth]` table of `~/.config/inkentry/config.toml`
(file mode `0600`). Existing setups that use a self-hosted server key (stored via
`inkentry auth set-key`, or the `INKENTRY_SERVER_KEY` environment variable) keep
working unchanged; `INKENTRY_SERVER_KEY` continues to take precedence, which is
handy for CI. See `inkentry auth` below for the self-hosted credential itself;
`inkentry login` only ever manages the `[auth]` cloud token pair.

### Where the self-hosted server key is stored

See [`inkentry auth`](#inkentry-auth) below for the full per-server credential
story (ADR-071). In short: a self-hosted server's bearer key is **not** kept in
plaintext anywhere. It lives in your operating system's secret store:

- **macOS**: Keychain
- **Linux**: Secret Service (libsecret / `org.freedesktop.secrets`)
- **Windows**: Credential Manager

keyed by the server's origin, so keys for two different self-hosted servers
never collide. A flat `server_key` from an install predating this scheme is
migrated in automatically the first time it's needed for a given server; no
action required. A `server_key` line in a project's checked-in
`.inkentry/config.toml` is no longer read at all (it was a plaintext-in-a-committed-file
footgun); if a project config still has that line, remove it and have each
developer run `inkentry auth set-key --server <url>` instead.

**Headless / CI / containers.** When no OS keychain backend is available, the
credential never causes a hard failure:

- `INKENTRY_SERVER_KEY` remains the non-interactive escape hatch and always takes
  precedence: set it in CI and you never touch the keychain.
- Otherwise inkentry falls back to an owner-only (`0600`) file at
  `~/.config/inkentry/secrets.toml`.

`INKENTRY_SECRET_STORE` pins the backend explicitly:

| Value | Behaviour |
|-------|-----------|
| unset / `auto` | Prefer the OS keychain; fall back to the file store when none is available (default). |
| `keychain` | Require the OS keychain; error if it is unavailable. |
| `file` | Always use the `secrets.toml` file store (e.g. a container that mounts secrets from elsewhere). |

The credential is never logged.

---

## inkentry auth

Store credentials in the OS secret store: the per-server bearers a self-hosted
`server_url` resolves through (ADR-071), and the credential for a configured
`llm_url` endpoint. Distinct from `inkentry login`, which manages the
spelunk.cloud `[auth]` token pair.

```
inkentry auth set-key (--server <url> | --llm)
inkentry auth list-servers
```

| Subcommand | Notes |
|------------|-------|
| `set-key --server <url>` | Store a bearer key for the given server, keyed by its origin (scheme + host + non-default port). |
| `set-key --llm` | Store the credential for the configured `llm_url` chat-completions endpoint. A single entry: there is one LLM endpoint, not a set of them. |
| `list-servers` | Print every server origin with a stored key, one per line. Never prints key material. Notes if a legacy flat key is still present and pending migration. It lists servers, so a stored LLM credential does not appear. |

`--server` and `--llm` are mutually exclusive, and exactly one is required.
Either way the credential is read from stdin if piped, otherwise from an
interactive prompt. **It is never accepted as a flag value or a positional
argument**, because arguments are readable by any user on the machine through
the process table. Nothing about either command prints key material, and
neither writes a credential into `config.toml`.

```bash
echo "$SERVER_KEY" | inkentry auth set-key --server https://inkentry.internal.example.com
inkentry auth list-servers

inkentry auth set-key --llm                 # prompts
echo "$LLM_KEY" | inkentry auth set-key --llm
```

Resolution precedence for a given request's `server_url`: the `INKENTRY_SERVER_KEY`
environment variable (if set, always wins, regardless of origin) takes priority
over the per-origin store; a spelunk.cloud origin instead resolves through the
`[auth]` token pair from `inkentry login`. This lets CI pin a single key for the
one server it talks to without touching the keychain, while a developer's
machine holds separate keys per self-hosted server.

The LLM credential resolves as `INKENTRY_LLM_KEY` > this stored entry > unset,
and only on the code path that starts a local daemon: no other command reads
it, so none of them authorize against your keychain for it. The daemon receives
it in its environment and never opens the secret store itself. See
[Third-party models](third-party-models.md#security-properties).

---

## inkentry org

Manage the active organization for an authenticated session.

```
inkentry org switch <slug|uuid>
```

`inkentry org switch` re-scopes your session to another organization you belong
to, reusing the stored credentials — no new device login is required. Accepts an
org slug or its UUID.

```bash
inkentry org switch acme
```

---

## inkentry logout

Remove stored spelunk.cloud credentials. Bare `inkentry logout` clears **only**
the `[auth]` token pair written by `inkentry login`; it does not touch any
self-hosted server key, so recovering from a broken cloud login never costs
you the keys you use on other projects (ADR-071 D3). Clearing server keys is a
separate, explicit action:

```
inkentry logout [--servers | --server <url>]
```

| Flag | Notes |
|------|-------|
| (none) | Clears only the `[auth]` cloud token pair. If any server keys are still stored, prints how many and how to clear them. |
| `--servers` | Also clears every stored server key: the per-origin map and any legacy flat entry. |
| `--server <url>` | Also clears just the stored key for that one server's origin. Mutually exclusive with `--servers`. |

```bash
inkentry logout
inkentry logout --server https://inkentry.internal.example.com
inkentry logout --servers
```

---

## inkentry harvest

Capture memory from git history and session logs. Harvest is two things wearing
one command: a one-time **backfill** over a range of history, and the
**continuous** capture the post-commit hook runs after every commit. It sends
commit messages (or session logs) to the LLM, extracts significant entries, and
stores them in the project's memory, skipping near-duplicates.

```
inkentry harvest [--git-range HEAD~10..HEAD | --branch <ref>]
                 [--source git|claude-code|failures]
                 [--batch-size 3] [--history-file <path>] [--since <date>]
                 [--confirm] [--detach]
                 [--db <path>] [--backend sqlite|git-notes]
```

| Flag | Default | Notes |
|---|---|---|
| `--git-range <REV>` | `HEAD~10..HEAD` | conflicts with `--branch`; the default `HEAD~N..HEAD` shape clamps to the commits that exist, so a shallow repo never errors |
| `--branch <REF>` | — | conflicts with `--git-range`; walks the full history of the ref |
| `--source <S>` | `git` | one of `git`, `claude-code`, `failures` |
| `--batch-size <N>` | `3` | commits/sessions per LLM request (min 1) |
| `--history-file <PATH>` | `~/.claude/history.jsonl` | `claude-code` source only |
| `--since <DATE>` | — | `claude-code` source only |
| `--confirm` | false | required to read the history file (`claude-code`) |
| `--detach` | false | re-exec in the background and return immediately (used by the git hook) |
| `--db <PATH>` | auto-detect | memory-store override |
| `--backend <sqlite\|git-notes>` | `sqlite` | storage backend |

```bash
inkentry harvest                                   # backfill HEAD~10..HEAD
inkentry harvest --git-range v0.1.0..HEAD          # a custom range
inkentry harvest --branch main                     # full branch history
inkentry harvest --source claude-code --confirm    # from ~/.claude/history.jsonl
inkentry harvest --source failures                 # antipatterns from revert/bugfix commits
```

**Harvest needs an LLM.** All three sources use it for extraction, and the
command fails with a message naming the reason and what to do when none is
reachable. See
[Third-party models → How inkentry finds an LLM](third-party-models.md#how-inkentry-finds-an-llm).
Extraction and the dedup embedding resolve independently and can land on
different servers.

`inkentry memory harvest` is a deprecated, still-working alias of this command
for one release: it prints a deprecation warning on stderr and otherwise behaves
identically.

---

## inkentry memory

Store and query project context, decisions, and requirements. See
[Memory](memory.md) for full documentation.

```
inkentry memory add --title "..." [--body "..."] [--kind decision] [--tags auth,db] [--files src/auth.rs]
inkentry memory add --from-url <url> [--title "override"] [--kind requirement]
inkentry memory list [--kind decision] [--limit 20] [--format text|json] [--local-only]
inkentry memory show <id> [--format text|json]
inkentry memory harvest [...]                # deprecated alias of `inkentry harvest`
inkentry memory failures                    # list all antipatterns
inkentry memory archive <id>
inkentry memory supersede <id> --title "..." # archive old, add replacement
inkentry memory timeline <topic>
inkentry memory graph <id>
inkentry memory sync                         # two-way: push local + pull remote (see `inkentry sync`)
inkentry memory reconcile [--dry-run] [--all-projects] [--source-db <path>]
inkentry memory dedupe [--dry-run] [--format text|json]
inkentry memory reindex [--force] [--include-archived] [--dry-run] [--format text|json]
```

All `memory` subcommands accept `--backend sqlite|git-notes` (default `sqlite`)
and `--db <path>`.

`inkentry search` and `memory list` accept `--local-only` to skip the
cross-project dep pass (see [Cross-project visibility](memory.md#cross-project-visibility)).
Results from linked projects carry a `[from: <project>]` badge in text output
and `source_project` / `source_project_path` fields in JSON.

**Harvest is now the top-level [`inkentry harvest`](#inkentry-harvest).**
`inkentry memory harvest` remains as a deprecated, still-working alias for one
release; it prints a deprecation warning on stderr and points you at
`inkentry harvest`. See that section for the full flag reference.

**Memory kinds:** `decision` · `context` · `requirement` · `note` · `intent` ·
`answer` · `handoff` · `question` · `antipattern`

`inkentry memory failures` is a shortcut for `inkentry memory list --kind antipattern`.

**git-notes write-through:** when `store_in_git_notes` is true (the default),
`inkentry memory add` also appends the entry to `refs/notes/inkentry` on `HEAD`,
so memory travels with the code. The repo is resolved from the database in
use, the `--db <path>` directory when given, otherwise the discovered
`.inkentry` project, not the invocation's working directory: pointing `--db`
at another project's database writes notes to that project's repo. Outside a
git repo this is a graceful no-op. Concurrent writes are serialized by a
cross-process lock, and a write that
cannot take the lock in time fails rather than risk erasing a concurrent
writer's entry: `memory add` warns on stderr that the entry is stored locally
but will not travel with the repo (pre-`init`, where git notes is the sole
store, it fails instead), and retrying the command is the remedy.

**Entry identity:** entries are identified by a SHA-256 over exactly their
`kind`, `title`, and `body`, so the same decision recorded on two machines
converges on one identity. `memory reconcile` and the `inkentry init` git-notes
import dedup on it: entries with identical text collapse into one even when
their creation time, tags, or linked files differ, and the survivor carries the
union of the tags and linked files. The `id` shown by `memory list` is a local
row number, not this identity. See [Entry identity](memory.md#project-memory).
Existing duplicate rows already resident in `memory.db` are never collapsed
automatically; use `inkentry memory dedupe` to do that explicitly (see
[Collapsing duplicate entries already in memory.db](memory.md#collapsing-duplicate-entries-already-in-memorydb)).
Once a store's duplicates are cleared and its `entity_id` index is promoted to
UNIQUE, a plain `memory add` for byte-identical content no longer errors: it
reuses the existing entry and prints `Already recorded as ...` instead of
`Stored ...`. The same reuse applies to `inkentry sync` / `inkentry plumbing pull`:
a pulled entry matching an existing local row's identity merges into that row
(adopting the remote id, archiving it if the pulled entry is archived) instead
of adding a duplicate, so the printed pull count reflects only genuinely new
rows. Pre-promotion, a pull can still add a distinct row alongside matching
local content, same as `memory add`.

**Backfilling missing embeddings:** a note's semantic vector is minted at
`memory add` time, and again by `inkentry sync` / `inkentry plumbing push` for
any entry in the set they are about to push that still lacks one. A note that misses both, added
while the embedder was down and never pushed, or carried
through the 768→896 embedding-dimension upgrade (which drops the old vectors),
stays present-but-unembedded: still listed by `memory list` and `context`, but
absent from the semantic ranking of `inkentry search`. Text search is not a
dependable fallback for it — the memory text matcher requires the query as a
contiguous phrase (see [`inkentry search`](#inkentry-search)).
`inkentry memory reindex` re-embeds those notes against the local
embedder (the same path
`memory add` uses), so it needs a reachable embedder and exits non-zero if none
is; it commits each vector as it goes, so an interrupted run resumes on re-run.
`--force` re-embeds every active note (replacing existing vectors), `--include-archived`
also covers archived notes, and `--dry-run` reports counts without writing or
contacting the embedder. This is separate from `inkentry index`, which re-embeds
the code index. See
[Backfilling missing embeddings](memory.md#backfilling-missing-embeddings).

---

## inkentry import

```bash
inkentry import project.dump
inkentry import project.dump --format json
inkentry import project.dump --no-embed
```

Read a [portable dump](dump-format.md) into this project's stores: memory
entries and the relationships between them, plus any projects and recorded
commands the dump carries. This is how an existing store crosses into a store
this build created — nothing is opened in place.

The dump is read and checked **whole** before anything is written. Its record
counts and its digest are both recomputed, and any mismatch — a truncated file,
a single altered byte, records in a different order, a relationship naming an
entity that is not there, a record kind this build does not know, two entries
claiming one `uuid` or one `remote_id`, an entry carrying a blank one — refuses
the entire file. There is no
partial import, because importing most of a damaged dump and saying nothing is
the worst available outcome. The refusal covers **every** store the import
would touch: memory entries, the project registry and the recorded-command
table are written under one refusal, so a dump that is rejected leaves all
three exactly as it found them.

Entries arriving with an identifier keep it. Entries without one are assigned a
UUIDv7 seeded from their own creation time, so a back catalogue keeps its
ordering instead of being stamped with the instant it was imported.

**Import writes to the local memory store, so it refuses to run when that is not
where this project's memory lives.** Under `mode = "cloud_first"` with a
`server_url`, the server is the store of record and every memory command reads
it; a local write there would report success and leave the whole dump in a file
the project never opens. Import into the local store first
(`INKENTRY_MODE=local_first`), then `inkentry sync` to carry it up.

**Entries are identified by their content, so the count is of rows, not of
records.** A memory entry's convergence key is computed over its kind, title
and body, and the store declares that key unique — so two records carrying one
key are one entry, whatever the dump says. Two harvested entries with the same
text from different commits are exactly that case, and they differ only in
`source_ref`. The earliest-created one survives (its tags and linked files
gaining the other's), and the summary reports the fold separately from the
entries that landed rather than counting both. Records whose entry is already
in this store — a second run of the same import — are reported apart again, so
"imported" never includes something that was already there.

**Imported entries travel with the repository.** Once the rows are committed,
the import appends them to `refs/notes/inkentry` — the same
[git-notes carrier](memory.md#sharing-memory-across-clones-via-git-notes)
`memory add` writes through
to — so a teammate cloning the repo gets the imported decisions along with the
code. Each record carries the dump's own `created_at`, status and `entity_id`
verbatim, and a `supersedes` relationship travels as the successor's
`entity_id`, so the log converges the same way on every machine that receives
it. Entries the repo's notes ref already holds are **not** written to it again,
so re-importing a dump that came off this carrier adds nothing; the summary
reports that count separately. The carry is best-effort — the local store is
the store of record and already holds the entries — so a repo without a commit
to anchor a note to, or a `--db` outside any repository, imports normally and a
failure is a warning rather than a refusal. Set `store_in_git_notes = false` to
turn the carry off.

**Embeddings are not carried in a dump**, so imported entries are not in
semantic search until they are embedded. The import runs its writes in one
transaction with no embedding inside it, then runs `memory reindex`'s pass
afterwards. If no embedder is reachable the import still succeeds and reports
how many entries are waiting, along with the command that finishes the job;
`inkentry status` carries the same count. Pass `--no-embed` to skip the attempt
and just be told.

This matters more than it looks: the default search mode is hybrid, so
unembedded entries are still returned by the full-text half. Semantic recall
degrades while the store looks like it is working.

| Flag | Meaning |
|---|---|
| `--db <PATH>` | Memory database to import into (overrides auto-detect) |
| `--no-embed` | Import without embedding; still reports what is pending |
| `--format <FMT>` | `text` (default) or `json` |

---

## inkentry sync

Two-way sync (shorthand for `inkentry memory sync`): push your local memory
entries to the configured server **and** pull remote entries into the local
`memory.db`, so a team converges on one shared memory. Code never leaves the
machine; only memory does. Requires a configured `server_url`.

Under the default `local_first` mode, a background reconciler already drains
unpushed entries and pulls new ones during interactive sessions, so this
command is no longer required in the normal day-to-day path: `inkentry
status` shows what's still pending. Reach for `inkentry sync` when you want an
immediate, synchronous reconcile instead of waiting on the background drain,
or in a non-interactive context (CI, a script, a git hook) where the
background reconciler never auto-starts.

```
inkentry sync [--project <slug>] [--source <path>] [--include-archived]
```

| Flag | Notes |
|------|-------|
| `--project <slug>` | Project slug to sync into. Required on first sync when no `project_id` is configured: the server lazily creates the project from this slug, and repeat syncs with the same slug reuse it. Overrides a configured `project_id` when both are present. Never auto-derived from the folder name or git remote; with neither flag nor a configured `project_id`, sync halts with an actionable message pointing at `--project`. |
| `--source <path>` | Local `memory.db` to sync (default: the auto-detected project `memory.db`). |
| `--include-archived` | Include archived entries in the push, propagating tombstones. |

For a one-directional transfer (seeding, CI), use the plumbing forms
`inkentry plumbing push` (local → server) or `inkentry plumbing pull`
(server → local); each emits a single JSONL report. The former porcelain
`inkentry memory push` / `inkentry memory pull` have been removed; there is no
alias, so invoking them errors as an unknown subcommand.

**The push embeds what it pushes.** Before the batch is built, both `inkentry
sync` and `inkentry plumbing push` embed every entry in the push set that has no
usable local vector, through the local loopback embedder and using the same
document text `inkentry memory reindex` uses, and commit each vector to
`memory.db`. A pushed entry is then findable by semantic `search` locally
without a separate `reindex`. This changes what is stored locally, not what is
sent: `kind`, `title`, and `body` are serialised on every push and always were,
and the vector fields are additive. The step is skipped in `cloud_first` mode
with a `server_url` set, the same condition `memory reindex` declines under. With
no local embedder reachable the push still completes, text-only, with the exit
code it always had, and prints one warning naming how many entries went out
without a local embedding and that `inkentry memory reindex` is the cure. The
summary line reports the local embed count separately from `created` /
`skipped` / `failed`, for example `Sync complete. Pushed 4 entries (created 4,
skipped 0), applied 1 new remote entries. Embedded 2 locally.` Entries already
synced, and entries arriving via a pull, are outside the push set and are
not embedded by this step. See [Backfilling missing
embeddings](memory.md#backfilling-missing-embeddings).

---

## inkentry plumbing

Low-level commands for agents and scripts. All emit JSONL and exit non-zero on
error (exit 1 for "no results", exit 2 for errors). See
[plumbing-and-porcelain.md](plumbing-and-porcelain.md). These field names,
types, and exit codes are semver-bound and test-enforced; the
[stability contract](stability.md) says exactly what may change and what may
not.

```
inkentry plumbing cat-chunks <file>     # indexed chunks for a file
inkentry plumbing ls-files              # all indexed files
inkentry plumbing parse-file <file>     # parse + chunk without storing
inkentry plumbing hash-file <file>      # blake3 hash + index currency
inkentry plumbing knn <query>           # KNN vector search
inkentry plumbing embed                 # read stdin lines, emit vectors
inkentry plumbing graph-edges           # code graph edges
inkentry plumbing read-memory           # memory entries as JSONL
inkentry plumbing publish-notes [remote]  # publish memory notes to a remote
inkentry plumbing push [--source <path>] [--include-archived]  # one-way local -> team server
inkentry plumbing pull                  # one-way team server -> local
```

`push` and `pull` are the one-way memory transfers, for seeding a server or
running in CI; for everyday two-way convergence use `inkentry sync`. Both emit a
single JSONL report and, like `publish-notes`, **write** and perform **network
I/O** — and they require an explicitly-configured team `server_url` (never the
inference loopback). Their exit `1` means an empty delta (nothing new pushed, or
nothing new pulled) and still emits the report; only exit `2` (the run did not
complete: setup, network, auth, a total failure, or an interruption) leaves
stdout empty.

`publish-notes` fetches the remote's `refs/notes/inkentry` onto the tracking ref,
merges it into yours with `cat_sort_uniq`, and pushes the result (defaulting to
`origin`). It is the flow behind `inkentry hooks install --pre-push`, which is the
command to reach for; this one is the plumbing underneath it.

Unlike the rest of the namespace it **writes** and performs **network I/O**, so
"plumbing is read-only" does not hold for it. It exits 2 on a publish failure
like any other plumbing command; `--best-effort` downgrades that to a warning on
stderr and exit 0, which is what the hook uses so a failed publish can never cost
you your `git push`.

If another process holds the notes lock, the merge cannot run, so the publish is
skipped rather than pushed unmerged. That is reported on stderr and as
`"skipped":"lock_unavailable"` on stdout, and exits 0 whether or not
`--best-effort` was passed. Nothing is lost: your records stay on the local ref
and publish on your next push.

---

## Environment variables

| Variable | Effect |
|----------|--------|
| `AGENT=true` | Force JSON output for commands that support it |
| `NO_COLOR` | Any non-empty value disables colored output, overriding the `auto` default (`--color=always` still overrides `NO_COLOR`) |
| `INKENTRY_NO_SERVER=1` | Never autostart or use a server (fully offline / no-server mode) |
| `INKENTRY_SERVER_URL` | Point the CLI at a specific server URL |
| `INKENTRY_CLOUD_URL` | Override the spelunk.cloud API URL used by `login` / `org` (default `https://api.spelunk.cloud`) |
| `INKENTRY_SERVER_KEY` | Static credential for a team/self-hosted server; takes precedence over the keychain-stored credential and `login` tokens (the non-interactive escape hatch for CI / headless) |
| `INKENTRY_SERVER_CA` | Path to a PEM CA bundle to trust for a `INKENTRY_SERVER_URL` whose certificate is signed by an internal or self-signed CA. Added as a trust anchor on top of the built-in roots; TLS verification stays on (no insecure mode). Overrides `server_ca` in `config.toml`. |
| `INKENTRY_LLM_URL` | Chat-completions endpoint a locally started daemon serves LLM features from; overrides `llm_url` in the personal config. Set but empty blanks the configured value rather than falling through to it. |
| `INKENTRY_LLM_MODEL` | Model name sent to that endpoint; overrides `llm_model`. Same empty-value rule. |
| `INKENTRY_LLM_KEY` | Credential for that endpoint; takes precedence over the entry stored by `inkentry auth set-key --llm`. Not a `config.toml` field. Blank reads as unset. |
| `INKENTRY_SECRET_STORE` | Secret-store backend: `auto` (default — keychain, file fallback), `keychain` (require the OS keychain), or `file` (force `~/.config/inkentry/secrets.toml`) |
| `INKENTRY_CONFIG_DIR` | Override the whole `~/.config/inkentry/` directory (not just the config file), same as `-c, --config` but for the entire directory |
| `INKENTRY_STATE_DIR` | Override the runtime state directory (default `~/.local/state/inkentry/`) that holds the server's pid/port/log/db files and the embed worker's pid/baseline files. Every reader and writer resolves through this same variable, so it is safe to redirect wholesale (useful for test isolation, containers, or a non-default `HOME`). |
| `RUST_LOG=debug` | Enable verbose logging |
| `EDITOR` / `VISUAL` | Editor opened by `inkentry memory add` when `--body` is omitted |
