//! The persisted OID markers that gate the git-notes import on read.
//!
//! A read compares the live notes-ref OIDs against these values and merges or
//! imports only when one moved. The working-ref OID is written in the same
//! transaction as the imported rows, so a crash cannot leave the two disagreeing.

use anyhow::{Context, Result};
use rusqlite::OptionalExtension;

use super::MemoryStore;

/// The OIDs recorded by the last git-notes merge and import.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NotesImportMarker {
    /// OID of `refs/notes/origin/inkentry` at the last merge; `None` if never merged.
    pub last_merged_tracking_oid: Option<String>,
    /// OID of `refs/notes/inkentry` at the last import; `None` if never imported.
    pub last_imported_working_oid: Option<String>,
}

impl MemoryStore {
    /// Reads the persisted markers; a store with none yet returns the default
    /// (both `None`).
    pub fn notes_import_state(&self) -> Result<NotesImportMarker> {
        self.conn
            .query_row(
                "SELECT last_merged_tracking_oid, last_imported_working_oid \
                 FROM notes_import_state WHERE id = 0",
                [],
                |r| {
                    Ok(NotesImportMarker {
                        last_merged_tracking_oid: r.get(0)?,
                        last_imported_working_oid: r.get(1)?,
                    })
                },
            )
            .optional()
            .context("reading notes_import_state")
            .map(Option::unwrap_or_default)
    }

    /// Records the tracking-ref OID observed at the last merge, leaving the
    /// working-ref marker unchanged.
    pub fn set_notes_merged_tracking_oid(&self, oid: Option<&str>) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO notes_import_state (id, last_merged_tracking_oid) \
                 VALUES (0, ?1) \
                 ON CONFLICT(id) DO UPDATE SET \
                     last_merged_tracking_oid = excluded.last_merged_tracking_oid",
                rusqlite::params![oid],
            )
            .context("recording last_merged_tracking_oid")?;
        Ok(())
    }

    /// Records the working-ref OID imported into this store, leaving the
    /// tracking-ref marker unchanged. Call it inside the import transaction so
    /// the marker and the imported rows commit together.
    pub fn set_notes_imported_working_oid(&self, oid: Option<&str>) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO notes_import_state (id, last_imported_working_oid) \
                 VALUES (0, ?1) \
                 ON CONFLICT(id) DO UPDATE SET \
                     last_imported_working_oid = excluded.last_imported_working_oid",
                rusqlite::params![oid],
            )
            .context("recording last_imported_working_oid")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

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

    fn open_store() -> (tempfile::TempDir, MemoryStore) {
        register_sqlite_vec();
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let store = MemoryStore::open(&tmp.path().join("memory.db")).expect("open");
        (tmp, store)
    }

    #[test]
    fn absent_marker_reads_as_default() {
        let (_tmp, store) = open_store();
        assert_eq!(
            store.notes_import_state().unwrap(),
            NotesImportMarker::default()
        );
    }

    #[test]
    fn each_oid_updates_independently_and_preserves_the_other() {
        let (_tmp, store) = open_store();
        store.set_notes_imported_working_oid(Some("w1")).unwrap();
        store.set_notes_merged_tracking_oid(Some("t1")).unwrap();

        let marker = store.notes_import_state().unwrap();
        assert_eq!(marker.last_imported_working_oid.as_deref(), Some("w1"));
        assert_eq!(marker.last_merged_tracking_oid.as_deref(), Some("t1"));

        store.set_notes_imported_working_oid(Some("w2")).unwrap();
        let marker = store.notes_import_state().unwrap();
        assert_eq!(marker.last_imported_working_oid.as_deref(), Some("w2"));
        assert_eq!(
            marker.last_merged_tracking_oid.as_deref(),
            Some("t1"),
            "updating the working OID must preserve the tracking OID"
        );
    }

    #[test]
    fn marker_survives_reopen() {
        register_sqlite_vec();
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("memory.db");
        {
            let store = MemoryStore::open(&path).expect("open");
            store
                .set_notes_imported_working_oid(Some("abc123"))
                .unwrap();
        }
        let reopened = MemoryStore::open(&path).expect("reopen");
        assert_eq!(
            reopened
                .notes_import_state()
                .unwrap()
                .last_imported_working_oid
                .as_deref(),
            Some("abc123"),
            "the marker must persist across a store reopen"
        );
    }
}
