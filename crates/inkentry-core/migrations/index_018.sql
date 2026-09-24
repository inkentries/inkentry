-- Schema version 18 (ADR-097): `graph_edges.target_file`, the repo-relative
-- path of the file defining the callee an edge resolved to. NULL means
-- unresolved: consumers fall back to joining on `target_name` alone, which is
-- what every edge meant before this column existed, so existing rows need no
-- rewrite to stay correct.
--
-- `storage/index_migrate.rs` runs this, then records that the next index run
-- owes every file a graph-only re-extraction to fill the column.

ALTER TABLE graph_edges ADD COLUMN target_file TEXT;

-- Backs the two-column join a resolved edge allows: `chunks.name =
-- target_name` in the file `target_file`.
CREATE INDEX graph_edges_target_name_file ON graph_edges(target_name, target_file);
