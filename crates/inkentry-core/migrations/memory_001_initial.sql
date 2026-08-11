-- Initial memory schema. Declares the final shape directly: there is no
-- ladder to climb, because this binary opens no memory store it did not
-- create itself. A store carrying data from an earlier product is crossed
-- with `inkentry import`, never opened in place (ADR-078).

-- `uuid` is the exported identity (a UUIDv7), NOT NULL and uniquely indexed
-- from creation. `id` is a storage surrogate: `memory_fts` is an FTS5
-- external-content table keyed `content_rowid=id` and `note_embeddings` is a
-- vec0 table keyed `note_id INTEGER PRIMARY KEY`, and neither can key on TEXT.
-- Nothing outside this module may read it. AUTOINCREMENT is what stops SQLite
-- reusing a deleted row's rowid, which would otherwise hand a new note the
-- previous occupant's embedding.
CREATE TABLE IF NOT EXISTS notes (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    uuid          TEXT    NOT NULL,
    kind          TEXT    NOT NULL DEFAULT 'note',   -- decision | context | requirement | note
    title         TEXT    NOT NULL,
    body          TEXT    NOT NULL,
    tags          TEXT,                              -- comma-separated
    linked_files  TEXT,                              -- comma-separated file paths
    created_at    INTEGER NOT NULL DEFAULT (unixepoch()),
    status        TEXT    NOT NULL DEFAULT 'active',
    -- No ON DELETE action: a delete that would strand this reference must fail
    -- rather than silently rewrite history. `memory dedupe` clears the link
    -- before deleting a loser, which is why it can delete in any order.
    superseded_by TEXT    REFERENCES notes(uuid),
    source_ref    TEXT,                              -- git commit SHA for harvested entries
    valid_at      INTEGER,
    invalid_at    INTEGER,
    -- Content-addressed convergence key over kind/title/body. The import
    -- carries it verbatim and never recomputes it, and its unique index below
    -- exists from creation, so there is no window in which duplicates can
    -- appear and nothing to backfill or promote on open.
    entity_id     TEXT    NOT NULL,
    remote_id     TEXT                               -- cloud-minted id, once known
);

-- Uniqueness is declared as named indexes rather than inline column
-- constraints so it is visible under `PRAGMA index_list` and can be named in a
-- test. `idx_notes_uuid` is also the parent-key index the two foreign keys
-- below resolve against.
CREATE UNIQUE INDEX IF NOT EXISTS idx_notes_uuid      ON notes(uuid);
CREATE UNIQUE INDEX IF NOT EXISTS idx_notes_entity_id ON notes(entity_id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_notes_remote_id ON notes(remote_id);
CREATE INDEX        IF NOT EXISTS idx_memory_invalid_at ON notes(invalid_at);

-- Semantic embeddings for notes (one row per note).
CREATE VIRTUAL TABLE IF NOT EXISTS note_embeddings USING vec0(
    note_id    INTEGER PRIMARY KEY,
    embedding  FLOAT[896]
);

-- FTS5 full-text index over notes. `content=` avoids duplicating the text;
-- the triggers below keep the index in sync.
CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(
    title,
    body,
    tags,
    content=notes,
    content_rowid=id
);

CREATE TRIGGER IF NOT EXISTS memory_fts_insert
AFTER INSERT ON notes BEGIN
    INSERT INTO memory_fts(rowid, title, body, tags)
    VALUES (new.id, new.title, new.body, COALESCE(new.tags, ''));
END;

CREATE TRIGGER IF NOT EXISTS memory_fts_delete
BEFORE DELETE ON notes BEGIN
    INSERT INTO memory_fts(memory_fts, rowid, title, body, tags)
    VALUES ('delete', old.id, old.title, old.body, COALESCE(old.tags, ''));
END;

CREATE TRIGGER IF NOT EXISTS memory_fts_update
AFTER UPDATE ON notes BEGIN
    INSERT INTO memory_fts(memory_fts, rowid, title, body, tags)
    VALUES ('delete', old.id, old.title, old.body, COALESCE(old.tags, ''));
    INSERT INTO memory_fts(rowid, title, body, tags)
    VALUES (new.id, new.title, new.body, COALESCE(new.tags, ''));
END;

-- Memory entry relationships. Endpoints are uuids, matching every other id
-- that leaves this module. The cascade is live: `MemoryStore::open` issues
-- `PRAGMA foreign_keys = ON` rather than inheriting it from whichever SQLite
-- the workspace happened to link against.
CREATE TABLE IF NOT EXISTS memory_edges (
    from_id    TEXT NOT NULL REFERENCES notes(uuid) ON DELETE CASCADE,
    to_id      TEXT NOT NULL REFERENCES notes(uuid) ON DELETE CASCADE,
    kind       TEXT NOT NULL CHECK(kind IN ('supersedes', 'relates_to', 'contradicts')),
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY (from_id, to_id, kind)
);

CREATE INDEX IF NOT EXISTS idx_memory_edges_from ON memory_edges(from_id);
CREATE INDEX IF NOT EXISTS idx_memory_edges_to   ON memory_edges(to_id);

-- ADR-077: gate the read-path git-notes import on notes-ref OID movement.
--
-- One row (id = 0) records the OIDs seen at the last merge and the last import,
-- so a read whose notes refs have not moved since skips both the merge
-- subprocess and the import walk. Derived state, and keyed on OIDs that a ref
-- rename invalidates, so `inkentry import` deliberately does not populate it:
-- the cost of starting empty is one redundant walk.
CREATE TABLE IF NOT EXISTS notes_import_state (
    id INTEGER PRIMARY KEY CHECK (id = 0),
    last_merged_tracking_oid TEXT,
    last_imported_working_oid TEXT
);
