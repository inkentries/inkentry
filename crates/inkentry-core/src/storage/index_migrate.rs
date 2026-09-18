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

use crate::storage::migration_ladder::MigrationStep;

/// What a registered version does to get the store there from the version
/// immediately below.
// `INDEX_MIGRATIONS` is empty, so neither variant is constructed outside the
// tests below yet; both are matched on unconditionally in `db.rs`.
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

/// The production registry. Empty while [`super::db::CURRENT_SCHEMA_VERSION`]
/// stays at 17: the first real step arrives with whichever change needs it.
/// Add entries at the end, numbered for the version each one produces; never
/// renumber or reorder an existing entry.
pub(super) const INDEX_MIGRATIONS: &[(i32, IndexMigrationKind)] = &[];

#[cfg(test)]
mod tests {
    use super::INDEX_MIGRATIONS;
    use crate::storage::db::CURRENT_SCHEMA_VERSION;
    use crate::storage::migration_ladder::{MigrationStep, assert_contiguous};

    fn unreachable_step(_: &rusqlite::Connection) -> anyhow::Result<()> {
        unreachable!("a version stamped Rebuild in the production registry has no step body")
    }

    #[test]
    fn the_production_registry_is_contiguous_from_the_first_migratable_version_to_the_current_one()
    {
        // `assert_contiguous` only ever reads the version half of each entry
        // (see its own body), so a placeholder step stands in for whichever
        // kind a real entry turns out to be.
        let versions: Vec<(i32, MigrationStep)> = INDEX_MIGRATIONS
            .iter()
            .map(|&(v, _)| (v, unreachable_step as MigrationStep))
            .collect();
        assert_contiguous(
            &versions,
            CURRENT_SCHEMA_VERSION + 1,
            CURRENT_SCHEMA_VERSION,
        );
    }
}
