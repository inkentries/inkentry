-- Schema version 12 (ADR-101), last part: the comma-joined columns go once
-- their contents have been copied into `note_tags` / `note_files` by the data
-- pass in `storage/memory/migrate.rs`. Kept apart from memory_012.sql because
-- it must run after that pass, and the pass reads these columns.
ALTER TABLE notes DROP COLUMN tags;
ALTER TABLE notes DROP COLUMN linked_files;
