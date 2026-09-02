# ADR-093: `entity_id` is the portable handle for memory entries

**Date:** 2026-09-02
**Deciders:** founder (Johan); architect
**Relationship to prior ADRs:** applies
[ADR-068](068-zero-setup-onboarding-git-notes-memory-fallback.md)'s
content-addressed identity to the command surface; keeps
[ADR-078](078-uuidv7-memory-entry-identity.md)'s UUIDv7 as the store identity;
follows [ADR-086](086-carrier-representation-for-memory-edges.md), which already
references entries by `entity_id` on the carrier; leaves
[ADR-092](092-memory-sync-recovery-force-restore-and-total-row-signal.md)'s
carry-and-restore in force where a server exists. Closes the carrier-identity
question [ADR-059](059-git-notes-v1-format-freeze.md) left open.

## Context

`memory list`, `show` and `context` print the entry's UUIDv7. That id is minted
per machine: a note that travels on `refs/notes/inkentry` and is imported
elsewhere gets a fresh one there, so the id a user copied into a document, a
commit message or a handoff resolves nowhere but on the machine that minted it.
Reported from a git-notes round trip, where the same entries came back with the
same titles, bodies and timestamps and different ids.

The entry already has a portable identity. `entity_id` is `sha256` over
`{kind, title, body}` (ADR-068), stored as a `UNIQUE` column on `notes`, carried
in every git-notes record, and the key the carrier fold, the import dedupe and
the edge references all converge on. Nothing displays it and nothing resolves
it, so the ephemeral id is the only one a user can see and quote.

## Decision

**The quotable handle of a memory entry is its `entity_id`. The UUIDv7 stays
the store's identity, stays per machine, and is not made portable.**

### D1 - the UUIDv7 diverges across machines by design

In a git-notes-only setup there is no id-issuing authority, so two machines
holding the same note hold different UUIDv7s and nothing reconciles them. This
is the ADR-068 position and it stands.

ADR-092's carry-and-restore is not extended to the carrier. It is sound for a
team server because `sync_id` is issued by one authority and can be handed back
to it; on git notes, two developers importing each other's notes would each
restore an independently minted id with nothing to resolve the collision, and
the format would carry two competing cross-machine identities. Where a server
exists, `sync_id` remains the authoritative issued id and ADR-092 is unchanged.
Git-notes-only users reconcile by `entity_id`, and nothing about reconciliation
depends on running a server.

### D2 - the handle is a short prefix; the full value is always available

The human table shows the first **12 characters** of `entity_id`. Lookup
accepts the full 64-character value or any prefix of **8 or more** characters.

A prefix that matches more than one entry fails: the message names the prefix
and the number of entries it matched, and says to give more characters. It
never picks one. A prefix shorter than 8 characters is not looked up as an
entity id and falls through to the existing not-found message.

Twelve characters are what a person can type and compare, and in a store of the
size a project accumulates the chance that two entries share them is
negligible; eight is the floor at which an accidental match against unrelated
input stops being plausible. Both are display and parsing rules over the
existing column, and the width may change without a storage change.

`--format json` and `--format jsonl` carry the full `entity_id` on every entry,
always, as a field named `entity_id`. The `id` field keeps the UUIDv7. The
handle is a display rule for humans and is never what a machine consumer
receives.

### D3 - where the handle appears

`memory list` and `context` lead each entry line with the handle in place of
the UUIDv7. `memory show` prints the handle in the heading and the full
`entity_id` and the UUIDv7 as fields, so the one screen a user reads before
quoting an entry holds both in full.

The handle is computed from the entry's own `kind`, `title` and `body`, so every
backend and every listing shows the same one for the same entry: the SQLite
store, a team server, a linked project's entries, and the git-notes backend,
whose record token is no longer displayed as an id at all.

### D4 - `show`, `archive` and `supersede` resolve the handle

Each takes an id token and resolves it in this order: the UUIDv7, then the
`entity_id` by exact match, then by prefix under D2. A miss at the end of that
chain is the existing not-found message, unchanged, including its wording for a
numeric id. A UUID carries hyphens and a hex prefix does not, so the two forms
never compete for a match.

The `UNIQUE` index on `entity_id` makes the exact lookup a point read; the
prefix lookup is a range over the same index.

### D5 - import says what it re-minted

`init` when it hydrates from git notes, and `import`, report that the ids this
machine shows were minted here and that the entity id is the one to quote across
machines. A count of imported entries with every visible id silently changed is
the outcome this record exists to end.

### D6 - no storage change

`entity_id` is already persisted, indexed and carried. Nothing is migrated,
no column changes, the git-notes record format stays at `schema_version` 1,
and the portable dump is untouched.

## What this settles for the carrier

A git-notes record's identity is its `entity_id`. The record's `id` field is a
timestamp or a rowid depending on the writer, is documented as non-identity, and
is neither compared nor restored on import. That answers the question ADR-059
left open: any path that writes entries onto `refs/notes/inkentry`, including an
import that writes through to it, dedupes on `entity_id` and carries no other
identity.

## Consequences

- **A quoted handle resolves on every machine that holds the note.** The
  UUIDv7 in an old document still resolves on the machine that minted it and
  nowhere else, as before.
- **JSON consumers gain a field and lose nothing.** `entity_id` is additive on
  a best-effort surface under `docs/stability.md`, and consumers already must
  tolerate unknown fields. `docs/stability.md` records that `entity_id` is the
  portable identity of an entry and `id` is the per-machine one.
- **An entry's handle does not move.** Entries are not edited in place; a
  correction is a new entry with its own `entity_id` (ADR-068). Two entries
  with identical text share one handle because they are, under ADR-068, one
  entry.
- **The prefix is a convenience, and the full value is the record.** Anything
  written to last, an ADR, a handoff, a script, should quote the full
  `entity_id` from `show` or from JSON.
- **The changelog** says that `memory list`, `show` and `context` now show the
  portable entity id, that the three commands accept it, and that it is the id
  to quote when referring to an entry outside this machine.

## Acceptance criteria

1. `memory list`, `show` and `context` show the 12-character handle; their
   `--format json` output carries the full `entity_id` and the unchanged `id`.
2. `show`, `archive` and `supersede` accept a UUIDv7, a full `entity_id`, and a
   prefix of 8 or more characters; an ambiguous prefix fails naming the count
   and resolves nothing; a shorter token and a miss produce the existing
   not-found message.
3. The same entry shows the same handle on every backend.
4. `init` hydration and `import` state that local ids were minted on this
   machine and name the entity id as the portable one.
5. No migration, no schema change, and no change to the git-notes record or the
   dump format.
