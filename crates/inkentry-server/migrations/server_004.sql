-- inkentry-server migration 004
-- Additive cross-machine identity for server-stored memory entries.
--
-- `remote_id` is the canonical cross-machine id (a uuid string). It is nullable
-- and additive: existing rows keep NULL. A partial UNIQUE index keeps non-NULL
-- values unique without forcing a value onto legacy rows. Mirrors the client
-- shape in spelunk-core migration 020.

ALTER TABLE notes ADD COLUMN remote_id TEXT;

CREATE UNIQUE INDEX IF NOT EXISTS idx_notes_remote_id
    ON notes(remote_id) WHERE remote_id IS NOT NULL;
