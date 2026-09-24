// Unlike `memory.db`, a step may ask to rebuild instead of migrating in place,
// for a change that invalidates stored data outright. That is resolved in
// `db.rs`, not in the shared ladder, so `memory.db`, which must never rebuild,
// can share the ladder unchanged.

use crate::storage::migration_ladder::MigrationStep;

#[derive(Clone, Copy)]
pub(super) enum IndexMigrationKind {
    Migrate(MigrationStep),
    // A `Rebuild` anywhere between the found version and the target makes the
    // store rebuild once instead of running any step in that range.
    // Unconstructed outside tests: no registered step rebuilds yet.
    #[allow(dead_code)]
    Rebuild,
}

// Append only: number each step for the version it produces, and never
// renumber or reorder an existing one.
pub(super) const INDEX_MIGRATIONS: &[(i32, IndexMigrationKind)] =
    &[(18, IndexMigrationKind::Migrate(add_target_file))];

// Existing edges read as unresolved until a graph-only re-extraction fills
// `target_file`. That pass is owed only when the store holds edges, so a fresh
// index climbing the registry owes nothing.
fn add_target_file(conn: &rusqlite::Connection) -> anyhow::Result<()> {
    conn.execute_batch(include_str!("../../migrations/index_018.sql"))?;
    conn.execute(
        "INSERT OR REPLACE INTO index_meta (key, value)
         SELECT ?1, '1' WHERE EXISTS (SELECT 1 FROM graph_edges)",
        rusqlite::params![super::db::GRAPH_EDGES_REEXTRACT],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::INDEX_MIGRATIONS;
    use crate::storage::db::{CURRENT_SCHEMA_VERSION, INITIAL_SCHEMA_VERSION};
    use crate::storage::migration_ladder::{MigrationStep, assert_contiguous};

    fn unreachable_step(_: &rusqlite::Connection) -> anyhow::Result<()> {
        unreachable!("a version stamped Rebuild in the production registry has no step body")
    }

    #[test]
    fn the_production_registry_is_contiguous_from_the_first_migratable_version_to_the_current_one()
    {
        // `assert_contiguous` reads only the version, so a placeholder step suffices.
        let versions: Vec<(i32, MigrationStep)> = INDEX_MIGRATIONS
            .iter()
            .map(|&(v, _)| (v, unreachable_step as MigrationStep))
            .collect();
        assert_contiguous(
            &versions,
            INITIAL_SCHEMA_VERSION + 1,
            CURRENT_SCHEMA_VERSION,
        );
    }
}
