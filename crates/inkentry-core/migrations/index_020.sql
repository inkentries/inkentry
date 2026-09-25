-- Chunks left to full-text search alone (ADR-104): tests, changelogs, JSON,
-- and unnamed windows of code. The embed queue skips them. The rule reads
-- the file's path and language, so it is computed in Rust
-- (`indexer::embed_scope::is_text_only`) when a chunk is written; the step
-- that runs this file computes it for existing rows. A vector such a chunk
-- already holds is kept.

ALTER TABLE chunks ADD COLUMN text_only INTEGER NOT NULL DEFAULT 0;
