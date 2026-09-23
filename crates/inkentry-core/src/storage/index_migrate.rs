//! `index.db`'s forward-migration registry: steps
//! [`super::db::CURRENT_SCHEMA_VERSION`] + 1 and up, resolved by
//! `db.rs::Database::migrate_or_rebuild` before reaching the store-agnostic
//! runner in `storage::migration_ladder`.
//!
//! Unlike `memory.db`, a step here may ask to [`IndexMigrationKind::Rebuild`]
//! instead of migrating in place, for a change that invalidates stored data
//! outright (a different embedding space, a chunking change) rather than one
//! an in-place `ALTER`/backfill can fix. That kind is resolved in `db.rs`,
//! not inside `migration_ladder.rs`: the shared runner stays ignorant of
//! rebuilding, which is what keeps `memory.db` — which must never rebuild —
//! on the exact same runner.

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::storage::migration_ladder::MigrationStep;

/// What a registered version does to get the store there from the version
/// immediately below.
// `Rebuild` is never constructed outside the tests below: no step has needed
// it yet. Both variants are matched on unconditionally in `db.rs`.
#[allow(dead_code)]
#[derive(Clone, Copy)]
pub(super) enum IndexMigrationKind {
    /// Applied in place through the shared ladder, like every `memory.db` step.
    Migrate(MigrationStep),
    /// Discards and recreates the file instead, carrying `usage` across like
    /// any other rebuild. Resolved in `db.rs` before the ladder runs: when a
    /// registered version between the store's found version and the target
    /// is a `Rebuild`, the store rebuilds once instead of running any of the
    /// steps in that range, migrate or rebuild alike, since the rebuild
    /// already lands the store at the current shape.
    Rebuild,
}

/// The production registry. Add entries at the end, numbered for the version
/// each one produces; never renumber or reorder an existing entry.
pub(super) const INDEX_MIGRATIONS: &[(i32, IndexMigrationKind)] =
    &[(18, IndexMigrationKind::Migrate(add_target_file_column))];

/// ADR-097 step 18: a nullable `target_file` column on `graph_edges` (NULL =
/// unresolved, today's bare-name fallback) and the index backing its widened
/// (target_name, target_file) identity — DDL in `migrations/index_018.sql`.
/// Existing rows already read correctly as unresolved, but backfilling
/// `target_file` needs a real `inkentry index` run, not this migration step,
/// so this records the same marker
/// [`crate::storage::Database::mark_pass_owed`] would — but only when there
/// is a row to backfill: this step also runs on a brand-new index's climb
/// from the frozen initial schema (`Database::create_fresh`), which has no
/// `graph_edges` yet and nothing owed.
fn add_target_file_column(conn: &Connection) -> Result<()> {
    conn.execute_batch(include_str!("../../migrations/index_018.sql"))
        .context("applying index_018.sql")?;
    let has_existing_edges: bool = conn
        .query_row("SELECT EXISTS(SELECT 1 FROM graph_edges)", [], |r| r.get(0))
        .context("checking for pre-existing graph edges")?;
    if has_existing_edges {
        conn.execute(
            "INSERT OR REPLACE INTO index_meta (key, value) VALUES ('graph_edges_reextract', '1')",
            [],
        )
        .context("recording that graph edges owe a re-extraction pass")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{INDEX_MIGRATIONS, add_target_file_column};
    use crate::storage::db::{CURRENT_SCHEMA_VERSION, INITIAL_SCHEMA_VERSION};
    use crate::storage::migration_ladder::{MigrationStep, assert_contiguous};
    use rusqlite::Connection;
    use std::sync::OnceLock;

    fn unreachable_step(_: &rusqlite::Connection) -> anyhow::Result<()> {
        unreachable!("a version stamped Rebuild in the production registry has no step body")
    }

    #[test]
    fn the_production_registry_is_contiguous_from_the_first_migratable_version_to_the_current_one()
    {
        // `assert_contiguous` only ever reads the version half of each entry
        // (see its own body), so a placeholder step stands in for whichever
        // kind a real entry turns out to be. The first migratable version is
        // fixed at `INITIAL_SCHEMA_VERSION + 1` (18): the frozen initial
        // schema is version `INITIAL_SCHEMA_VERSION`, and every step after it
        // is a registry entry, so this anchor never moves even as
        // `CURRENT_SCHEMA_VERSION` does.
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

    fn register_sqlite_vec() {
        static INIT: OnceLock<()> = OnceLock::new();
        INIT.get_or_init(|| {
            #[allow(clippy::missing_transmute_annotations)]
            unsafe {
                rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                    sqlite_vec::sqlite3_vec_init as *const (),
                )));
            }
        });
    }

    fn v17_connection() -> Connection {
        register_sqlite_vec();
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(include_str!("../../migrations/index_001_initial.sql"))
            .expect("create v17 schema");
        conn
    }

    fn reextract_owed(conn: &Connection) -> bool {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM index_meta WHERE key = 'graph_edges_reextract')",
            [],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn add_target_file_column_marks_nothing_owed_on_an_empty_graph_edges_table() {
        let conn = v17_connection();

        add_target_file_column(&conn).expect("step must apply to an empty index");

        assert!(
            !reextract_owed(&conn),
            "a fresh climb through this step has no edges to backfill, so nothing is owed"
        );
        let target_file_is_null: bool = conn
            .query_row(
                "SELECT COUNT(*) = 0 FROM pragma_table_info('graph_edges') \
                 WHERE name = 'target_file' AND \"notnull\" = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(target_file_is_null, "target_file must be nullable");
    }

    #[test]
    fn add_target_file_column_marks_a_reextraction_owed_when_edges_already_exist() {
        let conn = v17_connection();
        conn.execute(
            "INSERT INTO graph_edges (source_file, source_name, target_name, kind, line) \
             VALUES ('src/lib.rs', 'caller', 'callee', 'calls', 2)",
            [],
        )
        .unwrap();

        add_target_file_column(&conn).expect("step must apply to a populated index");

        assert!(
            reextract_owed(&conn),
            "an existing edge has no target_file yet; the next `inkentry index` must re-extract it"
        );
        let target_file: Option<String> = conn
            .query_row(
                "SELECT target_file FROM graph_edges WHERE target_name = 'callee'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            target_file, None,
            "a migrated row must read as unresolved, not as a spurious resolution"
        );
    }
}
