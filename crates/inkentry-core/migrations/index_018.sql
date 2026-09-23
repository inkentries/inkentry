-- Schema version 18 (ADR-097): a nullable `target_file` column on
-- `graph_edges`. NULL means unresolved (today's bare-name join, unchanged);
-- non-NULL is the repo-relative path of the file that defines the edge's
-- resolved target. Existing rows get NULL on this ALTER, which is already
-- the correct "unresolved" state, so a migrated index behaves exactly as
-- before until a graph-only re-extraction pass fills it in.
--
-- The index backs the widened (target_name, target_file) identity a
-- resolved edge is looked up by, mirroring graph_edges_target_name_kind.

ALTER TABLE graph_edges ADD COLUMN target_file TEXT;

CREATE INDEX graph_edges_target_name_target_file ON graph_edges(target_name, target_file);
