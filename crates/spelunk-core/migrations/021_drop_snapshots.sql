-- spelunk-oss^67: remove the snapshot storage layer.
--
-- `spelunk search --as-of <sha>` was never wired to indexing: nothing ever
-- called create_snapshot/insert_snapshot_*/update_snapshot_stats, so
-- snapshots/snapshot_files/snapshot_chunks were always empty and --as-of
-- errored on every use. Dropping the dead tables outright rather than
-- gating the feature (founder ruling, 2026-07-04).
--
-- snapshot_embeddings is a sqlite-vec vec0 virtual table (created by
-- 017_snapshot_vectors.sql) and does not honour normal DROP TABLE ordering
-- concerns the way FK-linked tables would, but is dropped defensively before
-- its logical parents regardless.
DROP TABLE IF EXISTS snapshot_embeddings;
DROP TABLE IF EXISTS snapshot_chunks;
DROP TABLE IF EXISTS snapshot_files;
DROP TABLE IF EXISTS snapshots;
