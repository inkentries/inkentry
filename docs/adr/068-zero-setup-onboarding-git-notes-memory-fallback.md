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

## Amendment (2026-07-13): canonical content-addressed identity for memory entries

**Date:** 2026-07-13
**Deciders:** founder (Johan); architect

This amendment fixes the identity model that D3's git-notes carrier and the
`init`-time git-notes import both depend on. It is recorded here, on ADR-068,
because both consumers are ADR-068 work and the dedup logic in each is gated on
the decision below. It refines the git-notes v1 surface frozen in
[ADR-059](059-git-notes-v1-format-freeze.md) (additive, backward-compatible) and
the local identity columns added under
[migration 020](../../crates/spelunk-core/migrations/020_memory_uuid.sql).

### A0 – Problem

A memory entry has, today, three identity-shaped values, none of which is a
stable cross-boundary identity:

- **Local i64 `id`** (`notes.id`): an autoincrement rowid. It is machine-local,
  it resets to 1 when a project is re-`init`'d (the DB is recreated), and it is
  numbered independently on every machine. It was never meant to leave the
  process, yet `NoteRecord` serializes it straight into `refs/notes/spelunk`
  (`note_record.rs`, `pub id: i64`), so it leaks across the git-notes carrier.
  Observed live during UAT: two different decisions were both stamped
  `"id":1` in one `refs/notes/spelunk` ref because a re-`init` reset the
  counter between the two writes. `superseded_by: Option<i64>` leaks the same
  unstable rowid, so a supersede edge cannot be resolved after the counter is
  renumbered or on another machine.
- **`remote_id`** (migration 020): the server-minted id. It only exists after an
  entry has been synced to a team server, so it covers the server path alone and
  is absent for the local-only and pre-`init` git-notes cases this ADR targets.
- **`uuid`** (migration 020): a random UUIDv7 minted lazily on first sync and
  pushed as the cloud `external_id` idempotency key. Because it is random rather
  than content-derived, it does not make re-recording idempotent across a
  re-`init` (the DB, and the `uuid`, are gone) or across two machines that
  independently record the same decision. It is regenerated per creation, so it
  cannot collapse duplicates that the content itself proves are the same.

The dedup logic in the git-notes carrier (D3) and the import-on-init hydration
needs one identity that is stable across the local store, the git-notes carrier,
and the server, and that is computable with no server, no sync, and even with
git notes disabled. The i64 rowid, `remote_id`, and the random `uuid` each fail
at least one of those requirements.

### A1 – Decision: a content-addressed id is the canonical identity

**The canonical identity of a memory entry is a content hash computed from its
semantic core. This id is the same value on the local store, in
`refs/notes/spelunk`, and on the server, and it is derivable by any reader from
the entry itself with no coordination.** Two parties that independently record
the same decision compute the same id; re-recording an unchanged entry is a
no-op keyed on that id.

Two id roles are defined. In today's data model they hold the same value for
every entry (see A3), but they are named separately because they answer
different questions and diverge the moment an in-place content edit is ever
added:

- **`content_hash`** – the idempotent write and dedup key. It is the hash of the
  entry's *current* canonical content. It is the key the git-notes carrier and
  the import both dedup on: a record whose `content_hash` is already present is
  not written or imported again.
- **`entity_id`** – the stable identity of the logical memory across its whole
  history (status changes, supersede chains, and any future in-place edit). It
  is the hash of the entry's *genesis* canonical content, minted once at
  creation and then immutable and carried on every serialized copy. Supersede
  and other edges reference `entity_id`, so they never dangle when a local rowid
  is renumbered or differs across machines.

`content_hash` and `entity_id` use the identical hash function over the identical
canonical form (A2). They differ only in *when* the input is captured: `entity_id`
is pinned to the genesis content and stored; `content_hash` is (re)derived from
the current content. One algorithm, two roles.

### A2 – Canonical form and hash (git-independent, cross-language)

The id is **`sha256`** over the **canonical JSON** of the entry's semantic core,
rendered as a 64-character lowercase hex string.

**Canonical field set (frozen for `schema_version` 1):** exactly three fields,
all strings:

- `body`
- `kind`
- `title`

Everything else is **excluded**: `id`, `remote_id`, `uuid`, `schema_version`,
`created_at`, `valid_at`, `invalid_at`, `status`, `superseded_by`, `source_ref`,
`tags`, and `linked_files`. The exclusions are deliberate:

- `id`, `created_at`, timestamps, `schema_version`, `remote_id`, `uuid` are
  machine-local, volatile, or format bookkeeping. Folding any of them in would
  reintroduce exactly the cross-machine, re-`init`-unstable behaviour this
  amendment removes.
- `status`, `superseded_by`, `valid_at`, `invalid_at` are **mutable state** that
  changes over an entry's life (archive, supersede, temporal validity). Keeping
  them out means the id is a **stable locator**: archiving or superseding an
  entry does not change its id, so those mutations find their target by a
  content-addressed key rather than by the unstable rowid.
- `tags` and `linked_files` are **mutable, machine-variable associative
  metadata**. Two people classifying or linking the same decision differently
  must still land on the same identity, otherwise re-tagging would fragment the
  identity and break the very idempotency this fixes. (This is the direct answer
  to the "a content hash changes on any re-tag, so it is only a version id"
  concern: the fix is not to admit `tags`/`status` into the hash and then paper
  over the churn with an entity id; it is to keep mutable metadata out of the
  hash entirely. The `entity_id` role in A1 exists for the one remaining genuine
  content mutation, an in-place `title`/`body` edit, which the model does not
  support today.)

**JSON canonicalization rules** (so the bytes are identical across the Rust
client and the server, and reproducible by any third-party reader):

1. Object with exactly the three canonical keys, **sorted ascending by Unicode
   code point**: `body`, `kind`, `title`.
2. **Compact:** no insignificant whitespace. `,` between members and `:` between
   key and value, with no surrounding spaces.
3. **UTF-8, no BOM.** String values are emitted as raw UTF-8; non-ASCII
   characters are **not** `\u`-escaped. Only the characters JSON requires are
   escaped: `"`, `\`, and the C0 control characters U+0000 through U+001F.
   Forward slash is not escaped.
4. **No Unicode normalization, no case folding, no whitespace trimming** is
   applied inside the hash: the exact stored bytes of each field are hashed. Any
   input tidying (for example trimming trailing whitespace so trivially
   different inputs collide) is an `add`-time concern applied *before* the record
   is stored, not part of the hash.
5. All three fields are strings, so there is no number, float, or boolean
   canonicalization to specify. Keeping the canonical form string-only is
   intentional and removes that whole class of cross-language divergence.

In Rust this is exactly `serde_json::to_vec` of a `BTreeMap<&str, &str>`
containing the three fields (the `BTreeMap` supplies the sorted keys; serde's
default string encoding supplies rules 2 and 3), then `sha256` of those bytes,
hex-encoded lowercase. Reference form:

```
canonical_bytes = serde_json::to_vec(&BTreeMap::from([
    ("body",  body),
    ("kind",  kind),
    ("title", title),
]))
content_id = hex_lower(sha256(canonical_bytes))
```

Worked example: an entry with `kind = "decision"`, `title = "HTTP layer"`,
`body = "use axum"` has canonical bytes
`{"body":"use axum","kind":"decision","title":"HTTP layer"}` and the id is the
lowercase hex `sha256` of those bytes.

**Explicitly not the git blob sha.** The id is not coupled to git's object hash.
Git's blob sha frames the content with a `blob <len>\0` header, it is computed
over the whole multi-record note blob rather than one entry, and it is SHA-1
that flips to SHA-256 on opt-in repositories. The canonical id here is a plain
`sha256` over one entry's canonical JSON and is identical whether or not the
entry ever touches git.

### A3 – Entity vs version, reconciled with the actual data model

The concern that a content hash identifies a *version* rather than an *entity*
assumes the hashed content can change under a stable entity. In this codebase it
cannot, for the common case:

- `kind`, `title`, and `body` are **immutable after creation**. There is no
  `memory edit` subcommand; `open_editor_for_body` composes a *new* entry's
  body at create time. The only in-place `UPDATE notes` statements touch
  `status`, `superseded_by`, `uuid`, and `remote_id` (mutable state, all
  excluded from the hash per A2).
- A correction is modeled as **supersede**: a new entry (its own
  `kind`/`title`/`body`, hence its own id) that archives the old one and links
  back to it.

Consequences:

- For every entry that exists today, `content_hash == entity_id`, because the
  content never changes after genesis. The two roles are named distinctly only
  so the format does not have to change if an in-place edit feature is ever
  added: an edited record would keep its stored `entity_id` (genesis) and take a
  new `content_hash` (current), and both fields already exist.
- **Supersede does not dangle.** The edge is expressed as the superseding
  entry's `entity_id`, a content-addressed value, so it resolves correctly after
  a re-`init` renumbers rowids and across machines. This replaces the
  `superseded_by: Option<i64>` leak.
- The downstream dedup key is therefore unambiguous: the git-notes carrier and
  the import both key on `content_hash` (equal to `entity_id` for every entry
  today), and both express supersede via `entity_id`.

### A4 – Reconciliation: one canonical identity, not three

To avoid the entry carrying three competing identities, the roles collapse as
follows:

- The **content-addressed id is the single canonical global identity** of a
  memory entry, on every surface (local store, `refs/notes/spelunk`, server).
- The **local i64 `id` is demoted to an in-process rowid only.** It stays the
  SQLite primary key for local joins and is convenient in CLI output, but it is
  **no longer serialized as identity**. Distributed surfaces carry the content
  id; a reader that needs a stable handle uses the content id, never the rowid.
- **`remote_id` becomes a server addressing handle mapped from the canonical
  id, not a competing identity.** The server may keep its own rowid or UUID for
  REST paths and internal joins, but correlation of "the same entry" across
  machines is by the content id. `remote_id` maps one-to-one to it and is not
  used to decide identity.
- **The random `uuid` (external_id) idempotency key is subsumed by the content
  id.** The content id is a strictly better idempotency key than a random
  UUIDv7: it makes re-recording idempotent across a re-`init` and across
  machines, which the random value could not. New work on the sync path should
  push the content id as the `external_id` in place of a freshly minted random
  `uuid`. This reverses, for the client's cross-boundary identity, the earlier
  "fresh UUIDv7, not content-derived" choice noted in migration 020; the founder
  directed the content-addressed model on 2026-07-13. The server's own internal
  id generation is unaffected; only the client's cross-boundary identity and
  idempotency key change. Migrating the existing `uuid` column and the push path
  is downstream implementation and is not gated by this amendment; the decision
  here is only that the content id is the canonical identity so no fourth
  identity is minted.

End state: **content id = canonical identity (everywhere); local i64 = in-process
rowid; `remote_id` = server addressing handle mapped from the content id.**

### A5 – What changes on each surface (additive, backward-compatible)

All changes are additive under ADR-059's rules (optional fields, absent reads as
`None`, no existing field changes type or nullability), so `schema_version` stays
`1`. The canonical-form definition in A2 is what `schema_version` 1 pins; any
change to the canonical field set is a `schema_version` bump and a new ADR.

- **`NoteRecord`** (`note_record.rs`, the git-notes and local JSON shape): add an
  additive `entity_id: Option<String>` (the content id) and, for supersede
  portability, an additive string form of the supersede reference carrying the
  target's `entity_id`. The existing `id: i64` and `superseded_by: Option<i64>`
  remain for backward compatibility but are no longer the identity of record.
  Because the id is a pure function of `{kind, title, body}`, a reader
  encountering a **legacy blob without `entity_id` recomputes it** from the
  three fields it already has. Absence is fully recoverable; storing the field
  is an optimization (O(1) dedup) and the carrier for `entity_id` once an edit
  feature could make it non-recomputable.
- **Local store:** persist the content id alongside each entry so dedup and edge
  resolution are index lookups rather than recomputations. Whether this reuses
  the migration 020 `uuid` column or adds a dedicated column is left to the
  implementing work; the logical requirement is that the canonical id is stored
  and uniquely indexed.
- **`/v1` wire types and server rows:** carry the content id as the additive
  canonical identity, mapped to the server's `remote_id` handle per A4. Additive
  and optional, consistent with ADR-059 D2's treatment of `remote_id`.

### A6 – Gating rule for the downstream work

Both ADR-068 consumers implement dedup against this model:

- **git-notes carrier (D3):** each entry is identified in `refs/notes/spelunk` by
  its content id. Appending an entry whose content id is already present on the
  target note is a no-op; two different entries have different content ids, so
  the observed `"id":1` collision cannot recur. Supersede and archive locate
  their target by content id, not by the i64 rowid.
- **import-on-init hydration:** when hydrating `memory.db` from
  `refs/notes/spelunk`, dedup by content id (recomputed from `{kind, title,
  body}` for any legacy line that lacks the stored field). An entry whose content
  id already exists locally is not re-inserted; mutable state (`status`,
  supersede links via `entity_id`) from the note is reconciled onto the matched
  row. Local rowids are assigned fresh on import and are never used to correlate.

### A7 – Non-goals, consequences, security

- **Non-goal:** building the git-notes-as-sync consumer (still out of scope per
  ADR-059) or changing the server's internal id generation. This amendment
  defines identity; it does not add a reconciler.
- **Non-goal:** admitting `tags`, `status`, or `linked_files` into identity. They
  stay mutable metadata on the record.
- **Consequence:** identity is now derivable and stable. Re-`init`, offline use,
  and independent recording of the same decision on two machines all converge on
  one id with no server and no coordination. The i64 rowid can be renumbered
  freely without affecting identity or edges.
- **Security:** the content id carries no authority; like `remote_id` it is an
  opaque identity string, and read/write authorization on a shared server is
  unchanged and remains governed by [ADR-056](056-oss-server-tenancy-model.md)
  (single trust domain, shared key). Hashing `title` and `body` exposes nothing
  new: the id always travels next to the very content it is derived from (the
  full body is in the same note line or row), so it reveals nothing a reader of
  the entry does not already hold. `sha256` collision resistance makes an
  accidental id clash between two genuinely different entries negligible. The
  existing pre-persistence secret scan is unaffected; identity is computed from
  the same fields that scan already gates.
