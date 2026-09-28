use anyhow::{Context, Result};
use rusqlite::{Connection, params};

use crate::storage::migration_ladder::MigrationStep;

use super::notes::split_csv;
use super::tags::normalize_tag;

// Append only: number each step for the version it produces, and never
// renumber or reorder an existing one.
pub(super) const MEMORY_MIGRATIONS: &[(i32, MigrationStep)] = &[
    (12, add_note_tags_and_files),
    (13, add_events_and_origin),
    (14, add_pending_anchors),
];

// The copy is Rust rather than SQL because tag normalisation is Unicode NFC.
//
// A step has no project root or git to validate paths against, so legacy paths
// get only `migration_clean_path` and are stored even when a live write would
// refuse them: failing the migration over data it cannot validate is worse.
// Migrated files are stamped `untracked` with `checked_at = 0`, a sentinel
// distinguishable from a real check time.
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

// Step 13 has no data pass: both additions are pure DDL.
fn add_events_and_origin(conn: &Connection) -> Result<()> {
    conn.execute_batch(include_str!("../../../migrations/memory_013.sql"))
        .context("applying memory_013.sql")
}

// Step 14 has no data pass either: both tables start empty. A pre-existing
// store never had a way to record a pending anchor, so there is nothing to
// backfill.
fn add_pending_anchors(conn: &Connection) -> Result<()> {
    conn.execute_batch(include_str!("../../../migrations/memory_014.sql"))
        .context("applying memory_014.sql")
}

fn migration_clean_path(raw: &str) -> String {
    let slashed = raw.trim().replace('\\', "/");
    slashed.strip_prefix("./").unwrap_or(&slashed).to_string()
}

#[cfg(test)]
thread_local! {
    // Lets a test fail the step after the tables exist, to exercise the
    // ladder's rollback against the real step body.
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
