# ADR-086: `relates_to` and `contradicts` travel on the git-notes carrier as portable id references

**Date:** 2026-08-19
**Deciders:** founder (Johan), ruling of 2026-08-19; architect (this record)
**Relationship to prior ADRs:** extends
[ADR-069](069-git-notes-sharing-pre-push-hook-and-tracking-refspec.md)'s carrier
and [ADR-068](068-entity-id-content-addressed-identity.md)'s portable identity.
Does not change the notes refspec, the pre-push hook's opt-in status, or the
`schema_version` contract `docs/stability.md` records for the ref.

## Context

`memory_edges` has three kinds, all directed, primary-keyed on
`(from_id, to_id, kind)`:

```sql
CREATE TABLE memory_edges (
    from_id TEXT NOT NULL REFERENCES notes(uuid) ON DELETE CASCADE,
    to_id   TEXT NOT NULL REFERENCES notes(uuid) ON DELETE CASCADE,
    kind    TEXT NOT NULL CHECK(kind IN ('supersedes','relates_to','contradicts')),
    PRIMARY KEY (from_id, to_id, kind)
);
```

Exactly one of those kinds reaches the carrier. `supersedes` travels as
`NoteRecord::superseded_by_entity_id`, a single typed field written by
`append_state_update` (`storage/git_notes/mod.rs:488-516`). `NoteRecord` has no
general edge field, and `GitNotesBackend` refuses both edge methods outright
(`git_notes/backend_impl.rs:201-207`).

### What the gap actually is, narrowed

An earlier statement of this problem said `relates_to` and `contradicts` "exist
only in the local `memory.db`". That is too strong, and the correction matters
for the design:

- **`relates_to` is pushed one way to an explicitly configured team server** by
  `sync` (`sync/push/mod.rs:341-380`). `contradicts` is server-generated and
  never pushed.
- **All three kinds cross in a portable dump.** `dump/import.rs:391-452` writes
  `relates_to` and `contradicts` straight into the local store and re-projects
  only `supersedes` onto the carrier.
- Nothing returns the other way: `sync/pull.rs` has no edge handling and
  `RemoteMemoryBackend::get_edges` returns empty lists by design.

So the accurate gap is narrower and worse than "edges are local": **the carrier
is the only transport that cannot express these two kinds, and it is the one
the default no-server setup depends on.** A team sharing memory the way the
product recommends by default loses two thirds of its graph, while a team with
a server or a dump does not.

## Decision

**Carry outgoing `relates_to` and `contradicts` edges on the note record, as
references to the target's portable entity id.** The graph is reconstructed
from the notes at import rather than queried from the ref.

The founder ruling, 2026-08-19:

> Yes the edges are just id references right, they should travel with the git
> note. You may not be able to query them from the git notes alone but you
> should be able to reconstruct the graph from the notes, once the notes are
> imported into the db.

That second sentence is the scope boundary and it is what keeps this cheap. The
carrier is not being made queryable. It is being made **sufficient**: a clone
that imports every note ends up with the same graph as the machine that wrote
them, and nothing has to interrogate the ref to get there.

### D1 - an additive field on `NoteRecord`, keyed by `entity_id`

A new optional field holding this note's **outgoing** edges, each an
`(kind, target entity_id)` pair, serialised only when non-empty.

**Outgoing only, on the source note.** `memory_edges` is directed and
primary-keyed on all three columns, so carrying each edge once from its source
reconstructs the table exactly. Writing it on both endpoints would put the same
row on the carrier twice and create a second place for the two copies to
disagree.

**Keyed by `entity_id`, not by `notes.uuid`.** The carrier already has a
portable identity for exactly this purpose, and `superseded_by_entity_id` is
the precedent: it is the pattern to follow, not to work around. Import resolves
each target entity id to the local row the same way the supersede path already
does.

### D2 - `supersedes` stays where it is and is not duplicated

`superseded_by_entity_id` continues to carry the supersede edge, and
`supersedes` is **not** added to the general edge list.

Duplicating it would give import two independent paths to create the same row,
which can disagree after a partial fetch and would need reconciling for no
benefit. The asymmetry is worth the one line of explanation it costs: supersede
is a state transition on the entry, which is why it is a field on the record,
and the other two are graph edges between entries.

### D3 - `schema_version` stays at 1

This is additive under the existing version, not a bump to 2.

`docs/stability.md` records that a git-notes record with a **higher
`schema_version` than the reader knows is refused rather than misread**. So
bumping the version would make every older reader refuse every new note,
turning a graph improvement into a memory-sharing outage for anyone mid-upgrade
on a shared ref. `NoteRecord` does not set `deny_unknown_fields`, so an older
reader ignores the new key and reads the note exactly as it does today, losing
only the edges it could not have had anyway.

A version bump is for a change an old reader must **not** attempt. This is the
opposite: an old reader is strictly better off reading the note without the
edges than refusing it.

### D4 - edges apply after notes, and a dangling target is skipped, not fatal

`memory_edges.from_id` and `to_id` are foreign keys onto `notes(uuid)`, so an
edge inserted before its endpoints exist fails outright.

Import therefore applies all notes first, then edges, and **an edge whose
target is still absent is skipped without failing the import**. A partial
fetch, an entry excluded with `store_in_git_notes = false`, or a note deleted
on the writing machine all produce that case legitimately.

Skipping is not silent: a count of unresolved edges belongs in the import
report, because "your graph is thinner than the source" is exactly the class of
outcome this product has repeatedly failed to surface. Re-running import after
a fuller fetch resolves them, and the primary key makes re-application
idempotent by construction.

## Consequences

- **`docs/stability.md`** describes what crosses on the ref and needs updating.
  So does the public documentation that now says two of the three edge kinds
  do not travel, which becomes wrong the moment this ships.
- **The dump path is prior art, not a duplicate.** `dump/import.rs` already
  decided how these edges serialise and which re-project onto the carrier. Read
  it before inventing a shape; where the two can share code they should, since
  a dump import and a notes import now converge on the same end state.
- **`GitNotesBackend`'s edge methods** can stop refusing, at least for writes
  that ride an appended record. Reads from the ref remain unsupported, per the
  scope boundary in the ruling.
- **`relates_to`'s existing one-way push to a team server is untouched.**
  Whether the server path should converge on this representation is a separate
  question and is not opened here.

## Alternatives rejected

**A separate edge ref, or edge-only records on the notes ref.** Queryable
without importing, and it makes the carrier a graph store. Rejected on the
ruling's own boundary: reconstruction after import is enough, and a second ref
is a second thing to fetch, merge, and get wrong.

**Bumping `schema_version` to 2.** Rejected under D3: it converts an additive
improvement into a refusal for every older reader on a shared ref.

**Carrying edges on both endpoints.** Rejected under D1: it puts one directed
row on the carrier twice and invents a disagreement that cannot otherwise
happen.
