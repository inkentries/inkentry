//! `memory.db`'s forward migration ladder: steps 12..
//! [`super::MEMORY_SCHEMA_VERSION`], applied by the store-agnostic runner in
//! `storage::migration_ladder`. `create_schema` (`mod.rs`) is the only
//! caller, and it runs the ladder for a fresh store too, from the frozen
//! `memory_001_initial.sql` at version 11.

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

use crate::storage::migration_ladder::MigrationStep;

use super::notes::split_csv;
use super::tags::normalize_tag;

/// The production ladder. Add steps at the end, numbered for the version
/// each one produces; never renumber or reorder an existing entry.
pub(super) const MEMORY_MIGRATIONS: &[(i32, MigrationStep)] = &[(12, add_note_tags_and_files)];

/// ADR-101 step 12. The DDL is `migrations/memory_012.sql` (new tables,
/// `memory_fts` rebuilt to be fed from `note_tags`); this function runs it,
/// copies the old columns into the new tables, then runs
/// `memory_012_drop_legacy_columns.sql`. The copy is Rust rather than SQL
/// because tag normalisation is Unicode NFC.
///
/// Tags are normalised exactly as a fresh write would (D2): NFC, lowercase,
/// trim, runs of whitespace/underscore to `-`; an empty result is dropped,
/// and duplicates collapse via `INSERT OR IGNORE`.
///
/// Paths cannot be normalised the same way. A migration step is `fn(&Connection)
/// -> Result<()>` — pure SQL/Rust with no project root and no git to check
/// against — so a legacy path only gets a superficial cleanup (a leading
/// `./` stripped, backslashes to `/`) and is otherwise stored as-is,
/// including one that a live write would refuse as escaping the root: this
/// step must not fail the whole migration over data it cannot validate.
/// Every migrated row is stamped `state = 'untracked'`, `checked_at = 0`.
/// Neither is knowable here; `0` is a sentinel distinguishable from a real
/// check time, left for a live write's re-resolution or a future
/// `inkentry index` re-check to correct (see ADR-101 D3/D5; that re-check is
/// not implemented by this migration).
fn add_note_tags_and_files(conn: &Connection) -> Result<()> {
    conn.execute_batch(include_str!("../../../migrations/memory_012.sql"))
        .context("applying memory_012.sql")?;

    if fault_due() {
        anyhow::bail!("injected test fault after creating note_tags/note_files");
    }

    let rows: Vec<(String, Option<String>, Option<String>)> = conn
        .prepare("SELECT uuid, tags, linked_files FROM notes")
        .context("preparing legacy tags/linked_files read")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .context("reading legacy tags/linked_files")?
        .collect::<rusqlite::Result<_>>()
        .context("collecting legacy tags/linked_files")?;

    {
        let mut insert_tag =
            conn.prepare("INSERT OR IGNORE INTO note_tags (note_uuid, tag) VALUES (?1, ?2)")?;
        let mut insert_file = conn.prepare(
            "INSERT OR IGNORE INTO note_files (note_uuid, path, state, checked_at) \
             VALUES (?1, ?2, 'untracked', 0)",
        )?;
        for (uuid, tags, files) in &rows {
            for raw in split_csv(tags.as_deref()) {
                if let Some(tag) = normalize_tag(&raw) {
                    insert_tag.execute(params![uuid, tag])?;
                }
            }
            for raw in split_csv(files.as_deref()) {
                let cleaned = migration_clean_path(&raw);
                if !cleaned.is_empty() {
                    insert_file.execute(params![uuid, cleaned])?;
                }
            }
        }
    }

    conn.execute_batch(include_str!(
        "../../../migrations/memory_012_drop_legacy_columns.sql"
    ))
    .context("applying memory_012_drop_legacy_columns.sql")?;

    Ok(())
}

/// Best-effort cleanup with no project root to validate against (see the
/// step's own doc comment): a leading `./` is stripped and backslashes
/// become forward slashes. Nothing else is checked, so an absolute or
/// out-of-root path from an old store is carried across unchanged rather
/// than failing the migration.
fn migration_clean_path(raw: &str) -> String {
    let slashed = raw.trim().replace('\\', "/");
    slashed.strip_prefix("./").unwrap_or(&slashed).to_string()
}

#[cfg(test)]
thread_local! {
    // Set by a test to make the step fail after creating `note_tags`/
    // `note_files` but before anything else, so the ladder's own
    // `BEGIN IMMEDIATE`/`ROLLBACK` around each step is exercised against this
    // step's real body rather than a synthetic one.
    static FAIL_AFTER_TABLES: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(super) fn inject_failure_after_creating_tables() {
    FAIL_AFTER_TABLES.with(|f| f.set(true));
}

#[cfg(not(test))]
fn fault_due() -> bool {
    false
}
#[cfg(test)]
fn fault_due() -> bool {
    FAIL_AFTER_TABLES.with(|f| f.get())
}

#[cfg(test)]
mod tests {
    use super::super::MEMORY_SCHEMA_VERSION;
    use super::MEMORY_MIGRATIONS;
    use crate::storage::migration_ladder::assert_contiguous;

    #[test]
    fn the_production_ladder_is_contiguous_from_12_to_the_current_version() {
        assert_contiguous(MEMORY_MIGRATIONS, 12, MEMORY_SCHEMA_VERSION);
    }

    #[test]
    fn migration_clean_path_strips_leading_dot_slash_and_normalises_slashes() {
        use super::migration_clean_path;
        assert_eq!(migration_clean_path("./src/lib.rs"), "src/lib.rs");
        assert_eq!(migration_clean_path("src\\lib.rs"), "src/lib.rs");
        assert_eq!(migration_clean_path("src/lib.rs"), "src/lib.rs");
        // Left as-is: no root to validate against here.
        assert_eq!(migration_clean_path("/etc/passwd"), "/etc/passwd");
        assert_eq!(migration_clean_path("../outside.rs"), "../outside.rs");
    }
}
