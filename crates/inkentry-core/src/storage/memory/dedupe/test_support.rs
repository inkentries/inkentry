// Shared test-only helpers for `dedupe`'s `tests` and `superseded_by_tests`
// submodules.

use super::{MemoryStore, NoteId};
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

// A real store with `idx_notes_entity_id` dropped, so these tests can seed
// the duplicate-content rows `dedupe_entity_ids` exists to collapse. The
// initial schema declares that index UNIQUE, so a store this binary created
// cannot hold such rows — but a hand-edited database can, and the collapse
// logic (particularly its `superseded_by` rewriting) is worth keeping covered.
// `dedupe_entity_ids` never reads the index: it groups by recomputing
// `note_entity_id` in Rust.
pub(super) fn open_store() -> MemoryStore {
    register_sqlite_vec();
    let conn = rusqlite::Connection::open(std::path::Path::new(":memory:"))
        .expect("open in-memory sqlite");
    let store = MemoryStore { conn };
    // Four tests below exist only to prove dedupe never leaves a live foreign-key
    // reference to a row it is about to delete; with enforcement off they pass
    // vacuously. Declared here for the same reason `MemoryStore::open` declares
    // it, rather than inherited from the bundled SQLite's compile flags.
    store
        .conn
        .execute_batch("PRAGMA foreign_keys = ON")
        .expect("foreign-key enforcement");
    store.create_schema().expect("schema creation");
    store
        .conn
        .execute_batch("DROP INDEX idx_notes_entity_id")
        .expect("dropping the entity_id unique index");
    store
}

pub(super) fn note_count(store: &MemoryStore) -> i64 {
    store
        .conn
        .query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))
        .unwrap()
}

pub(super) fn has_embedding(store: &MemoryStore, note_id: &NoteId) -> bool {
    store.get_embedding(note_id).unwrap().is_some()
}

// Snapshot every column of every row in `table`, ordered by `order_by`,
// as generic SQLite `Value`s. Used to assert a rolled-back or dry-run
// call left the database byte-for-byte unchanged: unlike a row-count or
// single-column check, this catches a regression in *any* column
// (tags, superseded_by, status, entity_id, uuid, remote_id, ...) without
// having to hand-maintain a column list.
fn full_table_snapshot(
    store: &MemoryStore,
    table: &str,
    order_by: &str,
) -> Vec<Vec<rusqlite::types::Value>> {
    let sql = format!("SELECT * FROM {table} ORDER BY {order_by}");
    let mut stmt = store.conn.prepare(&sql).unwrap();
    let n = stmt.column_count();
    stmt.query_map([], |row| {
        (0..n)
            .map(|i| row.get::<_, rusqlite::types::Value>(i))
            .collect()
    })
    .unwrap()
    .collect::<rusqlite::Result<Vec<_>>>()
    .unwrap()
}

pub(super) type TableSnapshot = Vec<Vec<rusqlite::types::Value>>;

// Snapshot of `notes` + `memory_edges` + `note_embeddings`, the three
// tables `dedupe_entity_ids` can touch.
pub(super) fn full_db_snapshot(
    store: &MemoryStore,
) -> (TableSnapshot, TableSnapshot, TableSnapshot) {
    (
        full_table_snapshot(store, "notes", "id"),
        full_table_snapshot(store, "memory_edges", "from_id, to_id, kind"),
        full_table_snapshot(store, "note_embeddings", "note_id"),
    )
}

// Expected `Note::superseded_by` for a store-minted id.
pub(super) fn sup(id: &NoteId) -> Option<NoteId> {
    Some(id.clone())
}
