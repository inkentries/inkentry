// The properties the fresh initial schema is supposed to guarantee, pinned
// against the schema rather than against any one code path that writes to it.

use super::{MemoryStore, NoteId};
use std::str::FromStr;
use std::sync::OnceLock;
use uuid::Uuid;

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

fn store() -> (tempfile::TempDir, MemoryStore) {
    register_sqlite_vec();
    let dir = tempfile::tempdir().expect("tempdir");
    let store = MemoryStore::open(&dir.path().join("memory.db")).expect("open");
    (dir, store)
}

fn add(store: &MemoryStore, title: &str) -> NoteId {
    store
        .add_note("note", title, "body", &[], &[], None, None)
        .expect("add")
        .0
}

// ── foreign keys ─────────────────────────────────────────────────────────────

#[test]
fn foreign_key_enforcement_is_on_for_every_connection() {
    let (_dir, store) = store();
    let on: i64 = store
        .conn
        .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
        .expect("reading the pragma");
    assert_eq!(
        on, 1,
        "the store must declare foreign-key enforcement rather than inherit it \
         from the SQLite it happens to link against"
    );
}

#[test]
fn an_edge_naming_an_absent_entry_is_refused() {
    let (_dir, store) = store();
    let real = add(&store, "real");
    let ghost = NoteId::from_str("0199a0f1-4d3c-7c2a-9b1e-000000000000").unwrap();

    let err = store
        .add_edge(&real, &ghost, "relates_to")
        .expect_err("an edge to an entry that does not exist must not be storable");
    assert!(
        err.to_string().to_lowercase().contains("foreign key"),
        "expected a foreign-key violation, got: {err}"
    );
}

#[test]
fn a_supersede_link_to_an_absent_entry_is_refused() {
    let (_dir, store) = store();
    let real = add(&store, "real");
    let ghost = NoteId::from_str("0199a0f1-4d3c-7c2a-9b1e-000000000000").unwrap();

    assert!(
        store.set_superseded_by(&real, &ghost).is_err(),
        "superseded_by must not be able to name an entry that does not exist"
    );
}

#[test]
fn deleting_an_entry_cascades_to_its_edges() {
    let (_dir, store) = store();
    let a = add(&store, "a");
    let b = add(&store, "b");
    store.add_edge(&a, &b, "relates_to").expect("edge");

    store.delete_note(&b).expect("delete");

    let (outgoing, incoming) = store.get_edges(&a).expect("edges");
    assert!(
        outgoing.is_empty() && incoming.is_empty(),
        "the declared cascade should have taken the edge with the entry"
    );
}

// `note_embeddings` is a vec0 virtual table keyed on the rowid, so no foreign
// key can cascade into it. `AUTOINCREMENT` on `notes.id` is what stops a reused
// rowid handing a new entry the previous occupant's vector — a second line of
// defence that only matters once an orphan exists, so no orphan may be left.
#[test]
fn deleting_an_entry_takes_its_embedding_with_it() {
    let (_dir, store) = store();
    let keep = add(&store, "keep");
    let gone = add(&store, "gone");
    for (id, fill) in [(&keep, 0.25f32), (&gone, 0.5)] {
        store
            .insert_embedding(id, &crate::embeddings::vec_to_blob(&vec![fill; 896]))
            .expect("embed");
    }

    store.delete_note(&gone).expect("delete");

    let left: i64 = store
        .conn
        .query_row("SELECT count(*) FROM note_embeddings", [], |r| r.get(0))
        .expect("counting vectors");
    assert_eq!(
        left, 1,
        "the deleted entry's vector must go with it rather than survive as an orphan \
         keyed on a rowid no row holds"
    );
}

// ── identity ─────────────────────────────────────────────────────────────────

#[test]
fn the_identity_column_is_not_null_and_uniquely_indexed() {
    let (_dir, store) = store();

    let notnull: i64 = store
        .conn
        .query_row(
            "SELECT \"notnull\" FROM pragma_table_info('notes') WHERE name = 'uuid'",
            [],
            |r| r.get(0),
        )
        .expect("uuid column must exist");
    assert_eq!(notnull, 1, "uuid must be NOT NULL");

    let unique: i64 = store
        .conn
        .query_row(
            "SELECT \"unique\" FROM pragma_index_list('notes') WHERE name = 'idx_notes_uuid'",
            [],
            |r| r.get(0),
        )
        .expect("idx_notes_uuid must exist");
    assert_eq!(unique, 1, "idx_notes_uuid must be UNIQUE");
}

#[test]
fn a_stored_entry_is_identified_by_a_uuid_v7() {
    let (_dir, store) = store();
    let id = add(&store, "a");
    let parsed = Uuid::from_str(id.as_str()).expect("the id must be a UUID");
    assert_eq!(parsed.get_version_num(), 7);
    assert_eq!(parsed.get_variant(), uuid::Variant::RFC4122);
}

#[test]
fn an_entry_added_with_a_past_created_at_is_identified_from_that_time() {
    let (_dir, store) = store();
    let then = 1_000_000_000;
    let (id, _) = store
        .add_note_with_created_at("note", "old", "body", &[], &[], None, "active", then)
        .expect("add");

    let (secs, _) = Uuid::from_str(id.as_str())
        .unwrap()
        .get_timestamp()
        .unwrap()
        .to_unix();
    assert_eq!(
        secs as i64, then,
        "the identifier must be seeded from the entry's own creation time, not the wall clock"
    );
}

#[test]
fn a_back_catalogue_keeps_its_creation_order_in_its_identifiers() {
    let (_dir, store) = store();
    // Inserted newest-first, so anything derived from insertion order would
    // come out reversed.
    let mut ids: Vec<(i64, NoteId)> = vec![];
    for (i, at) in [3_000_000_000i64, 2_000_000_000, 1_000_000_000]
        .into_iter()
        .enumerate()
    {
        let (id, _) = store
            .add_note_with_created_at(
                "note",
                &format!("entry {i}"),
                "body",
                &[],
                &[],
                None,
                "active",
                at,
            )
            .expect("add");
        ids.push((at, id));
    }

    let mut by_id = ids.clone();
    by_id.sort_by(|a, b| a.1.cmp(&b.1));
    let mut by_time = ids;
    by_time.sort_by_key(|(at, _)| *at);
    assert_eq!(
        by_id, by_time,
        "identifiers must sort in creation order, not import order"
    );
}

#[test]
fn entries_sharing_a_creation_time_get_distinct_identifiers() {
    let (_dir, store) = store();
    let at = 1_700_000_000;
    let (a, _) = store
        .add_note_with_created_at("note", "a", "body", &[], &[], None, "active", at)
        .unwrap();
    let (b, _) = store
        .add_note_with_created_at("note", "b", "body", &[], &[], None, "active", at)
        .unwrap();
    assert_ne!(a, b);
}

#[test]
fn a_second_entry_cannot_claim_an_existing_identity() {
    let (_dir, store) = store();
    let taken = add(&store, "first");
    let err = store
        .conn
        .execute(
            "INSERT INTO notes (uuid, kind, title, body, entity_id) \
             VALUES (?1, 'note', 'second', 'body', 'some-other-hash')",
            rusqlite::params![taken.as_str()],
        )
        .expect_err("two entries must not share an identity");
    assert!(err.to_string().contains("UNIQUE"), "{err}");
}

// ── what the schema no longer needs ──────────────────────────────────────────

#[test]
fn the_convergence_key_is_unique_from_the_first_row() {
    let (_dir, store) = store();
    let unique: i64 = store
        .conn
        .query_row(
            "SELECT \"unique\" FROM pragma_index_list('notes') WHERE name = 'idx_notes_entity_id'",
            [],
            |r| r.get(0),
        )
        .expect("idx_notes_entity_id must exist");
    assert_eq!(
        unique, 1,
        "declared unique from creation, which is what removes the promotion step"
    );

    // Adding the same content twice reuses the first row rather than needing a
    // later dedupe pass to collapse it.
    let first = add(&store, "same");
    let second = add(&store, "same");
    assert_eq!(first, second);
}

#[test]
fn a_store_from_an_older_product_is_refused_rather_than_half_migrated() {
    register_sqlite_vec();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE notes (id INTEGER PRIMARY KEY, title TEXT)")
            .unwrap();
    }
    let err = match MemoryStore::open(&path) {
        Ok(_) => panic!("an unrecognised store must not be opened"),
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("inkentry import"),
        "the refusal should name the way across: {err}"
    );
    // A tool the message does not name is a tool nobody can search for, and
    // `spelunk-export` is published only on the old product's release page.
    assert!(
        err.to_string().contains("spelunk-export"),
        "the refusal must name the export tool rather than gesture at one: {err}"
    );
}

// A store from a *released* product carries a stamp, and `user_version` is one
// counter per file shared with every stamp the old ladder wrote. Restarting
// this build's numbering below those made every such store read as "from the
// future", so three shipped releases were told to upgrade — advice that can
// never work, on the one move a user makes once.
#[test]
fn a_store_stamped_by_a_released_binary_is_sent_to_export_and_import() {
    register_sqlite_vec();
    for stamp in 1..=super::LAST_LEGACY_SCHEMA_VERSION {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(&format!(
                "CREATE TABLE notes (id INTEGER PRIMARY KEY, title TEXT); \
                 PRAGMA user_version = {stamp};"
            ))
            .unwrap();
        }
        let err = match MemoryStore::open(&path) {
            Ok(_) => panic!("a stamp of {stamp} is an older product's store, not this build's"),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("older product")
                && err.contains("spelunk-export")
                && err.contains("inkentry import"),
            "stamp {stamp}: the refusal must name both ends of the crossing, got: {err}"
        );
        assert!(
            !err.contains("upgrade inkentry"),
            "stamp {stamp}: telling the user to upgrade is the opposite of the truth, \
             and no build exists that would open this: {err}"
        );
    }
}

#[test]
fn a_store_from_a_future_build_is_refused() {
    register_sqlite_vec();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    {
        let store = MemoryStore::open(&path).unwrap();
        store
            .conn
            .execute_batch(&format!(
                "PRAGMA user_version = {}",
                super::MEMORY_SCHEMA_VERSION + 1
            ))
            .unwrap();
    }
    let err = match MemoryStore::open(&path) {
        Ok(_) => panic!("a newer schema must not be opened"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("newer than this build"), "{err}");
}
