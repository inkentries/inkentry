-- inkentry-server migration 008
-- Promote `sync_id` from a delta-pull cursor to the note's exported identity
-- (ADR-078). `notes.id` stays an INTEGER PRIMARY KEY because `note_embeddings`
-- is a vec0 virtual table keyed `note_id INTEGER PRIMARY KEY`, but it stops
-- being observable over HTTP: every route now carries the UUIDv7.
--
-- Every statement here is idempotent on its own, which is what lets it run
-- unconditionally on a runner with no version stamp — the same property
-- migration 006 relies on. The NULL rows migration 007 left behind are healed
-- by `ServerDb::backfill_missing_sync_ids`, which seeds each id's v7 timestamp
-- from that row's own `created_at`.

-- 007's index was partial (`WHERE sync_id IS NOT NULL`), which is what an
-- additive nullable column needs. With every row backfilled, uniqueness is
-- unconditional. `DROP` + `CREATE ... IF NOT EXISTS` re-runs harmlessly.
DROP INDEX IF EXISTS idx_notes_sync_id;

CREATE UNIQUE INDEX IF NOT EXISTS idx_notes_sync_id ON notes(sync_id);

-- SQLite cannot add NOT NULL to an existing column without rebuilding the
-- table, and rebuilding `notes` would strand `note_embeddings` (a vec0 table
-- with no foreign key to cascade) and cascade-delete `note_edges`. A trigger
-- buys the same guarantee at the only point that matters: a write path that
-- forgets to mint an id fails loudly instead of storing a note the HTTP API
-- cannot name.
CREATE TRIGGER IF NOT EXISTS notes_sync_id_required_on_insert
BEFORE INSERT ON notes
WHEN NEW.sync_id IS NULL
BEGIN
    SELECT RAISE(ABORT, 'notes.sync_id is the exported note identity and must not be null');
END;

CREATE TRIGGER IF NOT EXISTS notes_sync_id_required_on_update
BEFORE UPDATE OF sync_id ON notes
WHEN NEW.sync_id IS NULL
BEGIN
    SELECT RAISE(ABORT, 'notes.sync_id is the exported note identity and must not be null');
END;
