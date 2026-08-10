use super::{MemoryStore, NoteId};
use std::sync::OnceLock;

// Register the sqlite-vec extension exactly once per test process. The schema
// creates a `vec0` virtual table, which requires the extension to be loaded
// before any connection is opened.
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

fn open_store() -> MemoryStore {
    register_sqlite_vec();
    MemoryStore::open(std::path::Path::new(":memory:"))
        .expect("failed to open in-memory MemoryStore")
}

fn count_edges(store: &MemoryStore, from_id: &NoteId, to_id: &NoteId, kind: &str) -> i64 {
    store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM memory_edges WHERE from_id = ?1 AND to_id = ?2 AND kind = ?3",
            rusqlite::params![from_id.as_str(), to_id.as_str(), kind],
            |r| r.get(0),
        )
        .unwrap_or(0)
}

// Expected `Note::superseded_by` for a store-minted id.
fn sup(id: &NoteId) -> Option<NoteId> {
    Some(id.clone())
}

// An id that parses but names no row in any store.
fn absent_id() -> NoteId {
    "0199a0f1-4d3c-7c2a-9b1e-6f0a2c5d8e33".parse().unwrap()
}

// ── supersede() ──────────────────────────────────────────────────────────────

#[test]
fn supersede_happy_path() {
    let store = open_store();

    let (old_id, _) = store
        .add_note("decision", "Old decision", "old body", &[], &[], None, None)
        .unwrap();
    let (new_id, _) = store
        .add_note("decision", "New decision", "new body", &[], &[], None, None)
        .unwrap();

    let changed = store.supersede(&old_id, &new_id).unwrap();
    assert!(changed, "supersede() should return true on first call");

    // (a) old note must be archived with superseded_by set
    let old_note = store.get(&old_id).unwrap().expect("old note must exist");
    assert_eq!(old_note.status, "archived");
    assert_eq!(old_note.superseded_by, sup(&new_id));

    // (b) a memory_edges row must exist linking new → old
    assert_eq!(
        count_edges(&store, &new_id, &old_id, "supersedes"),
        1,
        "expected exactly one supersedes edge"
    );
}

#[test]
fn supersede_idempotent() {
    let store = open_store();

    let (old_id, _) = store
        .add_note("note", "Alpha", "body", &[], &[], None, None)
        .unwrap();
    let (new_id, _) = store
        .add_note("note", "Beta", "body", &[], &[], None, None)
        .unwrap();

    let first = store.supersede(&old_id, &new_id).unwrap();
    assert!(first);

    // Second call on an already-archived note must return false
    let second = store.supersede(&old_id, &new_id).unwrap();
    assert!(
        !second,
        "supersede() should return false when note is already archived"
    );

    // Must not have inserted a duplicate edge
    assert_eq!(
        count_edges(&store, &new_id, &old_id, "supersedes"),
        1,
        "duplicate supersedes edge must not be inserted"
    );
}

// ── add_note_superseding() ──────────────────────────────────────────────────

#[test]
fn add_note_superseding_happy_path_archives_old_and_links_new() {
    let store = open_store();

    let (old_id, _) = store
        .add_note("decision", "Old decision", "old body", &[], &[], None, None)
        .unwrap();

    let (new_id, created) = store
        .add_note_superseding(
            "decision",
            "New decision",
            "new body",
            &[],
            &[],
            None,
            &old_id,
        )
        .unwrap();
    assert!(
        created,
        "a fresh supersede insert must report created = true"
    );

    let old_note = store.get(&old_id).unwrap().expect("old note must exist");
    assert_eq!(old_note.status, "archived");
    assert_eq!(old_note.superseded_by, sup(&new_id));

    assert_eq!(
        count_edges(&store, &new_id, &old_id, "supersedes"),
        1,
        "expected exactly one supersedes edge"
    );
}

// ADR-068 amendment E4: re-superseding an already-archived OLD (via a second
// `add_note_superseding` call naming a different successor) must reject with
// an error and roll back the whole transaction — no orphaned new note, no
// second supersedes edge, OLD's existing successor link untouched.
#[test]
fn add_note_superseding_rejects_already_archived_old_and_writes_nothing() {
    let store = open_store();

    let (old_id, _) = store
        .add_note("decision", "Old decision", "old body", &[], &[], None, None)
        .unwrap();
    let (successor_a, _) = store
        .add_note_superseding("decision", "Successor A", "body a", &[], &[], None, &old_id)
        .unwrap();

    let count_before = store.count().unwrap();

    let result =
        store.add_note_superseding("decision", "Successor B", "body b", &[], &[], None, &old_id);
    assert!(
        result.is_err(),
        "re-superseding an already-archived OLD must error, not silently succeed"
    );

    assert_eq!(
        store.count().unwrap(),
        count_before,
        "a rejected supersede must not leave an orphaned new note row"
    );

    let old_note = store.get(&old_id).unwrap().expect("old note must exist");
    assert_eq!(
        old_note.superseded_by,
        sup(&successor_a),
        "OLD's successor link must still point at the first, not the rejected second, successor"
    );

    assert_eq!(
        count_edges(&store, &successor_a, &old_id, "supersedes"),
        1,
        "the original supersedes edge must be untouched"
    );
}

// Superseding a nonexistent OLD id must also error, not silently create an
// unlinked new note (the archive-`OLD` `UPDATE` matches zero rows either way).
#[test]
fn add_note_superseding_rejects_nonexistent_old() {
    let store = open_store();
    let count_before = store.count().unwrap();

    let result =
        store.add_note_superseding("decision", "New", "new body", &[], &[], None, &absent_id());
    assert!(
        result.is_err(),
        "superseding a nonexistent OLD id must error"
    );
    assert_eq!(
        store.count().unwrap(),
        count_before,
        "no note must be created when OLD does not exist"
    );
}

// ── add_edge() ───────────────────────────────────────────────────────────────

#[test]
fn add_edge_valid_kinds_accepted() {
    let store = open_store();
    let (a, _) = store
        .add_note("note", "A", "", &[], &[], None, None)
        .unwrap();
    let (b, _) = store
        .add_note("note", "B", "", &[], &[], None, None)
        .unwrap();

    for kind in ["supersedes", "relates_to", "contradicts"] {
        store
            .add_edge(&a, &b, kind)
            .unwrap_or_else(|e| panic!("add_edge with kind '{kind}' failed: {e}"));
    }
}

#[test]
fn add_edge_invalid_kind_returns_err() {
    let store = open_store();
    let (a, _) = store
        .add_note("note", "A", "", &[], &[], None, None)
        .unwrap();
    let (b, _) = store
        .add_note("note", "B", "", &[], &[], None, None)
        .unwrap();

    let err = store
        .add_edge(&a, &b, "invented")
        .expect_err("add_edge with invalid kind must return Err");
    assert!(
        err.to_string().contains("invented"),
        "error message must mention the invalid kind; got: {err}"
    );
}

#[test]
fn add_edge_duplicate_silently_ignored() {
    let store = open_store();
    let (a, _) = store
        .add_note("note", "A", "", &[], &[], None, None)
        .unwrap();
    let (b, _) = store
        .add_note("note", "B", "", &[], &[], None, None)
        .unwrap();

    store.add_edge(&a, &b, "relates_to").unwrap();
    store.add_edge(&a, &b, "relates_to").unwrap(); // second call must not error

    assert_eq!(
        count_edges(&store, &a, &b, "relates_to"),
        1,
        "duplicate edge must not produce a second row"
    );
}

// ── relates_to_edges_for_sync(): only fully-synced relates_to edges ───────────

// The edge push enumerates only `relates_to` edges whose BOTH endpoints carry
// a `remote_id`, so the cloud knows them by external_id. An edge with an
// unsynced endpoint is withheld until a later sync lands it; the two other
// edge kinds are never enumerated (supersedes rides its entry, and contradicts
// is server-derived).
#[test]
fn relates_to_edges_for_sync_requires_both_endpoints_synced() {
    let store = open_store();
    let (a, _) = store
        .add_note("note", "A", "", &[], &[], None, None)
        .unwrap();
    let (b, _) = store
        .add_note("note", "B", "", &[], &[], None, None)
        .unwrap();
    store.add_edge(&b, &a, "relates_to").unwrap();

    // Neither endpoint synced yet: nothing to push.
    assert!(store.relates_to_edges_for_sync().unwrap().is_empty());

    // Only one endpoint synced: still withheld.
    store
        .set_remote_id(&a, "01890000-0000-7000-8000-0000000000a1")
        .unwrap();
    assert!(
        store.relates_to_edges_for_sync().unwrap().is_empty(),
        "an edge with one unsynced endpoint must not be pushable"
    );

    // Both synced: the edge is now pushable, keyed by each endpoint's id.
    store
        .set_remote_id(&b, "01890000-0000-7000-8000-0000000000b2")
        .unwrap();
    let edges = store.relates_to_edges_for_sync().unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].from_id, b);
    assert_eq!(edges[0].to_id, a);
}

#[test]
fn relates_to_edges_for_sync_ignores_supersedes_and_contradicts() {
    let store = open_store();
    let (a, _) = store
        .add_note("note", "A", "", &[], &[], None, None)
        .unwrap();
    let (b, _) = store
        .add_note("note", "B", "", &[], &[], None, None)
        .unwrap();
    store
        .set_remote_id(&a, "01890000-0000-7000-8000-0000000000a1")
        .unwrap();
    store
        .set_remote_id(&b, "01890000-0000-7000-8000-0000000000b2")
        .unwrap();
    store.add_edge(&b, &a, "supersedes").unwrap();
    store.add_edge(&b, &a, "contradicts").unwrap();

    assert!(
        store.relates_to_edges_for_sync().unwrap().is_empty(),
        "only relates_to edges are enumerated for push"
    );
}

// ── identity + cursor + idempotent apply ─────────────────────────────────────

// The identity is minted at insert, never backfilled later: `add_note` hands
// back the entry's UUIDv7 and reading the row back yields the same id.
#[test]
fn add_note_mints_a_uuid_identity_at_insert() {
    let store = open_store();
    let (id, _) = store
        .add_note("decision", "D", "body", &[], &[], None, None)
        .unwrap();

    assert_eq!(id.as_str().len(), 36);
    let parsed = uuid::Uuid::parse_str(id.as_str()).expect("the id must be a UUID");
    assert_eq!(parsed.get_version_num(), 7);

    assert_eq!(
        store.get(&id).unwrap().expect("note must exist").id,
        id,
        "the id a read hands back must be the one the insert returned"
    );
}

#[test]
fn rows_for_sync_carries_the_identity_and_is_text_only() {
    let store = open_store();
    store
        .add_note("decision", "One", "first", &[], &[], None, None)
        .unwrap();
    store
        .add_note("note", "Two", "second", &[], &[], None, None)
        .unwrap();

    let rows = store.rows_for_sync(false).unwrap();
    assert_eq!(rows.len(), 2);
    // Every row carries its UUIDv7 identity; SyncRow has no embedding field at
    // all (text-only by construction).
    for r in &rows {
        assert_eq!(r.id.as_str().len(), 36);
        assert!(r.remote_id.is_none());
    }
    // Ordered oldest-first so supersede targets precede referrers.
    assert_eq!(rows[0].title, "One");
    assert_eq!(rows[1].title, "Two");
}

#[test]
fn apply_remote_note_is_idempotent_no_dupes() {
    let store = open_store();
    let remote_id = "01890000-0000-7000-8000-000000000001";

    let inserted = store
        .apply_remote_note(
            remote_id,
            "decision",
            "Remote",
            "body",
            None,
            1_700_000_000,
            false,
        )
        .unwrap();
    assert!(inserted, "first apply inserts");

    // Re-applying the same remote_id must NOT create a duplicate.
    let inserted2 = store
        .apply_remote_note(
            remote_id,
            "decision",
            "Remote",
            "body",
            None,
            1_700_000_000,
            false,
        )
        .unwrap();
    assert!(!inserted2, "second apply is a no-op");

    let n: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM notes WHERE remote_id = ?1",
            rusqlite::params![remote_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "exactly one local row for the remote id");
}

#[test]
fn apply_remote_note_tombstone_archives_existing() {
    let store = open_store();
    let remote_id = "01890000-0000-7000-8000-000000000002";

    store
        .apply_remote_note(remote_id, "note", "T", "b", None, 1_700_000_000, false)
        .unwrap();
    let local_id = store.note_id_for_remote_id(remote_id).unwrap().unwrap();
    assert_eq!(store.get(&local_id).unwrap().unwrap().status, "active");

    // A pulled tombstone archives the local copy (never un-archives).
    let inserted = store
        .apply_remote_note(remote_id, "note", "T", "b", None, 1_700_000_000, true)
        .unwrap();
    assert!(!inserted);
    assert_eq!(store.get(&local_id).unwrap().unwrap().status, "archived");
}

// ── apply_remote_note: entity_id + collision recovery ──────────────────────
// `idx_notes_entity_id` is UNIQUE from creation, so every test below runs
// against a store where a duplicate `{kind,title,body}` insert collides.

#[test]
fn apply_remote_note_sets_entity_id_on_fresh_insert() {
    let store = open_store();
    let remote_id = "01890000-0000-7000-8000-000000000010";

    let inserted = store
        .apply_remote_note(
            remote_id,
            "decision",
            "Fresh",
            "body",
            None,
            1_700_000_000,
            false,
        )
        .unwrap();
    assert!(inserted, "criterion 1: no collision, fresh row inserts");

    let local_id = store.note_id_for_remote_id(remote_id).unwrap().unwrap();
    let stored_eid: Option<String> = store
        .conn
        .query_row(
            "SELECT entity_id FROM notes WHERE uuid = ?1",
            rusqlite::params![local_id.as_str()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        stored_eid,
        Some(crate::storage::entity_id::entity_id(
            "decision", "Fresh", "body"
        )),
        "criterion 1: entity_id must be populated at insert time"
    );
}

#[test]
fn apply_remote_note_recovers_from_collision_and_adopts_remote_id() {
    let store = open_store();
    let (existing_id, _) = store
        .add_note(
            "decision",
            "dup entry",
            "same content",
            &[],
            &[],
            None,
            None,
        )
        .unwrap();

    let remote_id = "01890000-0000-7000-8000-000000000011";
    let inserted = store
        .apply_remote_note(
            remote_id,
            "decision",
            "dup entry",
            "same content",
            None,
            1_700_000_000,
            false,
        )
        .unwrap();
    assert!(
        !inserted,
        "criterion 3: a colliding pull must report false, not a fresh insert"
    );
    assert_eq!(
        store.count().unwrap(),
        1,
        "criterion 3: the collision must not create a second row"
    );
    assert_eq!(
        store.note_id_for_remote_id(remote_id).unwrap(),
        Some(existing_id),
        "criterion 3: the existing row must adopt the pulled remote_id"
    );
}

#[test]
fn apply_remote_note_collision_with_existing_remote_id_leaves_it_unchanged() {
    let store = open_store();
    let (existing_id, _) = store
        .add_note(
            "decision",
            "dup entry",
            "same content",
            &[],
            &[],
            None,
            None,
        )
        .unwrap();
    let own_remote_id = "01890000-0000-7000-8000-000000000012";
    store.set_remote_id(&existing_id, own_remote_id).unwrap();

    let pulled_remote_id = "01890000-0000-7000-8000-000000000013";
    let inserted = store
        .apply_remote_note(
            pulled_remote_id,
            "decision",
            "dup entry",
            "same content",
            None,
            1_700_000_000,
            false,
        )
        .unwrap();
    assert!(
        !inserted,
        "criterion 4: still a collision, not a fresh insert"
    );
    assert_eq!(
        store.count().unwrap(),
        1,
        "criterion 4: no second row from the collision"
    );
    assert_eq!(
        store.note_id_for_remote_id(own_remote_id).unwrap(),
        Some(existing_id),
        "criterion 4: the row's own remote_id must be left untouched"
    );
    assert_eq!(
        store.note_id_for_remote_id(pulled_remote_id).unwrap(),
        None,
        "criterion 4: the pulled remote_id must not be stored anywhere locally"
    );
}

#[test]
fn apply_remote_note_collision_and_archived_pull_archives_existing_row() {
    let store = open_store();
    let (existing_id, _) = store
        .add_note(
            "decision",
            "dup entry",
            "same content",
            &[],
            &[],
            None,
            None,
        )
        .unwrap();
    assert_eq!(store.get(&existing_id).unwrap().unwrap().status, "active");

    let remote_id = "01890000-0000-7000-8000-000000000014";
    store
        .apply_remote_note(
            remote_id,
            "decision",
            "dup entry",
            "same content",
            None,
            1_700_000_000,
            true,
        )
        .unwrap();

    assert_eq!(
        store.get(&existing_id).unwrap().unwrap().status,
        "archived",
        "criterion 5: an archived pull must archive the reused existing row"
    );
}

#[test]
fn apply_remote_note_collision_non_archived_pull_does_not_unarchive_existing() {
    let store = open_store();
    let (existing_id, _) = store
        .add_note_with_created_at(
            "decision",
            "dup entry",
            "same content",
            &[],
            &[],
            None,
            "archived",
            1_700_000_000,
        )
        .unwrap();
    assert_eq!(store.get(&existing_id).unwrap().unwrap().status, "archived");

    let remote_id = "01890000-0000-7000-8000-000000000015";
    store
        .apply_remote_note(
            remote_id,
            "decision",
            "dup entry",
            "same content",
            None,
            1_700_000_000,
            false,
        )
        .unwrap();

    // Without these two, the assertion below passes trivially even when the
    // pulled note lands as a distinct second row (never touching existing_id
    // at all), so it would not actually catch a collision-recovery regression.
    let total_rows: i64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        total_rows, 1,
        "criterion 6: must be a collision recovery, not a second distinct row"
    );
    assert_eq!(
        store.note_id_for_remote_id(remote_id).unwrap(),
        Some(existing_id.clone()),
        "criterion 6: the pulled remote_id must be adopted onto the existing row"
    );
    assert_eq!(
        store.get(&existing_id).unwrap().unwrap().status,
        "archived",
        "criterion 6: a non-archived pull must never revert an archived row to active"
    );
}

#[test]
fn apply_remote_note_other_insert_error_propagates_and_rolls_back() {
    let store = open_store();
    store
        .conn
        .execute_batch(
            "CREATE TRIGGER reject_specific_title
             BEFORE INSERT ON notes
             WHEN NEW.title = 'trigger-reject'
             BEGIN SELECT RAISE(ABORT, 'synthetic non-unique failure'); END;",
        )
        .unwrap();

    let result = store.apply_remote_note(
        "01890000-0000-7000-8000-000000000017",
        "note",
        "trigger-reject",
        "body",
        None,
        1_700_000_000,
        false,
    );
    assert!(
        result.is_err(),
        "criterion 9: a non-UNIQUE error must propagate, not be swallowed"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("synthetic non-unique failure"),
        "expected the synthetic trigger error to propagate verbatim, got: {msg}"
    );
    assert_eq!(
        store.count().unwrap(),
        0,
        "criterion 7: the failed transaction must roll back, no orphaned row left behind"
    );
}

// The existing rollback test above only forces a failure at the INSERT
// itself, which every prior insert path already rolled back on trivially
// (a single failed statement leaves nothing behind, transaction or not).
// This forces the failure one step later, inside set_remote_id's UPDATE
// after collision recovery already succeeded, to prove the BEGIN/COMMIT
// wrapping is doing real work for criterion 7's "partway through" case.
#[test]
fn apply_remote_note_failure_after_collision_recovery_rolls_back() {
    let store = open_store();
    let (existing_id, _) = store
        .add_note(
            "decision",
            "dup entry",
            "same content",
            &[],
            &[],
            None,
            None,
        )
        .unwrap();

    store
        .conn
        .execute_batch(
            "CREATE TRIGGER reject_remote_id_update
             BEFORE UPDATE OF remote_id ON notes
             BEGIN SELECT RAISE(ABORT, 'synthetic post-recovery failure'); END;",
        )
        .unwrap();

    let remote_id = "01890000-0000-7000-8000-000000000099";
    let result = store.apply_remote_note(
        remote_id,
        "decision",
        "dup entry",
        "same content",
        None,
        1_700_000_000,
        false,
    );
    assert!(
        result.is_err(),
        "criterion 7: a failure in set_remote_id after recovery must propagate: {result:?}"
    );
    assert_eq!(
        store.note_id_for_remote_id(remote_id).unwrap(),
        None,
        "criterion 7: remote_id must not be adopted when the transaction rolled back"
    );
    let total_rows: i64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        total_rows, 1,
        "criterion 7: no orphan row from the aborted transaction; existing_id={existing_id}"
    );
}

#[test]
fn max_remote_id_is_the_pull_cursor() {
    let store = open_store();

    // Nothing synced yet → no cursor (caller does a full catch-up).
    assert_eq!(store.max_remote_id().unwrap(), None);

    // Record a few cloud ids. UUIDv7 strings sort lexically == time order, so
    // MAX() returns the newest one regardless of insertion order.
    let (a, _) = store
        .add_note("note", "A", "b", &[], &[], None, None)
        .unwrap();
    let (b, _) = store
        .add_note("note", "B", "b", &[], &[], None, None)
        .unwrap();
    let (c, _) = store
        .add_note("note", "C", "b", &[], &[], None, None)
        .unwrap();
    store
        .set_remote_id(&b, "01890000-0000-7000-8000-000000000002")
        .unwrap();
    store
        .set_remote_id(&a, "01890000-0000-7000-8000-000000000001")
        .unwrap();
    store
        .set_remote_id(&c, "01890000-0000-7000-8000-000000000003")
        .unwrap();

    assert_eq!(
        store.max_remote_id().unwrap().as_deref(),
        Some("01890000-0000-7000-8000-000000000003"),
        "cursor must be the max (newest) remote_id"
    );
}

// Direct, fast unit test on the cursor's lexical-sort assumption using
// genuinely generated `Uuid::now_v7()` values (inkentry-oss story 272/269
// hardening), not hand-typed strings: the server mints `sync_id` the same
// way, so this proves `MAX(remote_id)` picks the truly newest entry for
// real UUIDv7 output, independent of the row insertion order used to
// stamp them. A future regression that acks a push with anything other
// than a genuine `sync_id` (e.g. a raw autoincrement row id, which sorts
// lexically after any current-era UUIDv7's smaller leading hex digits)
// would fail here in milliseconds, instead of only surfacing via the
// full-server integration test.
#[test]
fn max_remote_id_orders_real_uuidv7_values_by_time_not_insertion_order() {
    let store = open_store();

    let (first_row, _) = store
        .add_note("note", "first", "b", &[], &[], None, None)
        .unwrap();
    let (second_row, _) = store
        .add_note("note", "second", "b", &[], &[], None, None)
        .unwrap();

    // Two genuinely generated UUIDv7 values. Sort them ourselves (don't
    // assume generation order == lexical order across two close calls) and
    // stamp the lexically SMALLER one onto the row added FIRST, so a
    // passing result can't be explained by MAX() secretly tracking
    // insertion/rowid order instead of the UUIDv7 string's own value.
    let uuid_x = uuid::Uuid::now_v7().to_string();
    let uuid_y = uuid::Uuid::now_v7().to_string();
    let (smaller, larger) = if uuid_x < uuid_y {
        (uuid_x, uuid_y)
    } else {
        (uuid_y, uuid_x)
    };
    store.set_remote_id(&first_row, &smaller).unwrap();
    store.set_remote_id(&second_row, &larger).unwrap();

    assert_eq!(
        store.max_remote_id().unwrap().as_deref(),
        Some(larger.as_str()),
        "MAX(remote_id) must return the lexically largest real UUIDv7, \
         not whichever row it happens to be stamped on"
    );
}

// ── has_note: does this store own the id? ─────────────────────────────────
// Used to apply a relayed push-ack back onto the originating row.

#[test]
fn has_note_recognises_only_ids_this_store_minted() {
    let store = open_store();
    let (id, _) = store
        .add_note("note", "N", "b", &[], &[], None, None)
        .unwrap();

    assert!(store.has_note(&id).unwrap());
    assert!(!store.has_note(&absent_id()).unwrap());
}

// ── pending_sync_count: cheap outbox count, never mutates ──────────────────

#[test]
fn pending_sync_count_reports_unpushed_active_rows() {
    let store = open_store();
    assert_eq!(store.pending_sync_count().unwrap(), 0);

    let (a, _) = store
        .add_note("note", "A", "b", &[], &[], None, None)
        .unwrap();
    store
        .add_note("note", "B", "b", &[], &[], None, None)
        .unwrap();
    assert_eq!(
        store.pending_sync_count().unwrap(),
        2,
        "two freshly-added active rows, neither pushed yet"
    );

    store
        .set_remote_id(&a, "01890000-0000-7000-8000-0000000000aa")
        .unwrap();
    assert_eq!(
        store.pending_sync_count().unwrap(),
        1,
        "a stamped remote_id excludes the row from the outbox count"
    );
}

#[test]
fn pending_sync_count_ignores_archived_rows() {
    let store = open_store();
    let (id, _) = store
        .add_note("note", "N", "b", &[], &[], None, None)
        .unwrap();
    store.archive(&id).unwrap();
    assert_eq!(
        store.pending_sync_count().unwrap(),
        0,
        "an archived-and-never-pushed row is not a pending push (matches \
         rows_for_sync's default include_archived=false view)"
    );
}

#[test]
fn pending_sync_count_is_a_pure_read_unaffected_by_rows_for_sync() {
    let store = open_store();
    store
        .add_note("note", "A", "b", &[], &[], None, None)
        .unwrap();
    store
        .add_note("note", "B", "b", &[], &[], None, None)
        .unwrap();

    // pending_sync_count must never mutate: calling it repeatedly, interleaved
    // with the real read, must not change what either sees.
    assert_eq!(store.pending_sync_count().unwrap(), 2);
    let rows = store.rows_for_sync(false).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        store.pending_sync_count().unwrap(),
        2,
        "count is unaffected by the preceding rows_for_sync call"
    );
}

#[test]
fn set_remote_id_records_and_dedupes() {
    let store = open_store();
    let (id, _) = store
        .add_note("note", "N", "b", &[], &[], None, None)
        .unwrap();
    let remote_id = "01890000-0000-7000-8000-0000000000ff";

    assert!(!store.has_remote_id(remote_id).unwrap());
    store.set_remote_id(&id, remote_id).unwrap();
    assert!(store.has_remote_id(remote_id).unwrap());
    assert_eq!(store.note_id_for_remote_id(remote_id).unwrap(), Some(id));
}

#[test]
fn add_note_persists_entity_id() {
    let store = open_store();
    let (id, _) = store
        .add_note("decision", "HTTP layer", "use axum", &[], &[], None, None)
        .unwrap();

    let stored: Option<String> = store
        .conn
        .query_row(
            "SELECT entity_id FROM notes WHERE uuid = ?1",
            rusqlite::params![id.as_str()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        stored.as_deref(),
        Some("cc308a1ca5d849191e1710cc9def561377a9ef37e4fcb895e5aa3b1896e43603"),
        "the stored column must hold the canonical id"
    );
}

#[test]
fn union_tags_and_files_is_add_wins() {
    let store = open_store();
    let (id, _) = store
        .add_note("note", "N", "b", &["alpha"], &["a.rs"], None, None)
        .unwrap();

    let read = |store: &MemoryStore| -> (Option<String>, Option<String>) {
        store
            .conn
            .query_row(
                "SELECT tags, linked_files FROM notes WHERE uuid = ?1",
                rusqlite::params![id.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    };

    // New values are appended; the existing ones survive.
    assert!(
        store
            .union_tags_and_files(&id, &["beta".to_string()], &["b.rs".to_string()])
            .unwrap()
    );
    assert_eq!(read(&store).0.as_deref(), Some("alpha,beta"));
    assert_eq!(read(&store).1.as_deref(), Some("a.rs,b.rs"));

    // Nothing new to add: no write, and nothing is dropped.
    assert!(
        !store
            .union_tags_and_files(&id, &["alpha".to_string()], &[])
            .unwrap(),
        "a subset must not rewrite the row"
    );
    assert_eq!(read(&store).0.as_deref(), Some("alpha,beta"));
}

// The union rewrites `tags`, and `tags` is an FTS-indexed column — the
// AFTER UPDATE trigger must keep the index in step or search goes stale.
#[test]
fn union_tags_keeps_fts_in_sync() {
    let store = open_store();
    let (id, _) = store
        .add_note("note", "Findable", "body", &["alpha"], &[], None, None)
        .unwrap();
    store
        .union_tags_and_files(&id, &["zetatag".to_string()], &[])
        .unwrap();

    let hits: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM memory_fts WHERE memory_fts MATCH 'zetatag'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hits, 1, "the unioned tag must be searchable");
}

// ── insert_embedding ─────────────────────────────────────────────────────────

// `note_embeddings` is keyed by the storage surrogate, which is the only place
// outside this module's own SQL that needs it.
fn embedding_rowid(store: &MemoryStore, id: &NoteId) -> i64 {
    store
        .rowid_for(id)
        .expect("rowid lookup")
        .expect("note must exist")
}

// `note_embeddings` is a `vec0` virtual table, so like the code `embeddings`
// table it does not honour `INSERT OR REPLACE`: re-embedding an existing
// note must overwrite in place (one last-write-wins row), not error or
// duplicate.
#[test]
fn insert_embedding_replaces_a_repeated_note_id() {
    let store = open_store();
    let (id, _) = store
        .add_note("note", "N", "b", &[], &[], None, None)
        .unwrap();
    let rowid = embedding_rowid(&store, &id);

    let dim = crate::embeddings::EMBEDDING_DIM;
    let mut first = vec![0f32; dim];
    first[0] = 1.0;
    let mut second = vec![0f32; dim];
    second[5] = 1.0;

    store
        .insert_embedding(&id, &crate::embeddings::vec_to_blob(&first))
        .expect("first note embedding");
    store
        .insert_embedding(&id, &crate::embeddings::vec_to_blob(&second))
        .expect("second note embedding (replace)");

    let count: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM note_embeddings WHERE note_id = ?1",
            rusqlite::params![rowid],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "a repeated note must leave exactly one row");

    let stored: Vec<u8> = store
        .conn
        .query_row(
            "SELECT embedding FROM note_embeddings WHERE note_id = ?1",
            rusqlite::params![rowid],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        stored,
        crate::embeddings::vec_to_blob(&second),
        "the second embedding must overwrite the first"
    );
}

// Replacing a note that has never been embedded must be a harmless no-op
// DELETE followed by a normal INSERT, not an error — the common case of
// embedding a note for the first time.
#[test]
fn insert_embedding_of_nonexistent_note_id_is_a_harmless_delete_no_op() {
    let store = open_store();
    let (id, _) = store
        .add_note("note", "N", "b", &[], &[], None, None)
        .unwrap();
    let rowid = embedding_rowid(&store, &id);

    let dim = crate::embeddings::EMBEDDING_DIM;
    let mut vector = vec![0f32; dim];
    vector[7] = 1.0;

    store
        .insert_embedding(&id, &crate::embeddings::vec_to_blob(&vector))
        .expect("embedding a never-before-embedded note must succeed");

    let count: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM note_embeddings WHERE note_id = ?1",
            rusqlite::params![rowid],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 1,
        "the first embed for a fresh note must land exactly once"
    );
}

// The strongest test of "joins the existing transaction" vs. "just happens
// not to error": call `insert_embedding` for a repeated note from WITHIN a
// transaction the caller already opened, then roll that outer transaction
// back. If the delete+insert genuinely joined the caller's transaction,
// rolling it back must undo both halves, restoring the pre-transaction row
// exactly.
#[test]
fn insert_embedding_joins_callers_transaction_and_rolls_back_with_it() {
    let store = open_store();
    let (id, _) = store
        .add_note("note", "N", "b", &[], &[], None, None)
        .unwrap();
    let rowid = embedding_rowid(&store, &id);

    let dim = crate::embeddings::EMBEDDING_DIM;
    let mut first = vec![0f32; dim];
    first[0] = 1.0;
    store
        .insert_embedding(&id, &crate::embeddings::vec_to_blob(&first))
        .expect("seed row (autocommit)");

    let mut second = vec![0f32; dim];
    second[1] = 1.0;

    {
        let tx = store
            .conn
            .unchecked_transaction()
            .expect("caller opens an outer transaction");
        assert!(
            !store.conn.is_autocommit(),
            "precondition: connection must be mid-transaction, exercising the \
             is_autocommit() guard's join branch rather than its own-BEGIN branch"
        );

        store
            .insert_embedding(&id, &crate::embeddings::vec_to_blob(&second))
            .expect("replacing inside the caller's open transaction must not nest a BEGIN");

        tx.rollback().expect("roll back the outer transaction");
    }

    let count: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM note_embeddings WHERE note_id = ?1",
            rusqlite::params![rowid],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 1,
        "rollback must not leave the row deleted — the DELETE half of the \
         replace was part of the outer transaction and must roll back with it"
    );

    let stored: Vec<u8> = store
        .conn
        .query_row(
            "SELECT embedding FROM note_embeddings WHERE note_id = ?1",
            rusqlite::params![rowid],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        stored,
        crate::embeddings::vec_to_blob(&first),
        "rollback must restore the pre-transaction (first) vector — if the \
         delete+insert had committed independently of the caller's \
         transaction, the row would still hold `second` here"
    );
}

// ── notes_missing_embeddings / reindex candidate queries ─────────────────────

// Give `note_id` a valid 896-dim embedding so it drops out of the missing set.
fn embed(store: &MemoryStore, note_id: &NoteId) {
    let blob = crate::embeddings::vec_to_blob(&[0.1f32; 896]);
    store
        .insert_embedding(note_id, &blob)
        .expect("insert embedding");
}

fn missing_ids(store: &MemoryStore, include_archived: bool) -> Vec<NoteId> {
    store
        .notes_missing_embeddings(include_archived)
        .expect("query missing")
        .into_iter()
        .map(|(id, ..)| id)
        .collect()
}

// Seed one active note, return its id.
fn add_active(store: &MemoryStore, title: &str) -> NoteId {
    store
        .add_note(
            "note",
            title,
            &format!("body of {title}"),
            &[],
            &[],
            None,
            None,
        )
        .expect("add note")
        .0
}

#[test]
fn notes_missing_embeddings_returns_only_active_unembedded_by_default() {
    let store = open_store();
    let a = add_active(&store, "a");
    let b = add_active(&store, "b");
    let c = add_active(&store, "c");
    // Embed exactly one of the three.
    embed(&store, &b);

    let mut got = missing_ids(&store, false);
    got.sort();
    let mut want = vec![a, c];
    want.sort();
    assert_eq!(
        got, want,
        "only the two active-unembedded notes are returned"
    );
}

#[test]
fn notes_missing_embeddings_excludes_embedded_and_archived_by_default() {
    let store = open_store();
    let embedded = add_active(&store, "active-embedded");
    embed(&store, &embedded);
    let unembedded = add_active(&store, "active-unembedded");
    let archived = add_active(&store, "archived-unembedded");
    assert!(store.archive(&archived).expect("archive"));

    assert_eq!(
        missing_ids(&store, false),
        vec![unembedded],
        "default mode returns only the active, unembedded note"
    );
}

#[test]
fn notes_missing_embeddings_boundaries_all_and_none_embedded() {
    let store = open_store();
    let a = add_active(&store, "a");
    let b = add_active(&store, "b");

    // None embedded: both returned in insertion order.
    assert_eq!(missing_ids(&store, false), vec![a.clone(), b.clone()]);

    // All embedded: empty.
    embed(&store, &a);
    embed(&store, &b);
    assert!(
        missing_ids(&store, false).is_empty(),
        "a fully embedded store has nothing missing"
    );
}

#[test]
fn notes_missing_embeddings_include_archived_covers_archived() {
    let store = open_store();
    let active = add_active(&store, "active");
    let archived = add_active(&store, "archived");
    assert!(store.archive(&archived).expect("archive"));

    let mut got = missing_ids(&store, true);
    got.sort();
    let mut want = vec![active, archived];
    want.sort();
    assert_eq!(
        got, want,
        "include_archived surfaces the unembedded archived note too"
    );
}

#[test]
fn insert_embedding_drops_note_out_and_force_query_keeps_all() {
    let store = open_store();
    let a = add_active(&store, "a");
    let b = add_active(&store, "b");

    assert_eq!(missing_ids(&store, false), vec![a.clone(), b.clone()]);
    embed(&store, &a);
    assert_eq!(
        missing_ids(&store, false),
        vec![b.clone()],
        "an embedded note drops out of notes_missing_embeddings"
    );

    // The force-path query returns every active note regardless of embedding.
    let force: Vec<NoteId> = store
        .all_active_notes_for_reembed(false)
        .expect("force query")
        .into_iter()
        .map(|(id, ..)| id)
        .collect();
    assert_eq!(
        force,
        vec![a, b],
        "the --force candidate set is every active note, embedded or not"
    );
}

#[test]
fn notes_missing_embeddings_returns_title_and_body_for_embed_text() {
    let store = open_store();
    let id = store
        .add_note("decision", "My Title", "the body", &[], &[], None, None)
        .expect("add")
        .0;
    let rows = store.notes_missing_embeddings(false).expect("query");
    assert_eq!(
        rows,
        vec![(id, "My Title".to_string(), "the body".to_string())]
    );
}

fn force_ids(store: &MemoryStore, include_archived: bool) -> Vec<NoteId> {
    store
        .all_active_notes_for_reembed(include_archived)
        .expect("force query")
        .into_iter()
        .map(|(id, ..)| id)
        .collect()
}

// The --force candidate set must widen to archived notes under
// include_archived, and must NOT include them otherwise. Without this, a
// `--force --include-archived` run would silently skip archived notes and a
// wrong WHERE clause (still filtering status = 'active') would go unnoticed.
#[test]
fn all_active_notes_for_reembed_include_archived_covers_archived_and_embedded() {
    let store = open_store();
    let active = add_active(&store, "active");
    let archived = add_active(&store, "archived");
    assert!(store.archive(&archived).expect("archive"));
    // Embed the active one: the force set must still return it (embedded or
    // not) so --force re-embeds everything.
    embed(&store, &active);

    assert_eq!(
        force_ids(&store, false),
        vec![active.clone()],
        "default force set is active notes only, regardless of embedding"
    );

    let mut got = force_ids(&store, true);
    got.sort();
    let mut want = vec![active, archived];
    want.sort();
    assert_eq!(
        got, want,
        "include_archived force set covers the archived note too"
    );
}

// A superseded note is archived (supersede sets status = 'archived'), so it
// must drop out of the default missing set and reappear only under
// include_archived. Pins the superseded-handling half of the query contract:
// reindex must not re-embed a note the user has explicitly superseded unless
// they opt in.
#[test]
fn superseded_note_excluded_by_default_included_with_archived() {
    let store = open_store();
    let old = add_active(&store, "old");
    let new = add_active(&store, "new");
    assert!(store.supersede(&old, &new).expect("supersede"));

    // Default: the superseded (now archived) note is gone; only the active
    // successor is missing.
    assert_eq!(
        missing_ids(&store, false),
        vec![new.clone()],
        "a superseded note is archived, so default reindex skips it"
    );

    // include_archived: both the successor and the superseded note surface.
    let mut got = missing_ids(&store, true);
    got.sort();
    let mut want = vec![old, new];
    want.sort();
    assert_eq!(
        got, want,
        "include_archived surfaces the superseded note for backfill"
    );
}

// ── schema creation (there is no migration ladder) ───────────────────────────
// `memory_001_initial.sql` declares the final shape and every statement in it
// is `IF NOT EXISTS`, so creation is idempotent on a store this binary already
// made, and a store stamped with any other version is refused outright.

fn user_version(store: &MemoryStore) -> i32 {
    store
        .conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn fresh_memory_db_stamps_current_version() {
    let store = open_store();
    assert_eq!(user_version(&store), super::MEMORY_SCHEMA_VERSION);
}

// Re-opening an already-created store is a clean no-op that keeps the version
// and touches no existing row.
#[test]
fn reopen_memory_db_is_idempotent() {
    register_sqlite_vec();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let path = tmp.path().join("memory.db");

    let store = MemoryStore::open(&path).expect("first open");
    let (id, _) = store
        .add_note("decision", "Keep", "body", &[], &[], None, None)
        .unwrap();
    drop(store);

    let reopened = MemoryStore::open(&path).expect("second open");
    assert_eq!(user_version(&reopened), super::MEMORY_SCHEMA_VERSION);
    assert_eq!(
        reopened.get(&id).unwrap().map(|n| n.title),
        Some("Keep".to_string()),
        "re-opening an already-created store must not touch existing rows"
    );
}

// A store stamped with a schema version newer than this binary supports (e.g.
// opened by an older binary after a newer one wrote it) refuses with a clear
// message instead of half-creating anything.
#[test]
fn future_memory_schema_version_refuses_to_open() {
    register_sqlite_vec();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let path = tmp.path().join("memory.db");

    {
        let store = MemoryStore::open(&path).expect("build current store");
        store
            .execute_batch(&format!(
                "PRAGMA user_version = {}",
                super::MEMORY_SCHEMA_VERSION + 1
            ))
            .expect("stamp a future version");
    }

    let err = match MemoryStore::open(&path) {
        Ok(_) => panic!("an older binary must refuse a DB stamped with a newer schema version"),
        Err(e) => e,
    };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("newer") || msg.contains("upgrade inkentry"),
        "the error must explain the version mismatch clearly, got: {msg}"
    );
}

// `MemoryStore::open` sets no `busy_timeout` on its connection (same as
// `Database::open` for index.db), so a second writer holding the file's write
// lock while `open` creates the schema must surface as a loud `SQLITE_BUSY`
// error, not hang or silently race on it. Creating is the only write `open`
// still performs: reopening an already-stamped store touches nothing.
#[test]
fn concurrent_open_under_a_held_write_lock_fails_loudly_not_silently() {
    register_sqlite_vec();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let path = tmp.path().join("memory.db");

    let locker = rusqlite::Connection::open(&path).expect("second connection");
    locker
        .execute_batch("BEGIN IMMEDIATE; CREATE TABLE lock_probe (id INTEGER);")
        .expect("acquire the write lock");

    let err = MemoryStore::open(&path)
        .err()
        .expect("opening under a held write lock must fail, not hang or corrupt state");
    let msg = format!("{err:#}").to_lowercase();
    assert!(
        msg.contains("lock") || msg.contains("busy"),
        "expected a locking error, got: {msg}"
    );

    // Nothing half-created survives the failed attempt: the schema and its
    // stamp commit together.
    locker.execute_batch("ROLLBACK;").expect("release the lock");
    let recovered = MemoryStore::open(&path).expect("once the lock is released, open completes");
    assert_eq!(user_version(&recovered), super::MEMORY_SCHEMA_VERSION);
}

// A single-note reopen test cannot distinguish "row content survives" from
// "row COUNT survives but rows got cross-attributed" (e.g. an embedding
// landing on the wrong note, or FTS text from one note leaking onto another).
// Use several notes with distinct kind/title/body/tags and distinct
// embeddings, and assert each note's own content, its own embedding, and its
// own FTS match survive a reopen attached to the correct row.
#[test]
fn reopen_preserves_distinct_multi_row_content_not_just_row_count() {
    register_sqlite_vec();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let path = tmp.path().join("memory.db");

    let rows = [
        (
            "decision",
            "Alpha decision",
            "alpha body text",
            vec!["infra"],
            0.11f32,
        ),
        (
            "context",
            "Beta context",
            "beta body text",
            vec!["billing"],
            0.22f32,
        ),
        (
            "requirement",
            "Gamma requirement",
            "gamma body text",
            vec!["auth", "urgent"],
            0.33f32,
        ),
    ];
    let mut ids = Vec::new();
    {
        let store = MemoryStore::open(&path).expect("build store");
        for (kind, title, body, tags, fill) in &rows {
            let (id, _) = store
                .add_note(kind, title, body, tags, &[], None, None)
                .unwrap();
            let vector = vec![*fill; crate::embeddings::EMBEDDING_DIM];
            store
                .insert_embedding(&id, &crate::embeddings::vec_to_blob(&vector))
                .unwrap();
            ids.push(id);
        }
    }

    let store = MemoryStore::open(&path).expect("reopen");
    assert_eq!(user_version(&store), super::MEMORY_SCHEMA_VERSION);

    for (id, (kind, title, body, tags, fill)) in ids.iter().zip(rows.iter()) {
        let note = store
            .get(id)
            .unwrap()
            .unwrap_or_else(|| panic!("note {id} must survive the reopen"));
        assert_eq!(&note.kind, kind, "note {id} kind must not cross-attribute");
        assert_eq!(
            &note.title, title,
            "note {id} title must not cross-attribute"
        );
        assert_eq!(&note.body, body, "note {id} body must not cross-attribute");
        assert_eq!(&note.tags, tags, "note {id} tags must not cross-attribute");

        let embedding = store
            .get_embedding(id)
            .unwrap()
            .unwrap_or_else(|| panic!("note {id} embedding must survive the reopen"));
        let vector = crate::embeddings::blob_to_vec(&embedding);
        assert!(
            vector.iter().all(|v| (*v - *fill).abs() < 1e-4),
            "note {id} embedding content must be its own, not another note's"
        );

        let hits: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM memory_fts WHERE memory_fts MATCH ?1",
                rusqlite::params![title],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            hits, 1,
            "note {id}'s own title must be findable via FTS after the reopen"
        );
    }
}

// ── point-in-time (`--as-of`) reconstruction ────────────────────────────────
// The as-of filter must return exactly the entries live at instant T:
//     COALESCE(valid_at, created_at) <= T AND (invalid_at IS NULL OR invalid_at > T)
// independent of archived status. Two boundaries are pinned here:
//   * an entry invalidated AFTER T (a since-superseded decision) is present at
//     T without needing include_archived (the then-current decision);
//   * an entry whose valid_at is AFTER T is absent — including the case where
//     valid_at is inherited from created_at (stored NULL), which must be read
//     as created_at, not as "always valid".
// The `--archived` dimension must not change which historically-live entries
// appear, so the list case is asserted with include_archived both false and
// true.

const DAY: i64 = 86_400;
// 2026-01-01T00:00:00Z, the base every date below is offset from.
const Y2026: i64 = 1_767_225_600;
const JAN_15: i64 = Y2026 + 14 * DAY; // superseded decision becomes valid
const MAR_01_NOON: i64 = Y2026 + 59 * DAY + 12 * 3_600; // probe, before the supersede
const MAY_01: i64 = Y2026 + 120 * DAY; // probe, still before the supersede
const JUN_20: i64 = Y2026 + 170 * DAY; // supersede instant: old.invalid_at, new.valid_at
const JUL_01: i64 = Y2026 + 181 * DAY; // probe, after the supersede
const AUG_04: i64 = Y2026 + 215 * DAY; // "today": the created-today entry's created_at

fn id_set(notes: &[super::Note]) -> std::collections::BTreeSet<NoteId> {
    notes.iter().map(|n| n.id.clone()).collect()
}

// Seed a real supersede chain plus one entry created "today" with no explicit
// valid_at. Returns (old_decision, new_decision, created_today). All three
// share the FTS term "quorum" so search_text can retrieve every one.
//
//   old_decision   valid JAN_15, superseded → status archived, invalid_at JUN_20
//   new_decision   valid JUN_20, active successor
//   created_today  created AUG_04, valid_at stored NULL (inherits created_at)
fn seed_as_of_chain(store: &MemoryStore) -> (NoteId, NoteId, NoteId) {
    let (old_id, _) = store
        .add_note(
            "decision",
            "Cache writes synchronously",
            "quorum write-through",
            &[],
            &[],
            None,
            Some(JAN_15),
        )
        .unwrap();
    let (new_id, _) = store
        .add_note_superseding(
            "decision",
            "Cache writes asynchronously",
            "quorum write-behind",
            &[],
            &[],
            Some(JUN_20),
            &old_id,
        )
        .unwrap();
    // add_note_superseding stamps the old entry's invalid_at with wall-clock
    // now(); pin it to the historical supersede instant so the point-in-time
    // boundary is deterministic. This is exactly the archived + invalid_at
    // state a reconciled/harvested supersede leaves behind.
    store
        .conn
        .execute(
            "UPDATE notes SET invalid_at = ?1 WHERE uuid = ?2",
            rusqlite::params![JUN_20, old_id.as_str()],
        )
        .unwrap();
    // Created "today" with no --valid-at: valid_at is stored NULL and the
    // as-of filter must treat it as created_at, not as "always valid".
    let (today_id, _) = store
        .add_note_with_created_at(
            "decision",
            "Cache eviction policy",
            "quorum lru",
            &[],
            &[],
            None,
            "active",
            AUG_04,
        )
        .unwrap();
    (old_id, new_id, today_id)
}

// list --as-of must reconstruct the exact set live at T, and that set must not
// depend on include_archived (the archived flag controls the *current* view,
// never which historically-live entries a point-in-time query returns).
#[test]
fn list_as_of_reconstructs_point_in_time_ignoring_archived() {
    let store = open_store();
    let (old_id, new_id, today_id) = seed_as_of_chain(&store);

    // (as_of, expected live ids) for a `--kind decision` listing.
    let cases: Vec<(i64, Vec<NoteId>)> = vec![
        // Before the supersede: the old decision is the then-current one; the
        // successor is not yet valid and the created-today entry is future.
        (MAR_01_NOON, vec![old_id.clone()]),
        (MAY_01, vec![old_id.clone()]),
        // After the supersede: the old decision is gone (invalid_at <= T), the
        // successor is live, the created-today entry is still future.
        (JUL_01, vec![new_id.clone()]),
        // "Today": the created-today entry finally becomes valid; the old
        // decision stays gone.
        (AUG_04, vec![new_id.clone(), today_id.clone()]),
    ];

    for (as_of, expected) in &cases {
        let want: std::collections::BTreeSet<NoteId> = expected.iter().cloned().collect();
        for include_archived in [false, true] {
            let got = id_set(
                &store
                    .list_filtered(Some("decision"), None, 100, include_archived, Some(*as_of))
                    .unwrap(),
            );
            assert_eq!(
                got, want,
                "as_of={as_of} include_archived={include_archived}: point-in-time \
                 set must match and must not depend on archived status"
            );
        }
    }
}

// search --as-of (text mode) must apply the same corrected filter: a
// since-superseded (archived) entry live at T is returned, and an entry whose
// inherited valid_at is after T is not. search_text has no include_archived
// flag, so as_of alone must surface the archived-but-then-live entry.
#[test]
fn search_text_as_of_reconstructs_point_in_time_ignoring_archived() {
    let store = open_store();
    let (old_id, new_id, today_id) = seed_as_of_chain(&store);

    let cases: Vec<(i64, Vec<NoteId>)> = vec![
        (MAR_01_NOON, vec![old_id.clone()]),
        (JUL_01, vec![new_id.clone()]),
        (AUG_04, vec![new_id.clone(), today_id.clone()]),
    ];
    for (as_of, expected) in &cases {
        let want: std::collections::BTreeSet<NoteId> = expected.iter().cloned().collect();
        let got = id_set(&store.search_text("quorum", 50, Some(*as_of)).unwrap());
        assert_eq!(
            got, want,
            "search_text as_of={as_of}: point-in-time set must match regardless \
             of archived status"
        );
    }
}

// ── list_by_entity_ids ───────────────────────────────────────────────────────

// `list_by_entity_ids` returns exactly the rows whose entity_id is requested,
// and applies the same active-only / include_archived gate as `list_filtered`.
// This is the SQLite read-back that `memory list --source-ref` uses once the
// git-notes anchor has resolved which entities belong to the commit.
#[test]
fn list_by_entity_ids_selects_and_respects_archived() {
    use crate::storage::entity_id::entity_id;

    let store = open_store();
    let (id_a, _) = store
        .add_note("decision", "A", "body a", &[], &[], None, None)
        .unwrap();
    store
        .add_note("decision", "B", "body b", &[], &[], None, None)
        .unwrap();

    let ea = entity_id("decision", "A", "body a");
    let eb = entity_id("decision", "B", "body b");

    // Only the requested entity comes back.
    let only_a = store
        .list_by_entity_ids(std::slice::from_ref(&ea), 50, false, None)
        .unwrap();
    assert_eq!(only_a.len(), 1);
    assert_eq!(only_a[0].title, "A");

    // Both when both ids are requested.
    let both = store
        .list_by_entity_ids(&[ea.clone(), eb.clone()], 50, false, None)
        .unwrap();
    assert_eq!(both.len(), 2);

    // An empty id list is a no-op, not a full-table scan.
    assert!(
        store
            .list_by_entity_ids(&[], 50, false, None)
            .unwrap()
            .is_empty()
    );

    // An unknown id matches nothing.
    assert!(
        store
            .list_by_entity_ids(&["deadbeef".to_string()], 50, false, None)
            .unwrap()
            .is_empty()
    );

    // Archiving A hides it by default, but include_archived surfaces it again.
    store.archive(&id_a).unwrap();
    assert!(
        store
            .list_by_entity_ids(std::slice::from_ref(&ea), 50, false, None)
            .unwrap()
            .is_empty(),
        "archived entry must be hidden when include_archived is false"
    );
    let with_archived = store.list_by_entity_ids(&[ea], 50, true, None).unwrap();
    assert_eq!(with_archived.len(), 1, "include_archived must surface it");
    assert_eq!(with_archived[0].status, "archived");
}
