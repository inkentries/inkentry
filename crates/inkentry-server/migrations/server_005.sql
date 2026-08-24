-- inkentry-server migration 005
-- Record the embedding model id per project (provenance), not just the dim.
-- A same-dim successor model must be a detectable, deliberate re-index rather
-- than a silent KNN corruption. NULL = legacy/unknown, lazy-stamped on first write.
ALTER TABLE projects ADD COLUMN embedding_model TEXT;
