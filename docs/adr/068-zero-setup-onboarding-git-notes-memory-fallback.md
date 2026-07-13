# ADR-068: Keep zero-setup onboarding; add a git-notes memory fallback before `init`

**Date:** 2026-07-11
**Deciders:** founder (Johan); architect
**Relationship to prior ADRs:** completes the product-direction decision that
[ADR-067](067-fail-closed-no-local-project.md) explicitly deferred to
spelunk-oss^134 ("Broader UX direction is out of scope … a separate product
decision"). Builds on ADR-067's isolation floor but **narrows** its
"fail-closed for memory" posture: where ADR-067 refused all memory operations
without a local `.spelunk/` project, this ADR routes `memory add` / `memory
list` to the git-notes backend when the working directory is inside a git repo,
and fails only when there is neither a project DB nor a git repo. Keeps
ADR-004's inference-vs-storage split intact.

## Context

A UAT "walk-the-store" session ran every command in the opening two sections of
`getting-started.mdx` ("First commands — no setup needed" and "Search and memory
together") against a real, populated, previously-un-`init`'d checkout of
`getlago/lago`. The doc's promises are explicit and load-bearing:

- Front matter: *"No API keys or servers required to start."*
- *"First commands — no setup needed … Open a terminal inside any git
  repository. Nothing to configure."*
- *"Memory is stored in git notes — no database, no server, no setup. It travels
  with the repo."*

Several of the walked commands did not behave as advertised, and the doc led
with invocations that are not the zero-setup path. The fix is **not** to retreat
from the zero-setup promise — it is to make the promise true by pointing the
docs at the commands that genuinely work with no `init`, and to make `memory
add` / `memory list` honour the "stored in git notes — travels with the repo"
claim before `init` as well.

### What actually runs before `init` (on `main`, after oss^147)

`oss^147` (merged) closed the ADR-067 global-store residual: `graph`, `chunks`,
`check`, and `explore` now route through `require_project_db` and no longer fall
back to the machine-global `~/.config/spelunk/index.db`. Nothing reads that
global store implicitly any more. Current behaviour in an un-`init`'d, populated
repo:

| Doc command | Real behaviour with no `.spelunk/` project |
|---|---|
| `spelunk search "…"` (**auto**, no `--mode`) | Degrades to `search_live` (ast-grep structural scan) when no index is present. This is the genuine zero-setup search surface; the doc never showed it. |
| `spelunk search "…" --mode text` | Explicit FTS-over-index mode. Fails closed via `require_project_db` with *"no spelunk project here. Run 'spelunk init' first"*. Correct for an index-only mode — the doc simply led with the wrong invocation. |
| `spelunk graph <symbol>` | Falls to the ast-grep `symbol($$$)` live call-site scan (exact, unranked) when no index opens; no longer reads any global store (oss^147). Zero results print `No scannable source files under this directory` (empty/umbrella dir) or `No callers found for '<symbol>' (live scan)` (source present, no match); the live scan never suggests `init` (oss^127). |
| `spelunk graph <file-path>` / `chunks` / `check` / `explore` | Index-backed. Refuse with *"no spelunk project here. Run 'spelunk init' first"* (post-^147). |
| `spelunk memory add` / `list` / `search` | **Currently** all fail closed pre-`init`: `memory/mod.rs:377–380` calls `require_project_db(&cfg.db_path, false)` and bails without a `.spelunk/` dir. This ADR changes `add` / `list` (see D3). |

### The architectural fault line

The code already draws the line the product decision needs:

- **Working-tree-only, index-free, global-store-free** commands: `search "…"`
  (auto→ast-grep), `search --mode ast-grep`, `graph --live` / `graph <symbol>`'s
  ast-grep fallback. ADR-067 D1 exempts ast-grep because it "touches no index and
  no global store," and oss^147 removed the residual global read. These are the
  real zero-setup surface and are safe to run anywhere.
- **Index-backed** commands: `search --mode text` (FTS), `graph <file>`,
  `chunks`, `check`, `explore`. These need `spelunk index` and correctly refuse
  without a project.
- **Memory**: has two local-only backends already — the default SQLite
  `memory.db` (needs a `.spelunk/` project) and the **git-notes** backend
  (`crates/spelunk-core/src/storage/git_notes/`, `GitNotesBackend`,
  `append_to_git_notes`, ref `refs/notes/spelunk`), today reachable only via the
  explicit `--backend git-notes` flag. The git-notes backend stores each entry
  as a JSON line on the HEAD commit's note and supports `add`, `list`, `get`,
  `count`, `archive`; it returns a clear `BackendUnsupported` for the
  embedding-dependent operations (`search`, `search_hybrid`, `search_text`,
  `search_timeline`) and for `supersede` / graph edges.

That second point is the opening this decision uses: a git-notes-backed
`memory add` / `list` needs **no index, no server, and no `init`** — only a git
repo — which is exactly the "stored in git notes — travels with the repo"
promise the doc already makes. It also lets an engineer borrowed for a quick fix
record a result without indexing a whole (possibly huge) project.

## Decision

**Keep zero-setup as the headline promise. Do not lead onboarding with
`spelunk init`. Make `memory add` / `memory list` work before `init` via the
git-notes backend when a git repo is available, and document only the commands
that genuinely work without an index DB.**

### D1 — keep the zero-setup promise (reverses the earlier "lead with `init`")

`getting-started.mdx` keeps "no setup needed / no API keys or servers to start"
as the headline. `spelunk init` is **not** promoted to step 1, and the
zero-setup pitch is **not** retired. The opening section showcases the commands
that actually run with no `init` (D2); index-backed and semantic examples are
clearly marked as "after `spelunk init`," but they do not displace the
zero-setup framing at the top.

### D2 — a small, honestly-scoped index-free surface, documented as such

`getting-started` is ordered in sections. **The first section is pre-`init`**
and showcases **only** the commands below (the index-free surface). It is
followed by the **`spelunk init`** section, and only *after* that do the
index-backed commands appear (indexed `search` / `--mode text`, the full indexed
`graph`, semantic search, and DB-backed `memory`). "Index-free only" applies to
that opening pre-`init` section, not to the whole document. Keep exactly these
working with **no** `init`:

- `spelunk search "<query>"` (auto mode → ast-grep live structural scan) and
  `spelunk search --mode ast-grep`.
- `spelunk graph <symbol>` (ast-grep live call-site fallback) / `graph --live`.
- `spelunk memory add` and `spelunk memory list` (git-notes backend, per D3).

The docs must showcase the invocations that work as written — bare `search "…"`,
not `--mode text`; `graph <symbol>` / `--live`, not `graph <file>` — and frame
the code-search/graph pieces as a *live structural scan*, not the full indexed
graph/search. Working-tree-only is a hard constraint: none of these may read a
machine-global store (oss^147 already guarantees this for `graph`).

### D3 — git-notes memory fallback for `add` / `list` before `init` (new feature)

When there is no configured project DB (pre-`init`) but the current directory is
inside a git repository, `memory add` and `memory list` record to (and read back
from) git notes on the nearest repo instead of failing. This lives the "memory
goes with the repo" promise and gives an engineer a place to record a result,
and via `list` visibility into what is stored, without indexing the project.

> **Revised 2026-07-13** (founder correction from the #580 review): the first
> draft of this section resolved backend selection as a precedence ladder whose
> pre-`init` rung made git notes the *store of record* and **skipped** the
> existing git-notes write-through ("no double write"). That reshaped store
> priority for no reason. The corrected model below does not touch store
> priority: it leans on the universal `store_in_git_notes` write-through that
> already runs on every `memory add`, and only stops `add` / `list` from failing
> closed before `init`. #580 (^154) implemented the superseded framing and is
> re-scoped to this model.

**The carrier already exists.** `memory add` already appends every new entry as
a line of JSON to `refs/notes/spelunk` on HEAD, *in addition to* its primary
SQLite write, whenever `store_in_git_notes` is true, which is the default
(`Config::store_in_git_notes` in `config.rs`). That append (`append_to_git_notes`
in `memory/add.rs`) is best-effort and non-fatal, and it is the mechanism behind
the product's "memory travels with code" messaging. The only thing that stops it
before `init` is that `memory/mod.rs` resolves the store via
`require_project_db(&cfg.db_path, false)` for **every** subcommand and bails
before `add` ever runs.

**Store roles, reconciled with [ADR-004](004-unified-memory-storage.md).** Git
notes (`refs/notes/spelunk`) are the durable carrier that travels with the repo;
the local SQLite `memory.db` is the queryable index over that carrier. It holds
the embeddings semantic `memory search` needs and is hydrated from the notes
(the `init`-time git-notes import, ^155, is exactly that hydration step). This
does **not** contradict ADR-004. ADR-004 resolved a *local-vs-server*
split-brain: it makes `memory.db` canonical *relative to a shared team server*
and holds that memory stays local unless an explicit team `server_url` relocates
the store of record. Both `memory.db` and `refs/notes/spelunk` are local to the
repo, so neither leaves the machine and that clause is untouched. ADR-004
adjudicates local vs server, not the relationship between two local carriers;
describing git notes as the carrier and `memory.db` as the index over it sits
entirely inside ADR-004's "memory stays local" domain.

**What changes (`add` / `list` before `init`).** Do **not** fail closed, and do
**not** reshape store priority: the ADR-004 backend order for the primary store
is unchanged (explicit `--backend git-notes` selects git notes; an explicit team
`server_url` selects the remote backend; otherwise the local `memory.db`). The
single change is at the pre-dispatch `require_project_db` bail:

- **`memory add`:** with no `.spelunk/` project but CWD inside a git repo
  (`git rev-parse HEAD` succeeds), skip the absent SQLite primary write and let
  the universal write-through carry the entry to git notes through
  `append_to_git_notes`, the very call that already runs post-`init`. There is
  **one write path** pre- and post-`init`, so every note in `refs/notes/spelunk`
  carries an identical record shape (`schema_version`, timestamps, `remote_id`).
  That uniformity is the robustness win over a separate `GitNotesBackend.add`
  path: it keeps the `init`-time import (^155) and plain
  `git notes --ref=spelunk …` inspection consistent.
- **`memory list`:** with no `memory.db`, read the entries back from
  `refs/notes/spelunk`.
- **Fail only** when there is neither a project DB nor a git repo, with the same
  message as the first draft: *"no spelunk project here, and not inside a git
  repo. Run 'spelunk init' first, or run inside a git repository."*

**Double-write guard.** The one case where the primary store already *is* git
notes is an explicit `--backend git-notes`. There, and only there, suppress the
write-through so the entry is not written to `refs/notes/spelunk` twice. In every
other case the primary is SQLite (or, pre-`init`, absent), and the write-through
is the sole notes writer.

Entries recorded before `init` carry no embedding vector (git notes hold none),
which is fine because semantic `memory search` stays gated (D4); the vector is
added when the project is later `init`'d and indexed and the carrier hydrates the
index.

Scope of the fallback is deliberately narrow — **only `add` and `list`**. Every
other memory subcommand (`search`, `timeline`, `show`, `graph`, `since`,
`harvest`, `archive`, `supersede`, `push`, `pull`, `sync`, `reconcile`) keeps
its current pre-`init` behaviour (fail-closed, or its existing server/index
requirement). This matches the git-notes backend's own capability surface, which
already returns `BackendUnsupported` for the embedding-dependent methods.

**Known limitations to carry into implementation (not blockers):**

- **The git-notes read-modify-write race** (`GitNotesBackend` doc-comment /
  issue #185): concurrent `add`s to the same HEAD note can lose a write. That is
  acceptable for the solo, pre-`init` quick-fix use case this fallback targets;
  multi-agent workflows should `init` and use SQLite. State this in the docs, do
  not try to fix it here.
- **Empty repo / no HEAD:** if `git rev-parse HEAD` fails (a repo with no
  commits), the fallback cannot attach a note; treat that as "no git repo
  available" and fall to case 5 with the same message.

### D4 — `memory search` stays index/server-gated, with a better message

Do **not** attempt git-notes-backed semantic memory search (it is hard to do
well, and the git-notes backend already returns `BackendUnsupported` for it).
When no embedder/server and no local index are reachable, `memory search`
returns a clear message pointing at the right next step — `spelunk init`,
`spelunk server start`, or `--mode text` — rather than implying a **team**
`server_url` is required (the current message misleads a solo user).

## Per-ticket disposition

| Ticket | Disposition under keep-zero-setup + git-notes fallback |
|---|---|
| **^127 / ^128** — `graph` exact-match only, no signal on zero results | **Survives, rescoped to a zero-result affordance.** When `graph <symbol>` finds nothing, guide the user to `spelunk graph --live` (structural scan) or `spelunk init` (full graph), optionally a did-you-mean. No global-store risk remains (oss^147 merged). Drop any "fuzzy graph before init" goal. |
| **^129** — `search --mode text` hard-errors, demands `index` | **Mooted as a code bug; becomes a doc fix (marketing-site^33).** The hard error is correct for an explicit index-only mode. The zero-setup example must use bare `search "…"` (auto→ast-grep); `--mode text` is shown as a post-`init` example. No spelunk-oss change. |
| **^130** — ast-grep fallback has no substring/fuzzy | **Optional enhancement, not a blocker.** Ship a clear "no matches (live structural scan) — run `spelunk init` for full search" hint now; treat fuzzy/substring as a later nice-to-have. |
| **^131 / ^132** — memory scoping (silent global DB; git-notes not the default backend / no sync consumer) | **Direction changes from fail-closed-refuse to git-notes fallback (D3).** ADR-067 already closed the silent-global-DB leak. This ADR now makes git-notes the **pre-`init` memory path** for `add` / `list` — which reverses ^132's "git-notes is not the default backend" premise for the pre-`init` case. Follow-up: ^132's "no sync consumer" and ^126's "notes don't travel via push/fetch/clone by default" become **material** to the "travels with the repo" promise — see Open questions. |
| **^133** — `memory search` misleadingly suggests team `server_url` | **Messaging fix (D4).** Point at `spelunk init` / `spelunk server start` / `--mode text`, not a team server. Consider defaulting to `--mode text` when no embedder is available. |
| **^126** — git notes don't travel via push/fetch/clone by default | **Elevated by this decision.** Once git-notes is the pre-`init` "memory that travels with the repo," the promise only fully holds if `refs/notes/spelunk` is push/fetch-visible. Decide whether the fallback (or `init`, or a documented one-time git config) should configure the notes refspec — see Open questions. |
| **^140** — manual git-notes inspection docs | **Elevated.** More users will now have notes written via the fallback; the inspection docs (`git notes --ref=spelunk …`, and `spelunk memory list`) are the transparency surface. Keep them current. |
| **marketing-site ^32 / ^33** — getting-started rewrite | **Primary doc deliverable.** Keep the zero-setup framing (D1). Fix the broken examples to use commands that actually work with no `init` (D2): bare `search "…"`, `graph <symbol>` / `--live`, and `memory add` / `list` via the git-notes fallback (D3). Move `--mode text`, indexed graph, and semantic examples into a clearly-marked "after `spelunk init`" section without displacing the zero-setup headline. |
| **New work item — git-notes memory fallback** | **Implement D3** in spelunk-cli: before `init` (no `.spelunk/` project) but inside a git repo, stop `memory add` / `list` failing at the `require_project_db` bail. `add` skips the absent SQLite primary and lets the existing `store_in_git_notes` write-through carry the entry to `refs/notes/spelunk`; `list` reads it back from the notes; explicit `--backend git-notes` suppresses the write-through so an entry is not written twice. Fail only when there is neither a DB nor a git repo. File under spelunk-oss. |

## Non-goals

- **Not** retiring the zero-setup promise (this ADR reverses that earlier
  direction).
- **Not** git-notes-backed semantic `memory search` (D4 keeps it gated).
- **Not** extending the git-notes fallback beyond `add` / `list` — other memory
  subcommands keep their current pre-`init` behaviour.
- **Not** re-introducing a per-directory SQLite memory store outside `.spelunk/`
  (that would reopen the ADR-067 commingling leak; the fallback uses git notes,
  which are scoped to the repo, not a stray global DB).
- **Not** removing or migrating the global `~/.config/spelunk/` store (ADR-067
  left it behind an explicit-only path; unchanged).
- **Not** adding a `--global` flag (ADR-067 D2 reserved it; still deferred).
- **Not** changing the inference-vs-storage split (CLAUDE.md / ADR-004): an
  auto-discovered loopback server remains inference-only and never owns memory.

## Consequences

- **The pitch stays "zero setup."** The opening promise — no API keys, no
  servers, memory that travels with the repo — is kept and made true by pointing
  the docs at the commands that work without `init` and by carrying `memory add`
  / `list` to git notes before `init` through the write-through that already
  ships.
- **`memory add` / `list` gain a new pre-`init` path** (the git-notes
  write-through, now allowed to run with no `.spelunk/` project). This is net-new
  behaviour and a partial reversal of ADR-067's
  fail-closed-for-memory posture: fail-closed now means "fall back to git notes
  if a repo exists, else refuse," not "always refuse without `.spelunk/`."
- **git-notes visibility becomes a promise-load-bearing concern.** ^126 (notes
  don't push/fetch/clone by default) and ^132 (no sync consumer) move from
  cleanup to "does the 'travels with the repo' claim actually hold?" — resolved
  in Open questions / follow-up, not silently assumed.
- **The ticket set shrinks to messaging + docs + one new feature.** ^129 is a doc
  fix; ^131/^132 reframe to the git-notes fallback; ^127/^128/^133 are small
  affordance/messaging fixes; ^130 is optional; ^126/^140 are elevated; plus the
  one new implementation item (D3).
- **Revisit if:** the "travels with the repo" promise cannot be honoured without
  surprising git config changes (see Open questions), in which case the doc claim
  should soften to "stored locally in git notes" rather than "travels with the
  repo."

## Security implications

- No new trust boundary. The git-notes fallback writes to `refs/notes/spelunk`
  in the user's own repo — no network, no global store, no cross-repo
  commingling (notes are scoped to the repo git resolves from CWD). It does not
  reintroduce the ADR-067 per-directory-global-DB leak.
- The existing secret-scan gate in `memory add` (`contains_secret` on title and
  body, run **before** any persistence) applies unchanged on the git-notes path,
  so no credential reaches the note.
- D2's working-tree-only constraint on the index-free surface is preserved by
  oss^147 (no machine-global read from `graph`).

## Open questions

- **Does "travels with the repo" require configuring the notes refspec?**
  `git notes` under `refs/notes/spelunk` are **not** pushed, fetched, or cloned
  by default (^126). Because D3 makes those notes the durable carrier for
  pre-`init` memory (not a second copy of a SQLite store of record), the
  "travels with the repo" claim now rests entirely on that ref being visible
  across clones. For the promise to hold across machines / teammates, either
  the fallback path, `spelunk init`, or a documented one-time
  `git config --add remote.origin.fetch '+refs/notes/spelunk:refs/notes/spelunk'`
  (plus the matching push refspec) must make the notes ref travel. Recommended
  direction: have `init` offer to configure the refspec, and until then keep the
  doc claim accurate ("stored in git notes; run
  `spelunk memory list` to inspect") rather than over-promising cross-machine
  sync. Track under ^126; do not block D3's local `add` / `list` on it.
