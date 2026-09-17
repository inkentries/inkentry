# ADR-101: Normalised tags and linked files in the memory projection

**Date:** 2026-09-17
**Deciders:** Founder (Johan); Architect
**Relationship to prior ADRs:** changes the shape of `memory.db`
([ADR-004](004-unified-memory-storage.md),
[ADR-084](084-memory-db-journal-mode-wal.md)) without changing the carrier
record ([ADR-086](086-carrier-representation-for-memory-edges.md)) or identity
([ADR-093](093-entity-id-is-the-portable-handle-for-memory-entries.md): tags
and files are already outside `entity_id`). The schema change
is an ordinary `memory.db` migration step, which needs #292 first. It is the file-level half of connecting memory to
the code graph; symbol-level links wait for ADR-097.

## Context

Tags and linked files are the only structured handles a memory entry has on a
concept or on code, and both are strings.

- `notes.tags` and `notes.linked_files` are comma-joined `TEXT`
  (`migrations/memory_001_initial.sql`). The carrier already holds them as
  `Vec<String>` (`storage/note_record.rs`); the flattening happens only in the
  projection.
- `memory add` splits `--tags` and `--files` on commas and trims
  (`cli/cmd/memory/add.rs`). Nothing else. `Auth` and `auth` are two tags. A
  path that exists nowhere is stored silently.
- `linked_files` is read in two places: `context --path` and the
  intent-overlap warning. It is not used in ranking and is never joined to
  `index.db`'s `files`, `chunks` or `graph_edges`. `memory.db` and `index.db`
  are separate files with no relation between them.
- On this repository's own store, 0 of 195 entries have a linked file.
- A comma inside a path or tag corrupts the list.

Three things on the roadmap need better than this: a concept vocabulary that
can be resolved without a model, ranking that knows an entry is about the file
being edited, and marking an entry out of date when the code it describes is
removed.

## Decision

### D1 - tags and files become rows

```sql
CREATE TABLE note_tags (
    note_uuid TEXT NOT NULL REFERENCES notes(uuid) ON DELETE CASCADE,
    tag       TEXT NOT NULL,            -- normalised (D2)
    PRIMARY KEY (note_uuid, tag)
);
CREATE INDEX idx_note_tags_tag ON note_tags(tag);

CREATE TABLE note_files (
    note_uuid TEXT NOT NULL REFERENCES notes(uuid) ON DELETE CASCADE,
    path      TEXT NOT NULL,            -- repository-relative, normalised (D3)
    state     TEXT NOT NULL CHECK (state IN ('tracked','untracked','missing')),
    checked_at INTEGER NOT NULL,
    PRIMARY KEY (note_uuid, path)
);
CREATE INDEX idx_note_files_path ON note_files(path);
```

The `notes.tags` and `notes.linked_files` columns are dropped. `memory_fts`
keeps indexing tags, fed from `note_tags`. The carrier is unchanged: it already
holds lists, and import and export map lists to rows. The entity-id collision
path that merges tags and files (`recover_from_entity_id_collision`) becomes
two `INSERT OR IGNORE`s.

### D2 - tags are normalised on the way in

A tag is Unicode NFC, lowercased, trimmed, with runs of whitespace and
underscores replaced by `-`. An empty result is dropped. Normalisation happens
once, in the core write path, so the CLI, import, harvest and the server all
agree. The original spelling is not kept.

`inkentry memory tags` lists the vocabulary with counts. It is what the agent
contract will point at ("reuse an existing tag when one fits"), and it is the
seed of the concept layer: a closed, inspectable vocabulary that alias
resolution can later work over without a model.

### D3 - linked files are repository-relative and checked, never refused

A path is made relative to the main worktree root, given forward slashes, and
stripped of `./`. A path that escapes the root is an error. `state` is then set
from git, not the filesystem: `tracked` if `git ls-files` knows it at `HEAD`,
`untracked` if it exists on disk only, `missing` otherwise.

A `missing` path is stored, with a warning on stderr and `state` in the JSON
response. It is not refused: an agent often records a decision about a file it
is about to create. `inkentry index` re-checks `state` for every row as part of
its normal pass, so a link whose file is later deleted or renamed turns
`missing` without anyone asking. That transition is the hook the out-of-date
marking feature will use; this ADR only records the state.

### D4 - reads that the rows make possible

- `search` and `context` accept `--file <path>` and `--tag <tag>` as exact
  filters backed by the new indexes.
- Unified search gains a file-affinity signal: an entry linked to a file that
  is among the code results, or passed by the caller as the file in hand, is
  ranked ahead of an equally distant entry that is not. The size of the boost
  is set against the eval sets, not in this ADR.
- `rec.orphan_rate` (ADR-098) becomes a query over `note_files.state`.

### D5 - the schema change is a migration step

Using the existing migration pattern (re-enabled from version 11 by #292):
create the
two tables, populate them by splitting the old columns
through D2 and D3 (with `state` computed lazily on the next index pass when
git is not available at open time), rebuild `memory_fts`, drop the columns.
The tag normalisation makes it a Rust step rather than plain SQL.

## Rationale

| Option | Considered | Rejected because |
|---|---|---|
| Keep comma strings, normalise at write time | No schema change | Still cannot index, join, or hold a comma; every reader keeps re-parsing |
| JSON arrays in the existing columns | One-line change, SQLite has `json_each` | Queryable but not indexable per element; file lookups on every search would scan |
| Put file links in `index.db`, next to `files` | Natural join to code | `index.db` is rebuilt rather than migrated, and these links are authored data that a rebuild cannot reproduce. `usage` is already the one authored table there and is already a wart |
| Refuse an unknown path | Clean data | Decisions are often recorded before the file exists; refusing would train agents to omit the link |
| Keep the caller's tag spelling beside the normalised one | Nothing lost | Two values to keep consistent for no reader that needs the original |
| Link to symbols now | The real goal | Symbol identity is lexical today; ADR-097 exists because `new` matches every `new`. File links are correct now and become the fallback later |

## Consequences

**Easier**

- "What did we decide about this file" is an indexed lookup, and can run
  automatically when an agent opens or edits a file.
- A tag vocabulary exists and can be shown, reused and later resolved against.

**Harder**

- The step rewrites two columns into rows on every existing store and needs
  a fixture with awkward inputs: commas inside values, mixed case, empty
  items, absolute paths.
- `dump` and import change shape for tags and files
  (`docs/dump-format.md`), with the old form still accepted on import.
- inkentry-server's schema follows.

**Revisit if**

- The file-affinity signal cannot be shown to help on `memory-anchor/v1` or
  `memory-commit/v1`: keep the filter, drop the ranking signal.
- Tag counts show a long tail of single-use tags: normalisation is not enough
  and alias resolution moves up.

## Security implications

- Path normalisation rejects paths that escape the repository root, which
  closes a way to make `context --path` or a future staleness pass reason about
  files outside the project.
- `state` is computed by invoking git with a path argument. Paths are passed
  after `--` and never through a shell.
- Tags and paths are caller-supplied text and remain untrusted wherever they
  are rendered.

## Measured by

`rec.orphan_rate` becomes computable. Share of new entries with at least one
`tracked` linked file (new state metric `rec.linked_rate`). `memory-anchor/v1`
label count rises from zero. Retrieval metrics on `memory-anchor/v1` and
`memory-commit/v1` with and without the file-affinity signal decide whether D4's
ranking change ships.
