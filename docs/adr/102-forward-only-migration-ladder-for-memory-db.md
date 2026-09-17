# ADR-102: A forward-only migration ladder for `memory.db`, starting at version 11

**Date:** 2026-09-17
**Deciders:** Founder (Johan); Architect
**Relationship to prior ADRs:** refines the "created at final shape" reset that
accompanied [ADR-078](078-uuidv7-memory-entry-identity.md) and the import path.
That reset meant: a store from before version 11 is not opened in place, it is
exported and imported. It was never meant to say that version 11 is the only
version there will ever be. This record restores that intent in the code. It
is a prerequisite for the schema changes in
[ADR-098](098-metrics-and-evaluation-indexed-by-commit.md) D6,
[ADR-099](099-anchor-memory-entries-to-the-commit-that-carries-the-work.md) D1
and [ADR-101](101-normalised-tags-and-linked-files-in-the-memory-projection.md)
D1, and should land before any of them. Journal mode is unchanged
([ADR-084](084-memory-db-journal-mode-wal.md)).

## Context

`MemoryStore::create_schema` (`crates/inkentry-core/src/storage/memory/mod.rs`)
has three outcomes for a non-empty file:

| `user_version` | today |
|---|---|
| equal to `MEMORY_SCHEMA_VERSION` (11) | open |
| greater | refuse: "newer than this build supports; upgrade inkentry" |
| anything else, including a stamped store below the build's version | refuse: "written by an older product; export with `spelunk-export`, then `inkentry import`" |

The third row is correct for stamps at or below `LAST_LEGACY_SCHEMA_VERSION`
(10): 0.9.6 stamped 9, 0.9.7 and 0.9.8 stamped 10, and those stores belong to
the previous product. It is wrong for everything between 11 and the build's
version, and the two cases are indistinguishable in the code because the branch
tests only `version > 0 || !is_empty_file()`.

Nothing is broken today, because no build has ever stamped anything other
than 11. The first build that sets `MEMORY_SCHEMA_VERSION = 12` will tell every
1.0 and 1.1 user that their store was written by an older product and point
them at an export tool for a product they never used. There is no step from 11
to 12 because there has never been a 12.

Before the rename from spelunk to inkentry the CLI carried a ladder of numbered
steps and applied them on open. It was removed deliberately as part of the
rename: the product was pre-1.0, so there was no need to carry a full migration
history forward, and because the command name and the on-disk locations were
changing anyway it was the natural moment to force one external migration
(`spelunk-export`, then `inkentry import`). No inkentry user is expected to
hold a store below 11. The decision retired the pre-11 *history*. It was never
a decision that stores would not migrate again, and the mechanism itself was
sound.

For contrast, `index.db` is rebuilt rather than migrated and needs no ladder.
`server.db` has numbered files (`server_001.sql` to `server_008.sql`) that are
re-run on every open and rely on idempotent SQL or on swallowing
"duplicate column name" (`crates/inkentry-server/src/db.rs::migrate`). It
works, but it has no version stamp; it is noted here and left alone.

## Decision

### D1 - three ranges, decided by the two existing constants

| `user_version` | behaviour |
|---|---|
| 0 and the file has no tables | create at final shape, stamp `MEMORY_SCHEMA_VERSION` (unchanged) |
| 1 to `LAST_LEGACY_SCHEMA_VERSION` (10), or 0 with tables | refuse with the existing export-and-import message (unchanged) |
| 11 to `MEMORY_SCHEMA_VERSION - 1` | **apply the ladder** (new) |
| `MEMORY_SCHEMA_VERSION` | open (unchanged) |
| greater | refuse with the existing upgrade message (unchanged) |

`LAST_LEGACY_SCHEMA_VERSION` stays at 10 forever. The compile-time assertion
that the build's version is above it stays.

### D2 - steps are numbered SQL files, applied in order, one transaction each

`migrations/memory_012.sql`, `memory_013.sql`, and so on: one file per version,
named for the version it produces. For each missing step, in order:
`BEGIN; <step>; PRAGMA user_version = N; COMMIT;`. `user_version` is
transactional, so a crash or an error leaves the store at the last completed
version and the next open resumes from there.

A step that needs more than SQL (re-deriving a column through Rust code, as
ADR-101's tag normalisation will) is a Rust function registered for that
version and run inside the same transaction. The registry is a static table
of `(version, step)`; a test asserts it is contiguous from 12 to
`MEMORY_SCHEMA_VERSION`.

### D3 - the final shape stays declared in one file, and a test holds the two together

`memory_001_initial.sql` continues to declare the complete current shape, and a
new store is created from it directly rather than by replaying steps. Every
step therefore changes two things: its own file and `memory_001_initial.sql`.
A test builds one store fresh and one from a version-11 fixture through the
ladder and asserts that `sqlite_master` is identical, including indexes,
triggers and virtual tables. The version-11 fixture is a committed file
produced by the 1.1.0 release binary, and it is never regenerated.

### D4 - forward only

There are no down-steps. A user who downgrades gets the existing "newer than
this build" refusal, which is already correct advice. Recovery from a bad step
is `inkentry dump` from the newer build and `inkentry import` into the older
one, or a rebuild of the projection from `refs/notes/inkentry` followed by
`memory reindex`.

### D5 - concurrency and visibility

The ladder runs under an exclusive transaction, so two processes opening the
same store at once (several agents, shared by linked worktrees) serialise; the
loser finds the version already current and continues. A migration writes one
line to stderr naming the versions crossed and nothing to stdout, so
agent-parsed output is unchanged. Every command that opens the store triggers
it; there is no separate `migrate` command to remember.

### D6 - scope

This ADR lands with `MEMORY_SCHEMA_VERSION` still at 11 and an empty step
registry: the change is the open path, the registry, the fixture and the
tests. The first real step arrives with whichever of ADR-098, ADR-099 or
ADR-101 is implemented first.

## Rationale

| Option | Considered | Rejected because |
|---|---|---|
| Keep "no ladder"; rebuild `memory.db` from the git-notes carrier on a version change | Keeps a single shape in the code | Embeddings are not in the carrier, so every upgrade re-embeds the whole store; stores that predate `init`, and server-backed stores, have no carrier to rebuild from; local-only state (ADR-099's pending anchors) would be lost |
| Require `dump` and `import` for every schema change | Already exists and is tested | A manual step on every upgrade, for every user, on the only copy of their local state |
| Re-run idempotent SQL on every open, as `server.db` does | No version bookkeeping | Depends on every statement being idempotent or on matching error strings; cannot express a data backfill safely; pays the cost on every open |
| Adopt a migration crate (`refinery`, `rusqlite_migration`) | Less code to own | Each brings its own version bookkeeping, and `user_version` is already in use with a numbering that must not be reclaimed; the ladder is a few dozen lines |
| Replay the steps to create new stores too | One source of shape | New-store creation would get slower with every release, and the final shape would stop being readable in one place |
| Add down-steps | Symmetry | Doubles the surface to test for a case the existing refusal already handles honestly |

## Consequences

**Easier**

- Schema changes after 1.0 stop being a lockout. ADR-098, ADR-099 and ADR-101
  become ordinary steps rather than each needing to solve this.
- The refusal messages become accurate for every range.

**Harder**

- A step runs once, unattended, on a user's only copy of local state. Each one
  needs a fixture and a test, and the parity test (D3) fails the build when
  someone edits one file and not the other.
- The open path gains a failure mode: a step can fail on data nobody
  anticipated. The store stays at the prior version and the binary that failed
  cannot open it; the error must say which step failed and that the previous
  release still can.

**Revisit if**

- A step needs minutes on a large store: move long backfills out of the open
  path into a resumable background pass, keeping only the shape change in the
  step.
- `server.db`'s re-run-everything approach causes an incident: give it the
  same stamped ladder.

## Security implications

- Steps are compiled into the binary. No SQL is read from disk or from the
  network at run time.
- A step runs with the privileges of whatever opened the store, including git
  hooks. It must not invoke external processes or touch files other than the
  store.
- A failed step must not leave a partially migrated store; the
  transaction-per-step rule is what guarantees that, and the tests include a
  step that fails midway.
- No automatic backup is taken. The release note for the first build carrying
  a real step tells users to run `inkentry dump` first.

## Measured by

Not a product metric. Success is structural: the parity test exists and passes;
a version-11 fixture opens under a build at version 12 or later; a version-10
fixture is still refused with the export message; a version-99 fixture is still
refused with the upgrade message.
