# ADR-077: Teammate memory reaches read paths — import the notes carrier into `memory.db` on read, gated by a notes-ref OID marker

**Status:** Proposed
**Date:** 2026-08-04
**Deciders:** founder (Johan); architect
**Relationship to prior ADRs:** completes
[ADR-069](069-git-notes-sharing-pre-push-hook-and-tracking-refspec.md). ADR-069
D5 decided that spelunk merges the fetched tracking ref
(`refs/notes/origin/spelunk`) into the working ref (`refs/notes/spelunk`) on its
own read paths, so "reading a teammate's memory needs no opt-in." That merge is
implemented and tested, but it stops at the git ref. The default read paths query
the SQLite `memory.db`, which nothing imports the carrier into except `init`, so
the merged entries never actually surface. This ADR carries the notes across the
last gap — ref to `memory.db` — on every read, cheaply. Leaves ADR-068's carrier
model and ADR-004's inference-vs-storage split untouched: the carrier is still
`refs/notes/spelunk` and the queryable store is still `memory.db`; this ADR only
keeps the second in step with the first.

## Context

Two reproducible failures leave a teammate's published memory invisible.

### Failure 1 — a fresh clone imports zero memory

`git clone` does not fetch `refs/notes/*`. `spelunk init`
(`crates/spelunk-cli/src/cli/cmd/init.rs`) runs its git-notes import pass
(step 6b, `import_git_notes_into_memory`) **before** it configures the `origin`
notes fetch refspec (step 7, `configure_notes_refspec`). On a fresh clone the
tracking ref has never been fetched — the refspec that would fetch it is set only
in step 7, which runs after the import — so step 6b's `merge_tracking_notes` +
`import_git_notes_into_memory` run against an absent/empty notes ref and import
nothing. The only sequence that currently works is
`clone → init → git fetch → init again`: the first `init` sets the refspec, the
manual `git fetch` populates the tracking ref, and the second `init` finally
imports. One `init` after clone is not enough.

### Failure 2 — ongoing teammate memory never reaches the read paths

Once the refspec is configured, `git fetch` brings a teammate's notes onto
`refs/notes/origin/spelunk` (verified present on the tracking ref). ADR-069 D5's
`merge_tracking_notes` (`crates/spelunk-core/src/storage/git_notes/mod.rs`) then
folds that tracking ref into the working ref `refs/notes/spelunk` on the
`memory list` and `context` read paths. **But the default read paths do not read
the git ref.** `open_memory_backend` (`crates/spelunk-core/src/storage/mod.rs`)
returns a `LocalMemoryBackend` over `.spelunk/memory.db` for every read unless the
caller passes `--backend git-notes` or is on a cloud-routing config. So the merge
updates a git ref that the SQLite-backed `memory list` / `memory search` /
`memory show` / `context` never consult. The one function that bridges ref →
`memory.db` — `import_git_notes_into_memory`
(`crates/spelunk-cli/src/cli/cmd/memory/reconcile.rs`) — is called **only from
`init`**. No read command triggers it. A teammate's freshly published entry
therefore stays invisible on the default read path until the user manually
re-runs `init`.

Two smaller facts compound Failure 2:

- **Two read paths do not even merge.** `merge_tracking_notes` is wired into
  `memory list` (`memory/list.rs`) and `context` (`context.rs`) but **not** into
  `memory search` (`memory/search.rs`) or `memory show` (`memory/show.rs`). Those
  two are blind to the tracking ref entirely.
- **The existing round-trip test reads the wrong path.** `notes_round_trip_through_bare_origin`
  (`crates/spelunk-cli/tests/init_notes_refspec.rs`) does exercise a real
  two-repo clone/fetch, but it asserts visibility via
  `spelunk memory --backend git-notes list` — it reads the git ref directly, not
  the default `memory.db`. So the merge is proven and the *default read path is
  not*. This is the precise shape of a prior false-close in this area: a mechanism
  applied and asserted, while the path a real user hits is never exercised
  end to end.

### The config-commit overlap

The project slug is written to `.spelunk/config.toml` by `init`
(`write_project_slug`, `crates/spelunk-core/src/config/persist.rs`), and the docs
state plainly that this file is committed and travels with the repo
(`docs/getting-started.md`: ".spelunk/config.toml stays tracked";
`docs/commands.md`: "writes the project slug to `.spelunk/config.toml`
(committed…)"). But `init` neither stages nor commits it. When the slug cannot be
re-derived on a fresh clone — a repo with no remote derives `local/<blake3-hex>`
from the local path (`derive_project_id`, `crates/spelunk-core/src/config/project_id.rs`),
which differs per clone, and an explicit `--name` slug cannot be re-derived at
all — the promise "the slug travels with the repo" is not kept, and the user
needs a second `init`. This ADR decides the `init`-stages-config half of that
overlap. The `.spelunk/.gitignore` half (ensuring `config.toml` is not ignored
while the SQLite files are) is owned by a separate change and is not designed
here.

## Decision

**Read paths import the notes carrier into `memory.db`, but only when the notes
ref has moved since the last import, checked by comparing the ref OID against a
marker persisted in `memory.db`. `init` configures the refspec and fetches notes
*before* its import pass so one `init` suffices after clone. No new git hook.
`init` stages `.spelunk/config.toml` so the committed-carrier promise is
actionable.**

### D1 — read paths import from the carrier, gated on a notes-ref OID marker

Every SQLite-backed memory read path — `memory list`, `memory search`,
`memory show`, `context` — must, before it queries `memory.db`:

1. Run `merge_tracking_notes` (fold `refs/notes/origin/spelunk` into
   `refs/notes/spelunk`). This already runs on `list`/`context`; it must be added
   to `search` and `show`.
2. Import `refs/notes/spelunk` into `memory.db` via the existing
   `import_git_notes_into_memory`, **only if** the working ref moved since the
   last import.

Both steps are gated by a cheap check so the steady state costs almost nothing
(see D2). The import reuses `import_git_notes_into_memory` unchanged: it already
dedups by content-addressed `entity_id`, imports without embeddings, and is a
no-op on an absent ref, so running it from a read path is idempotent and safe.

This is deliberately **not** an unconditional import on every read. Option (a),
import-every-read, was rejected on cost: `import_git_notes_into_memory` lists the
git-notes backend, which reads *every reachable note blob* (a `git notes show`
subprocess per noted commit — the backend's own docs note "the entity fold has to
read every reachable note blob whatever the limit") and then full-scans
`memory.db` for the dedup set. On a CLI shelled out thousands of times per
session that is not acceptable per invocation. Option (c), an explicit
`spelunk sync`-style command, was rejected because it reintroduces exactly the
failure this ADR removes: a teammate reviewing code will not run a manual step,
so their colleagues' decisions stay invisible until someone remembers to sync.
The gated read-path import (option b) is the only one that makes reading
automatic *and* cheap.

### D2 — the marker is the notes-ref OID, persisted in `memory.db`

The gate is a comparison of git ref OIDs, read **in process** (not via a
subprocess). ADR-069 D5 already measured this exact trade for the merge: a
`git rev-parse` subprocess guard costs ~8ms, while an in-process read of the ref
file costs ~17µs, and concluded the in-process read "is worth having." This ADR
takes that same in-process read and uses it to gate both the merge and the
import.

`memory.db` persists two OIDs (a small `notes_import_state` table added as memory
schema migration step 10 — the runner in
`crates/spelunk-core/src/storage/memory/mod.rs` is currently at
`MEMORY_SCHEMA_VERSION = 9`):

- `last_merged_tracking_oid` — OID of `refs/notes/origin/spelunk` at the last
  merge.
- `last_imported_working_oid` — OID of `refs/notes/spelunk` at the last import.

On a read:

- Read current tracking OID `T` and working OID `W` in process.
- If `T` differs from `last_merged_tracking_oid` → run `merge_tracking_notes`,
  then re-read `W`; record `T`.
- If `W` differs from `last_imported_working_oid` → run
  `import_git_notes_into_memory`, then record the post-import `W`.
- If neither differs → skip both. This is the steady state.

The marker lives in `memory.db`, not a sidecar file, so the OID is written in the
**same transaction** as the import: a crash between "imported" and "recorded OID"
cannot leave the two disagreeing, and the marker is discarded with the store when
`memory.db` is deleted/rebuilt. A `.spelunk/`-local marker file was rejected for
lacking that atomicity and for being a second thing to keep in sync with the
store. (This mirrors the reasoning by which ADR's cloud-sync cursor avoided a
sidecar; there the cursor is *derived* from `MAX(remote_id)`, but no equivalent
value is derivable here — imported rows are stamped `source_ref = "init:git-notes"`,
not the ref OID — so a small persisted marker is the minimum needed.)

**Net effect on the hot path is a reduction, not an addition.** Today `memory list`
and `context` spawn a `merge_tracking_notes` git subprocess and take the notes
lock on *every* invocation. Under D2 the steady-state read spawns **zero** git
subprocesses: two in-process ref reads plus one indexed single-row `SELECT`.

### D3 — `init` configures the refspec and fetches notes before its import pass

Reorder `init` so a single run after clone hydrates teammates' memory:

1. Configure the `origin` notes fetch refspec (today's step 7) **first**.
2. Perform a one-time, best-effort `git fetch` of the notes ref to populate
   `refs/notes/origin/spelunk`. Non-fatal and skipped when there is no `origin`
   or the network is unreachable — `init` must still succeed offline.
3. Then run the merge + import pass (today's step 6b).

`init` is a setup command that already spawns a server and indexes the tree, so a
single bounded fetch at init is in keeping with its altitude; it must not sink
`init` on failure. The read-path import from D1 is the durable guarantee — even
without the init fetch, the first read after any later `git fetch` surfaces the
memory — but fetching at init is what makes one `init` self-sufficient instead of
requiring `clone → init → fetch → init again`.

### D4 — no new git hook

There is no role for a `post-merge` / `post-checkout` importer. ADR-069 D5
already established that git has **no post-fetch hook**; `post-merge` fires on
`git pull`'s merge but not on a bare `git fetch`, so it misses the fetch-only
reviewer, which is the common read case; and any hook must be installed to take
effect. The D1 read-path import covers every consumer — including the fetch-only
reviewer — with nothing to install, and it is idempotent and cheap via the D2
gate, so a hook would be both redundant with the read path and incomplete
relative to it. Publishing memory stays on the existing opt-in **pre-push** hook
(ADR-069 D1/D7), unchanged.

### D5 — `init` stages `.spelunk/config.toml`, and does not commit it

`init` `git add`s `.spelunk/config.toml` after writing the slug, best-effort and
non-fatal like the rest of `init`. It does **not** commit.

Staging makes the file appear in `git status`, teed up for the developer's next
commit, which is what turns the docs' "committed, travels with the repo" promise
into something that actually happens for a remote-less repo or an explicit
`--name` slug (the cases where the slug cannot be re-derived on a fresh clone).
Auto-committing was rejected: `init` is a setup command that indexes, spawns a
server, and writes several files, and slipping a commit into the user's
in-progress index and history at that moment is surprising and can entangle with
work they are composing. Staging is trivially reversible (`git restore --staged`)
and non-destructive; a commit is neither. This decision assumes the sibling
`.spelunk/.gitignore` change keeps `config.toml` tracked while ignoring
`index.db*` / `memory.db*`; the two must agree on that split.

## Non-goals

- **Not** changing the carrier or the merge strategy. `refs/notes/spelunk` stays
  the carrier and `cat_sort_uniq` (ADR-069 D2) stays the merge; this ADR only
  propagates the merged result into `memory.db`.
- **Not** re-deriving `NoteRecord.id` identity. Dedup on import already keys on
  the content-addressed `entity_id`, so colliding local rowids across developers
  are collapsed correctly by the existing `import_git_notes_into_memory`.
- **Not** importing embeddings from the carrier. Git-notes entries carry none;
  imported rows are re-embedded by the normal path, exactly as init's import does
  today.
- **Not** making reads perform network I/O. Like ADR-069 D5's merge, the
  read-path import merges and imports only what the user's own `git fetch` already
  wrote. The single fetch in D3 is confined to `init`, a user-initiated setup
  action.
- **Not** designing the `.spelunk/.gitignore` change (sibling task); D5 decides
  only the `init`-stages-`config.toml` question.

## Consequences

- **Reading a teammate's memory becomes automatic on the default path.** After a
  `git fetch`, the next `memory list` / `search` / `show` / `context` surfaces
  the fetched entries with no `--backend git-notes` and no re-`init`. The ADR-069
  D5 promise ("reading needs no opt-in") now holds for the store users actually
  query, not only for the git ref.
- **One `init` after clone is enough.** D3 removes the `clone → init → fetch →
  init again` dance.
- **Read commands acquire a local write on the ref-changed path only.** ADR-069
  D5 already made `list`/`context` mutate a local ref; D1 extends that to
  `search`/`show` and adds a gated `memory.db` write. On the steady-state
  (nothing fetched) path there is no ref mutation and no import at all.
- **Hot-path cost drops for `list`/`context`.** They stop spawning an
  unconditional merge subprocess; the OID gate short-circuits in process.
- **A memory schema migration lands** (step 10, `notes_import_state`). It is
  additive and idempotent, consistent with the existing forward-only runner.
- **The false-close guard is a hard requirement on the tests.** The acceptance
  round trip must read the **default** `memory.db` path end to end (two clones,
  fetch, no manual re-init), not `--backend git-notes`. See Implementation
  contract.
- **Revisit if:** a normal fetch starts landing thousands of divergent annotated
  objects (the axis ADR-069 flagged as the one that degrades to seconds), in
  which case the import cost per fetch — not per read — becomes the thing to
  bound.

## Security implications

- No new trust boundary and no new store of record. The carrier
  (`refs/notes/spelunk`) and the queryable store (`memory.db`) are both already
  in the user's own repo; D1 only copies between them.
- **The read path performs no network I/O.** The import merges and imports the
  local tracking ref that the user's own `git fetch` populated. This preserves
  ADR-069 D5's property that reads work with the remote unreachable and never put
  egress on a path the user did not point at a remote. The only fetch is the
  bounded, best-effort one in `init` (D3), a user-initiated setup action.
- The `memory add` secret scan (`contains_secret`, run before any persistence) is
  unchanged and still runs on the write path into both `memory.db` and the note,
  so imported content was already scanned at its origin.
- The D2 marker holds only a git object id, no secret.

## Implementation contract

> The architect does not write the Rust or the tests. This section is the
> contract the implementer drives off, and it is reproduced in the handoff report.

### Acceptance criteria

- **Two-clone round trip on the DEFAULT read path, end to end.** Clone A adds a
  memory entry (`memory add`, `store_in_git_notes = true`) and publishes the notes
  ref to a bare origin. Clone B (a fresh `git clone`) runs `init` once, then a
  plain `git fetch`, then `spelunk memory list` **with no `--backend git-notes`
  and no second `init`** — and the entry appears. The same holds for
  `memory search`, `memory show <id>`, and `context`. This assertion, on the
  SQLite `memory.db` path, is the criterion that a mere refspec/merge existence
  check does not satisfy.
- **Fresh-clone single-init.** In Clone B, one `init` (with the D3 fetch)
  followed by a read surfaces A's entry — the `clone → init → fetch → init again`
  sequence is no longer required.
- **Hot-path is subprocess-free in steady state.** With nothing fetched since the
  last import, a read spawns no `git notes merge` subprocess and runs no
  `import_git_notes_into_memory` note-blob walk (assert via the D2 marker being
  unchanged / a merge-invocation seam).
- **Import fires exactly on ref movement.** After a fetch that advances
  `refs/notes/spelunk`, the next read imports; an immediately following read with
  no new fetch does not import again.
- **Offline `init` still succeeds.** With no reachable `origin`, `init` completes
  (D3 fetch is best-effort) and configures the refspec.
- **`init` stages `config.toml`.** After `init` in a repo, `.spelunk/config.toml`
  is staged (present in `git diff --cached --name-only`) and no commit was created
  by `init`.
- **No regression to local-only or cloud-routing reads.** With no git repo, or on
  a `cloud_first` config, reads behave exactly as today (no import attempt).

### Canon-TDD test list

1. `read_path_surfaces_fetched_teammate_note_via_default_sqlite_backend` — two
   clones + bare origin; after fetch, `memory list` (no backend override) in
   Clone B contains A's entry.
2. `search_surfaces_fetched_teammate_note_on_default_path` — same setup;
   `memory search` (text mode) returns A's entry.
3. `show_surfaces_fetched_teammate_note_on_default_path` — same setup;
   `memory show <id>` resolves an imported entry.
4. `context_surfaces_fetched_teammate_note_on_default_path` — same setup;
   `context` includes A's decision/requirement.
5. `single_init_after_clone_imports_without_manual_refetch_reinit` — Clone B runs
   one `init`; a subsequent read surfaces A's entry (D3).
6. `read_skips_import_when_notes_ref_unchanged` — after a first importing read, a
   second read with no intervening fetch does not re-run the import (marker
   unchanged; import seam not called).
7. `read_imports_after_notes_ref_advances` — a fetch advances the working ref;
   the next read imports and the marker updates.
8. `read_path_import_does_no_network` — with origin unreachable, a read still
   merges+imports the local tracking ref and surfaces previously-fetched entries.
9. `import_marker_persisted_in_memory_db_and_survives_reopen` — the OID marker
   round-trips through a `MemoryStore` reopen (migration step 10).
10. `init_offline_succeeds_and_configures_refspec` — no origin reachable; `init`
    exits 0 and the fetch refspec is present.
11. `init_stages_config_toml_without_committing` — after `init`,
    `.spelunk/config.toml` is staged and `git log` shows no init-authored commit.
12. `import_dedups_colliding_ids_across_two_authors` — A and B each author a
    distinct entry that collides on local rowid; after B fetches A's note, B's
    default read shows both, deduped by `entity_id` (no loss, no double count).
13. `no_git_repo_read_makes_no_import_attempt` — a read outside any git repo
    behaves as today (regression guard).
