//! `memory.db`'s forward migration ladder: steps 12..
//! [`super::MEMORY_SCHEMA_VERSION`], applied by the store-agnostic runner in
//! `storage::migration_ladder`. `create_schema` (`mod.rs`) is the only
//! caller.

use crate::storage::migration_ladder::MigrationStep;

/// The production ladder. Empty while [`super::MEMORY_SCHEMA_VERSION`] stays
/// at 11: the first real step arrives with whichever change
/// needs it. Add steps at the end, numbered for the version each one
/// produces; never renumber or reorder an existing entry.
pub(super) const MEMORY_MIGRATIONS: &[(i32, MigrationStep)] = &[];

#[cfg(test)]
mod tests {
    use super::super::MEMORY_SCHEMA_VERSION;
    use super::MEMORY_MIGRATIONS;
    use crate::storage::migration_ladder::assert_contiguous;

    #[test]
    fn the_production_ladder_is_contiguous_from_12_to_the_current_version() {
        assert_contiguous(MEMORY_MIGRATIONS, 12, MEMORY_SCHEMA_VERSION);
    }
}
