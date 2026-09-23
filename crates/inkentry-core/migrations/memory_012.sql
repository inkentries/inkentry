-- Schema version 12 (ADR-101): tags and linked files move out of the
-- comma-joined `notes.tags` / `notes.linked_files` columns into rows.
--
-- This file is the DDL half of the step. `storage/memory/migrate.rs` runs it,
-- then copies the old columns into the new tables (a Rust pass, because tag
-- normalisation is Unicode NFC), then runs memory_012_drop_legacy_columns.sql.
--
-- `memory_fts` stops being an external-content table over `notes`: external
-- content cannot draw a column from a second table, and tags now live in
-- `note_tags`. It is rebuilt self-contained, with triggers on `note_tags`
-- keeping its `tags` column current. The rebuild runs before the data pass,
-- so `tags` starts empty here and the `note_tags_fts_insert` trigger fills it
-- as rows arrive.

CREATE TABLE note_tags (
    note_uuid TEXT NOT NULL REFERENCES notes(uuid) ON DELETE CASCADE,
    tag       TEXT NOT NULL,
    PRIMARY KEY (note_uuid, tag)
);
CREATE INDEX idx_note_tags_tag ON note_tags(tag);

CREATE TABLE note_files (
    note_uuid  TEXT NOT NULL REFERENCES notes(uuid) ON DELETE CASCADE,
    path       TEXT NOT NULL,
    state      TEXT NOT NULL CHECK (state IN ('tracked','untracked','missing')),
    checked_at INTEGER NOT NULL,
    PRIMARY KEY (note_uuid, path)
);
CREATE INDEX idx_note_files_path ON note_files(path);

DROP TRIGGER memory_fts_insert;
DROP TRIGGER memory_fts_delete;
DROP TRIGGER memory_fts_update;
DROP TABLE memory_fts;
CREATE VIRTUAL TABLE memory_fts USING fts5(
    title,
    body,
    tags
);
INSERT INTO memory_fts(rowid, title, body, tags)
SELECT n.id, n.title, n.body,
       COALESCE(
           (SELECT GROUP_CONCAT(nt.tag, ' ') FROM note_tags nt WHERE nt.note_uuid = n.uuid),
           ''
       )
FROM notes n;

CREATE TRIGGER memory_fts_insert AFTER INSERT ON notes BEGIN
    INSERT INTO memory_fts(rowid, title, body, tags) VALUES (new.id, new.title, new.body, '');
END;
CREATE TRIGGER memory_fts_delete BEFORE DELETE ON notes BEGIN
    DELETE FROM memory_fts WHERE rowid = old.id;
END;
CREATE TRIGGER memory_fts_update AFTER UPDATE ON notes BEGIN
    UPDATE memory_fts SET title = new.title, body = new.body WHERE rowid = new.id;
END;
CREATE TRIGGER note_tags_fts_insert AFTER INSERT ON note_tags BEGIN
    UPDATE memory_fts
    SET tags = (SELECT COALESCE(GROUP_CONCAT(tag, ' '), '') FROM note_tags WHERE note_uuid = new.note_uuid)
    WHERE rowid = (SELECT id FROM notes WHERE uuid = new.note_uuid);
END;
CREATE TRIGGER note_tags_fts_delete AFTER DELETE ON note_tags BEGIN
    UPDATE memory_fts
    SET tags = (SELECT COALESCE(GROUP_CONCAT(tag, ' '), '') FROM note_tags WHERE note_uuid = old.note_uuid)
    WHERE rowid = (SELECT id FROM notes WHERE uuid = old.note_uuid);
END;
