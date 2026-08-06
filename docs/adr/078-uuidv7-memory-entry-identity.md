# ADR-078: UUIDv7 as the exported identity for memory entries

**Date:** 2026-08-06
**Deciders:** founder (Johan); architect
**Relationship to prior ADRs:** it respects
[ADR-059](059-git-notes-v1-format-freeze.md), which freezes the git-notes v1
record format, and therefore changes nothing in the carrier. It completes the
identity half of [ADR-076](076-memory-wire-contract-ownership.md) by making
the CLI, the team server and the hosted API agree on id shape rather than only
on id version.

A standing decision already selects UUIDv7 for newly generated identifiers
across the product. That decision was applied to the hosted service and never
actioned here beyond two generator fixes, and it did not reach the question
this record answers: what "UUIDv7 everywhere" means for a codebase whose
primary keys are SQLite integers rather than UUIDs of any version.

## Context

Memory entries have been keyed by a SQLite integer rowid since the first
schema. The hosted API keys the same entities by UUID. The gap is one of
shape, not of version, and it has been papered over at the boundary rather
than closed.

The `NoteId` newtype was introduced to let a local integer and a remote UUID
coexist behind one trait. It is an opaque string newtype that can carry
either, plus a narrowing helper that fails when a caller aims a remotely
minted id at a local store. That was the right tactical shape, and it is a
symptom rather than a fix: it exists because the two sides disagree.

Two facts make this the moment to close it.

First, every installation reaches this schema through a standalone one-way
migration tool, and this binary opens no database it did not create itself.
Nothing arrives un-migrated. There is therefore no compatibility window to
design, and no database in the field that a boundary adapter would have to
keep serving.

Second, the destination column already exists. `notes.uuid` was added as a
nullable UUIDv7 column to carry the cloud `external_id`, and its migration
already records the intent: cloud defaults to UUIDv7 for both identifiers and
we match it. Promoting it is a smaller change than introducing it.

### The constraint that shapes the decision

The direct reading of "UUIDv7 everywhere" is to retype `notes.id` as TEXT.
SQLite forbids it, for three independent reasons.

1. `memory_fts` is an FTS5 external-content table declared `content=notes,
   content_rowid=id`. FTS5 rowids are 64-bit integers by definition, so a
   TEXT key cannot back the table.
2. `note_embeddings` is a sqlite-vec `vec0` table keyed
   `note_id INTEGER PRIMARY KEY`, joined as an integer by every nearest
   neighbour query.
3. SQLite rejects `TEXT PRIMARY KEY AUTOINCREMENT` outright, and the column
   is declared `INTEGER PRIMARY KEY AUTOINCREMENT`.

Converting regardless would mean rebuilding the full-text index as a
contentless or standard FTS5 table, duplicating every title, body and tag
into it; converting the vector table to its TEXT-key variant; and rewriting
all three synchronisation triggers and every join. It is a large change with
no effect a user can observe.

## Decision

**The exported identity of a memory entry is `notes.uuid`, a UUIDv7. The
integer `notes.id` survives as an internal storage surrogate and is never
observable outside the storage layer.**

Every identifier that crosses a boundary is a UUIDv7: command output, every
JSON and JSONL field, the plumbing wire, the HTTP API, both endpoints of a
graph edge, and every id a user can type. The integer remains solely as an
FTS5 and vector-table addressing artifact, in the same category as the FTS5
shadow tables, and no code path outside
`crates/inkentry-core/src/storage/memory/` may read it.

Consequently:

1. `notes.uuid` becomes `NOT NULL` with a full unique index. Existing rows are
   backfilled with a UUIDv7 whose timestamp is seeded from the row's own
   `created_at`, so the back catalogue keeps its creation ordering. Minting
   from the wall clock would stamp every historical entry with the migration
   instant and discard the ordering property that motivates v7.
2. `notes.superseded_by` and both endpoints of `memory_edges` become TEXT
   uuids. `MemoryEdge`, the backend edge methods, and the graph output type
   carry `NoteId` rather than integers.
3. The team server gains a UUIDv7 exported identity for its own notes and
   returns strings on every route that carries an id. Without this the two
   sides still disagree on shape and the adapter cannot be removed.
4. `NoteId` becomes the sole id type. Its integer duality is deleted: the
   integer constructor and accessor, the `From<i64>` conversion, the numeric
   serialisation branch, the integer deserialisation arms, and the narrowing
   helper together with its error message.
5. The `external_id` minted for cloud is corrected from v4 to v7, matching
   what its own migration already documents.

### What is deliberately not in scope

**The code index.** `files`, `chunks`, `specs`, `conventions` and
`graph_edges` keep integer keys. That database is machine-local and fully
rebuildable, its ids never reach a user or a wire and never outlive a
reindex, and converting them means fighting the same full-text and vector
constraints a second time for nothing observable.

**The project registry.** Its ids are internal, never printed and never sent.

**`entity_id`.** It stays a content hash over kind, title and body. It is not
a competing identity: it is the convergence key the git-notes fold groups on
and the deduplication key on import. Being content-derived is what makes it
correct for that job and what disqualifies it as a stable handle, since it
changes when the entry is edited.

**The git-notes record format.** ADR-059 froze v1. The record's integer `id`
field is already documented as non-identity, is a wall-clock stamp in two of
its three writers, and is re-minted on import. It is untouched.

## Consequences

### What this buys

The two codebases agree on id shape. The boundary adapter becomes dead code
rather than a maintained compatibility surface, and the narrowing helper and
its "this project numbers entries with integers" error disappear along with
the condition that made them true.

The initial schema declares the final shape. Because no existing database is
opened in place, the identity columns are correct from creation rather than
being added, backfilled and then constrained.

Graph output stops being self-inconsistent. It currently emits a
UUID-capable `id` next to integer edge endpoints for the same entities,
because the edge types never received the newtype treatment.

Ids become stable across a reindex and unique across machines. Two
installations no longer mint colliding ids by each starting a sequence at 1,
which is a failure the fold logic currently compensates for by ordering on a
content hash instead.

### What this breaks

**Every id in output becomes a string.** A script parsing an integer `id`
out of JSON breaks. This is a breaking change and belongs in the release
notes and the changelog.

**Old ids stop resolving.** A user with a numeric id in shell history or in a
script gets a miss. This is deliberate: the migration is one-way and there is
no resolution path. It is handled with an error message stating that entries
are identified by UUID, not with a lookup table.

**The integer is not gone, only unexported.** Anyone reading the schema will
find `notes.id` still an integer. That is the price of keeping external
content full-text search and the vector index, and it is recorded here so
the next reader does not file it as an oversight.

### Migration

The schema in this decision is what the initial memory schema declares.
Existing installations do not migrate in place: a standalone migration tool
exports the authored tables, and this binary creates every database fresh and
imports them. There is therefore no migration step for this change, no
nullable intermediate state for the uuid column, and no in-place conversion
of the edge or supersede columns.

One property must be protected during import. The UUIDv7 minted for a row
that does not already have one takes its timestamp from that row's own
creation time, never from the wall clock. The import processes an entire back
catalogue in a single pass, so wall-clock minting would stamp every
historical entry with the same instant and discard the ordering property that
motivates v7 for all history at once.

Foreign keys are already enforced on the memory database, so the declared
cascade on edges is live rather than inert. The store never issues
`PRAGMA foreign_keys=ON`, but the bundled SQLite this workspace links against
is compiled with foreign keys on by default, so every connection enforces
them, including the one that never asks. That means the guarantee currently
rests on a compile flag of a vendored dependency, and a guarantee of that
shape disappears silently the day anyone builds against a system SQLite. The
initial schema therefore declares foreign-key enforcement rather than
inheriting it, and a test pins that it is on.

Two consequences follow for the import. First, it must insert entities before
the relationships that reference them; the export format does not constrain
record order, so the reader buffers or makes two passes. That ordering is a
correctness requirement, not a nicety. Second, the store's manual edge
cleanup is belt-and-braces over a live cascade rather than the only
mechanism, so confirm that before deleting it.

One further detail of the current store should be confirmed rather than
assumed. Two maintenance routines run on every open outside the version
ladder, backfilling the content hash and promoting its index to unique. With
the hash carried verbatim by the export and declared unique in the initial
schema, both should become unnecessary; confirm that and delete them rather
than leaving them running against a schema that no longer needs them.

## Alternatives considered

**Retype the primary key to TEXT.** Rejected on the three SQLite constraints
above. Large, risky, and indistinguishable to a user from the decision taken.

**Keep integers locally and translate at the boundary.** This is the status
quo. Rejected because it makes the adapter permanent, and because a one-way
migration removes the only reason it had to exist.

**Rebuild the local database from the git-notes carrier.** Rejected on
evidence: the carrier cannot represent relates-to or contradicts edges at
all, does not carry the local uuid, and its import path overwrites each
entry's source commit. A carrier-sourced rebuild is silent data loss, and it
would re-mint the very identifier this decision exists to stabilise.

**Rebuild by copying rows from the existing store into a fresh schema.**
Adopted, and it is what the migration described above does. It shares none of
the carrier's losses: rows are copied verbatim, so edges, the local uuid and
each entry's source commit all survive. It is also what allows the initial
schema to declare the final shape directly rather than reaching it through a
sequence of alterations.

**Reuse `entity_id` as the identity.** Rejected: a content hash changes when
content changes, so it cannot be a stable handle, and it is already load
bearing as the convergence key.
