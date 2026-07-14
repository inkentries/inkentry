-- ADR-068 A5: persist the content-addressed canonical identity of a memory entry.
--
-- `entity_id` is sha256 over the canonical JSON of {body, kind, title}. It is a
-- pure function of columns this table already holds, and identity is always
-- recomputed in Rust on read, so the column is never the system of record. That
-- is what makes this migration safe to run on an existing store with no
-- backfill: sha256 is not available in SQLite, and the backfill rule is a
-- separate open decision.
--
-- The index is deliberately NOT UNIQUE. Existing stores legitimately hold rows
-- with identical kind/title/body: the previous dedup hash folded in created_at
-- precisely so two same-text entries stayed distinct, so real data contains
-- duplicates under this key and a UNIQUE index would abort the migration.
-- Those duplicates are harmless and are left in place.

ALTER TABLE notes ADD COLUMN entity_id TEXT;

CREATE INDEX IF NOT EXISTS idx_notes_entity_id
    ON notes(entity_id) WHERE entity_id IS NOT NULL;
