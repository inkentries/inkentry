use super::MemoryStore;
use std::sync::OnceLock;

/// Register the sqlite-vec extension exactly once per test process.
/// `MemoryStore::migrate()` creates a `vec0` virtual table, which
/// requires the extension to be loaded before any connection is opened.
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

fn count_edges(store: &MemoryStore, from_id: i64, to_id: i64, kind: &str) -> i64 {
    store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM memory_edges WHERE from_id = ?1 AND to_id = ?2 AND kind = ?3",
            rusqlite::params![from_id, to_id, kind],
            |r| r.get(0),
        )
        .unwrap_or(0)
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

    let changed = store.supersede(old_id, new_id).unwrap();
    assert!(changed, "supersede() should return true on first call");

    // (a) old note must be archived with superseded_by set
    let old_note = store.get(old_id).unwrap().expect("old note must exist");
    assert_eq!(old_note.status, "archived");
    assert_eq!(old_note.superseded_by, Some(new_id));

    // (b) a memory_edges row must exist linking new → old
    assert_eq!(
        count_edges(&store, new_id, old_id, "supersedes"),
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

    let first = store.supersede(old_id, new_id).unwrap();
    assert!(first);

    // Second call on an already-archived note must return false
    let second = store.supersede(old_id, new_id).unwrap();
    assert!(
        !second,
        "supersede() should return false when note is already archived"
    );

    // Must not have inserted a duplicate edge
    assert_eq!(
        count_edges(&store, new_id, old_id, "supersedes"),
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
            old_id,
        )
        .unwrap();
    assert!(
        created,
        "a fresh supersede insert must report created = true"
    );

    let old_note = store.get(old_id).unwrap().expect("old note must exist");
    assert_eq!(old_note.status, "archived");
    assert_eq!(old_note.superseded_by, Some(new_id));

    assert_eq!(
        count_edges(&store, new_id, old_id, "supersedes"),
        1,
        "expected exactly one supersedes edge"
    );
}

/// ADR-068 amendment E4: re-superseding an already-archived OLD (via a second
/// `add_note_superseding` call naming a different successor) must reject with
/// an error and roll back the whole transaction — no orphaned new note, no
/// second supersedes edge, OLD's existing successor link untouched.
#[test]
fn add_note_superseding_rejects_already_archived_old_and_writes_nothing() {
    let store = open_store();

    let (old_id, _) = store
        .add_note("decision", "Old decision", "old body", &[], &[], None, None)
        .unwrap();
    let (successor_a, _) = store
        .add_note_superseding("decision", "Successor A", "body a", &[], &[], None, old_id)
        .unwrap();

    let count_before = store.count().unwrap();

    let result =
        store.add_note_superseding("decision", "Successor B", "body b", &[], &[], None, old_id);
    assert!(
        result.is_err(),
        "re-superseding an already-archived OLD must error, not silently succeed"
    );

    assert_eq!(
        store.count().unwrap(),
        count_before,
        "a rejected supersede must not leave an orphaned new note row"
    );

    let old_note = store.get(old_id).unwrap().expect("old note must exist");
    assert_eq!(
        old_note.superseded_by,
        Some(successor_a),
        "OLD's successor link must still point at the first, not the rejected second, successor"
    );

    assert_eq!(
        count_edges(&store, successor_a, old_id, "supersedes"),
        1,
        "the original supersedes edge must be untouched"
    );
}

/// Superseding a nonexistent OLD id must also error, not silently create an
/// unlinked new note (the archive-`OLD` `UPDATE` matches zero rows either way).
#[test]
fn add_note_superseding_rejects_nonexistent_old() {
    let store = open_store();
    let count_before = store.count().unwrap();

    let result = store.add_note_superseding("decision", "New", "new body", &[], &[], None, 999_999);
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
            .add_edge(a, b, kind)
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
        .add_edge(a, b, "invented")
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

    store.add_edge(a, b, "relates_to").unwrap();
    store.add_edge(a, b, "relates_to").unwrap(); // second call must not error

    assert_eq!(
        count_edges(&store, a, b, "relates_to"),
        1,
        "duplicate edge must not produce a second row"
    );
}

// ── UUID identity + cursor + idempotent apply ────────────────────────────────

#[test]
fn ensure_uuid_backfills_and_is_idempotent() {
    let store = open_store();
    let (id, _) = store
        .add_note("decision", "D", "body", &[], &[], None, None)
        .unwrap();

    // No UUID until first sync.
    assert_eq!(store.uuid_for(id).unwrap(), None);

    let u1 = store.ensure_uuid(id).unwrap();
    assert!(!u1.is_empty());
    // UUIDv7 string form is 36 chars.
    assert_eq!(u1.len(), 36);

    // Re-running keeps the same UUID (idempotent backfill).
    let u2 = store.ensure_uuid(id).unwrap();
    assert_eq!(u1, u2);
    assert_eq!(store.uuid_for(id).unwrap(), Some(u1));
}

#[test]
fn rows_for_sync_assigns_uuids_and_is_text_only() {
    let store = open_store();
    store
        .add_note("decision", "One", "first", &[], &[], None, None)
        .unwrap();
    store
        .add_note("note", "Two", "second", &[], &[], None, None)
        .unwrap();

    let rows = store.rows_for_sync(false).unwrap();
    assert_eq!(rows.len(), 2);
    // Every row carries a freshly-assigned UUID; SyncRow has no embedding field
    // at all (text-only by construction).
    for r in &rows {
        assert_eq!(r.uuid.len(), 36);
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
    assert_eq!(store.get(local_id).unwrap().unwrap().status, "active");

    // A pulled tombstone archives the local copy (never un-archives).
    let inserted = store
        .apply_remote_note(remote_id, "note", "T", "b", None, 1_700_000_000, true)
        .unwrap();
    assert!(!inserted);
    assert_eq!(store.get(local_id).unwrap().unwrap().status, "archived");
}

// ── apply_remote_note: entity_id + collision recovery ──────────────────────
// A fresh :memory: store (via open_store) has zero rows at construction, so
// `MemoryStore::open`'s Step B promotes idx_notes_entity_id to UNIQUE
// immediately; every test below runs against an already-promoted index
// unless it explicitly drops back to a plain index to exercise criterion 8.

fn drop_entity_id_unique_constraint(store: &MemoryStore) {
    store
        .conn
        .execute_batch(
            "DROP INDEX idx_notes_entity_id; \
             CREATE INDEX idx_notes_entity_id ON notes(entity_id) WHERE entity_id IS NOT NULL;",
        )
        .unwrap();
}

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
            "SELECT entity_id FROM notes WHERE id = ?1",
            rusqlite::params![local_id],
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
    store.set_remote_id(existing_id, own_remote_id).unwrap();

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
    assert_eq!(store.get(existing_id).unwrap().unwrap().status, "active");

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
        store.get(existing_id).unwrap().unwrap().status,
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
    assert_eq!(store.get(existing_id).unwrap().unwrap().status, "archived");

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
        Some(existing_id),
        "criterion 6: the pulled remote_id must be adopted onto the existing row"
    );
    assert_eq!(
        store.get(existing_id).unwrap().unwrap().status,
        "archived",
        "criterion 6: a non-archived pull must never revert an archived row to active"
    );
}

#[test]
fn apply_remote_note_before_promotion_still_inserts_distinct_row() {
    let store = open_store();
    drop_entity_id_unique_constraint(&store);

    store
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

    let remote_id = "01890000-0000-7000-8000-000000000016";
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
        inserted,
        "criterion 8: pre-promotion, a pulled note must still land as a \
         distinct row alongside matching content"
    );
    assert_eq!(store.count().unwrap(), 2);
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
        .set_remote_id(b, "01890000-0000-7000-8000-000000000002")
        .unwrap();
    store
        .set_remote_id(a, "01890000-0000-7000-8000-000000000001")
        .unwrap();
    store
        .set_remote_id(c, "01890000-0000-7000-8000-000000000003")
        .unwrap();

    assert_eq!(
        store.max_remote_id().unwrap().as_deref(),
        Some("01890000-0000-7000-8000-000000000003"),
        "cursor must be the max (newest) remote_id"
    );
}

/// Direct, fast unit test on the cursor's lexical-sort assumption using
/// genuinely generated `Uuid::now_v7()` values (spelunk-oss story 272/269
/// hardening), not hand-typed strings: the server mints `sync_id` the same
/// way, so this proves `MAX(remote_id)` picks the truly newest entry for
/// real UUIDv7 output, independent of the row insertion order used to
/// stamp them. A future regression that acks a push with anything other
/// than a genuine `sync_id` (e.g. a raw autoincrement row id, which sorts
/// lexically after any current-era UUIDv7's smaller leading hex digits)
/// would fail here in milliseconds, instead of only surfacing via the
/// full-server integration test.
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
    store.set_remote_id(first_row, &smaller).unwrap();
    store.set_remote_id(second_row, &larger).unwrap();

    assert_eq!(
        store.max_remote_id().unwrap().as_deref(),
        Some(larger.as_str()),
        "MAX(remote_id) must return the lexically largest real UUIDv7, \
         not whichever row it happens to be stamped on"
    );
}

// ── note_id_for_uuid: forward lookup for applying a relayed push-ack ───────

#[test]
fn note_id_for_uuid_finds_the_row_that_owns_it() {
    let store = open_store();
    let (id, _) = store
        .add_note("note", "N", "b", &[], &[], None, None)
        .unwrap();
    let uuid = store.ensure_uuid(id).unwrap();

    assert_eq!(store.note_id_for_uuid(&uuid).unwrap(), Some(id));
    assert_eq!(store.note_id_for_uuid("no-such-uuid").unwrap(), None);
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
        .set_remote_id(a, "01890000-0000-7000-8000-0000000000aa")
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
    store.archive(id).unwrap();
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

    // pending_sync_count must never call ensure_uuid (a mutation): calling it
    // repeatedly, interleaved with the real mutating read, must not change
    // what either sees.
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
    store.ensure_uuid(id).unwrap();
    let remote_id = "01890000-0000-7000-8000-0000000000ff";

    assert!(!store.has_remote_id(remote_id).unwrap());
    store.set_remote_id(id, remote_id).unwrap();
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
            "SELECT entity_id FROM notes WHERE id = ?1",
            rusqlite::params![id],
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
                "SELECT tags, linked_files FROM notes WHERE id = ?1",
                rusqlite::params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    };

    // New values are appended; the existing ones survive.
    assert!(
        store
            .union_tags_and_files(id, &["beta".to_string()], &["b.rs".to_string()])
            .unwrap()
    );
    assert_eq!(read(&store).0.as_deref(), Some("alpha,beta"));
    assert_eq!(read(&store).1.as_deref(), Some("a.rs,b.rs"));

    // Nothing new to add: no write, and nothing is dropped.
    assert!(
        !store
            .union_tags_and_files(id, &["alpha".to_string()], &[])
            .unwrap(),
        "a subset must not rewrite the row"
    );
    assert_eq!(read(&store).0.as_deref(), Some("alpha,beta"));
}

/// The union rewrites `tags`, and `tags` is an FTS-indexed column — the
/// AFTER UPDATE trigger must keep the index in step or search goes stale.
#[test]
fn union_tags_keeps_fts_in_sync() {
    let store = open_store();
    let (id, _) = store
        .add_note("note", "Findable", "body", &["alpha"], &[], None, None)
        .unwrap();
    store
        .union_tags_and_files(id, &["zetatag".to_string()], &[])
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

// Runs against a store that predates the entity_id column and already holds
// rows colliding under the new key. Must add the column without aborting,
// backfill every legacy row (Step A), and leave the rows themselves alone:
// collapsing is `spelunk memory dedupe`'s job, so Step B must also leave the
// index non-unique while a duplicate group remains.
#[test]
fn entity_id_migration_backfills_but_does_not_collapse_duplicates() {
    register_sqlite_vec();
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("memory.db");

    // Build a store via the schema-only path (skips the Step A/B pipeline a
    // real `open()` runs), so seeding two duplicate-content rows below isn't
    // rejected by an index a fresh, zero-duplicate store would otherwise have
    // already promoted to UNIQUE. Then take the column away entirely to model
    // a genuinely pre-023 DB.
    {
        let conn = rusqlite::Connection::open(&path).expect("open raw");
        let store = MemoryStore {
            conn,
            reembed_needed: None,
        };
        store.migrate().expect("schema migration only");
        for created_at in [1_700_000_001_i64, 1_700_000_002] {
            store
                .add_note_with_created_at(
                    "decision",
                    "same text",
                    "same body",
                    &[],
                    &[],
                    None,
                    "active",
                    created_at,
                )
                .expect("seed duplicate-text note");
        }
        store
            .execute_batch(
                "DROP INDEX idx_notes_entity_id; \
                 ALTER TABLE notes DROP COLUMN entity_id;",
            )
            .expect("drop column to simulate the older schema");
        let has_col: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('notes') WHERE name = 'entity_id'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_col, 0, "precondition: the column is gone");
    }

    // Re-opening (the real `MemoryStore::open`) re-adds the column (migration
    // 023), then runs Step A (backfill) and Step B (duplicate scan).
    let store = MemoryStore::open(&path).expect("migration must not abort on existing data");

    let rows: i64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 2, "opening alone must not delete or merge any row");

    let has_col: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('notes') WHERE name = 'entity_id'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has_col, 1, "the column is added");

    // Step A backfills: no legacy row is left NULL.
    let nulls: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM notes WHERE entity_id IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        nulls, 0,
        "Step A must backfill entity_id for every legacy row"
    );
    assert_eq!(
        super::super::entity_id::note_entity_id(&store.list(None, 10, true).unwrap()[0]),
        super::super::entity_id::note_entity_id(&store.list(None, 10, true).unwrap()[1]),
        "the two legacy rows do collide under the new key"
    );

    // Step B must not have promoted the index: the two rows above are a
    // duplicate group, so the store stays on the non-unique index until an
    // explicit `spelunk memory dedupe` collapses it.
    let idx_sql: String = store
        .conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='index' AND name='idx_notes_entity_id'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        !idx_sql.to_uppercase().contains("UNIQUE"),
        "a duplicate group must keep the index non-unique: {idx_sql}"
    );

    // Idempotent: opening again is a no-op, not a duplicate-column error.
    drop(store);
    MemoryStore::open(&path).expect("re-open must be idempotent");
}

/// `note_embeddings` is a `vec0` virtual table, so like the code `embeddings`
/// table it does not honour `INSERT OR REPLACE`: re-embedding an existing
/// `note_id` must overwrite in place (one last-write-wins row), not error or
/// duplicate.
#[test]
fn insert_embedding_replaces_a_repeated_note_id() {
    let store = open_store();
    let (id, _) = store
        .add_note("note", "N", "b", &[], &[], None, None)
        .unwrap();

    let dim = crate::embeddings::EMBEDDING_DIM;
    let mut first = vec![0f32; dim];
    first[0] = 1.0;
    let mut second = vec![0f32; dim];
    second[5] = 1.0;

    store
        .insert_embedding(id, &crate::embeddings::vec_to_blob(&first))
        .expect("first note embedding");
    store
        .insert_embedding(id, &crate::embeddings::vec_to_blob(&second))
        .expect("second note embedding (replace)");

    let count: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM note_embeddings WHERE note_id = ?1",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "a repeated note_id must leave exactly one row");

    let stored: Vec<u8> = store
        .conn
        .query_row(
            "SELECT embedding FROM note_embeddings WHERE note_id = ?1",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        stored,
        crate::embeddings::vec_to_blob(&second),
        "the second embedding must overwrite the first"
    );
}

/// Replacing a `note_id` that has never been embedded must be a harmless
/// no-op DELETE followed by a normal INSERT, not an error — the common case
/// of embedding a note for the first time.
#[test]
fn insert_embedding_of_nonexistent_note_id_is_a_harmless_delete_no_op() {
    let store = open_store();
    let (id, _) = store
        .add_note("note", "N", "b", &[], &[], None, None)
        .unwrap();

    let dim = crate::embeddings::EMBEDDING_DIM;
    let mut vector = vec![0f32; dim];
    vector[7] = 1.0;

    store
        .insert_embedding(id, &crate::embeddings::vec_to_blob(&vector))
        .expect("embedding a never-before-embedded note must succeed");

    let count: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM note_embeddings WHERE note_id = ?1",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 1,
        "the first embed for a fresh note must land exactly once"
    );
}

/// The strongest test of "joins the existing transaction" vs. "just happens
/// not to error": call `insert_embedding` for a repeated `note_id` from
/// WITHIN a transaction the caller already opened, then roll that outer
/// transaction back. If the delete+insert genuinely joined the caller's
/// transaction, rolling it back must undo both halves, restoring the
/// pre-transaction row exactly.
#[test]
fn insert_embedding_joins_callers_transaction_and_rolls_back_with_it() {
    let store = open_store();
    let (id, _) = store
        .add_note("note", "N", "b", &[], &[], None, None)
        .unwrap();

    let dim = crate::embeddings::EMBEDDING_DIM;
    let mut first = vec![0f32; dim];
    first[0] = 1.0;
    store
        .insert_embedding(id, &crate::embeddings::vec_to_blob(&first))
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
            .insert_embedding(id, &crate::embeddings::vec_to_blob(&second))
            .expect("replacing inside the caller's open transaction must not nest a BEGIN");

        tx.rollback().expect("roll back the outer transaction");
    }

    let count: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM note_embeddings WHERE note_id = ?1",
            rusqlite::params![id],
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
            rusqlite::params![id],
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
fn embed(store: &MemoryStore, note_id: i64) {
    let blob = crate::embeddings::vec_to_blob(&[0.1f32; 896]);
    store
        .insert_embedding(note_id, &blob)
        .expect("insert embedding");
}

fn missing_ids(store: &MemoryStore, include_archived: bool) -> Vec<i64> {
    store
        .notes_missing_embeddings(include_archived)
        .expect("query missing")
        .into_iter()
        .map(|(id, ..)| id)
        .collect()
}

// Seed one active note, return its id.
fn add_active(store: &MemoryStore, title: &str) -> i64 {
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
    embed(&store, b);

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
    embed(&store, embedded);
    let unembedded = add_active(&store, "active-unembedded");
    let archived = add_active(&store, "archived-unembedded");
    assert!(store.archive(archived).expect("archive"));

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

    // None embedded: both returned in id order.
    assert_eq!(missing_ids(&store, false), vec![a, b]);

    // All embedded: empty.
    embed(&store, a);
    embed(&store, b);
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
    assert!(store.archive(archived).expect("archive"));

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

    assert_eq!(missing_ids(&store, false), vec![a, b]);
    embed(&store, a);
    assert_eq!(
        missing_ids(&store, false),
        vec![b],
        "an embedded note drops out of notes_missing_embeddings"
    );

    // The force-path query returns every active note regardless of embedding.
    let force: Vec<i64> = store
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

// ── D5: 768→896 migration flags the re-embed need once ───────────────────────

// Build a genuine pre-0.9 store on disk: a `note_embeddings` vec0 table declared
// FLOAT[768] with N notes and NO `schema_v896_note_embeddings` sentinel, exactly
// what an upgraded user's store looks like before the first 0.9 open.
fn make_pre_v896_store(path: &std::path::Path, n: usize) {
    register_sqlite_vec();
    let conn = rusqlite::Connection::open(path).expect("open raw pre-v896 store");
    conn.execute_batch(
        "CREATE TABLE notes (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            kind          TEXT    NOT NULL DEFAULT 'note',
            title         TEXT    NOT NULL,
            body          TEXT    NOT NULL,
            tags          TEXT,
            linked_files  TEXT,
            created_at    INTEGER NOT NULL DEFAULT (unixepoch())
        );
        CREATE VIRTUAL TABLE note_embeddings USING vec0(
            note_id INTEGER PRIMARY KEY, embedding FLOAT[768]
        );",
    )
    .expect("create pre-v896 schema");
    for i in 0..n {
        conn.execute(
            "INSERT INTO notes (kind, title, body) VALUES ('note', ?1, ?2)",
            rusqlite::params![format!("t{i}"), format!("b{i}")],
        )
        .expect("seed note");
    }
}

#[test]
fn open_after_768_upgrade_flags_reembed_count_once() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let path = tmp.path().join("memory.db");
    make_pre_v896_store(&path, 3);

    let store = MemoryStore::open(&path).expect("open upgrades 768→896");
    assert_eq!(
        store.reembed_needed,
        Some(3),
        "the drop must flag all 3 prior notes as needing re-embedding"
    );
    assert_eq!(
        store.notes_missing_embeddings(false).expect("query").len(),
        3,
        "after the drop every prior note is present-but-unembedded"
    );

    // The sentinel is now set: a second open must NOT re-flag (the notice fires
    // once, not on every command), and the notes stay present-but-unembedded.
    drop(store);
    let reopened = MemoryStore::open(&path).expect("reopen");
    assert_eq!(
        reopened.reembed_needed, None,
        "a store already at v896 must not re-flag the re-embed need"
    );
    assert_eq!(
        reopened
            .notes_missing_embeddings(false)
            .expect("query")
            .len(),
        3,
        "the notes are still unembedded until reindex runs"
    );
}

#[test]
fn open_fresh_store_does_not_flag_reembed() {
    let store = open_store();
    assert_eq!(
        store.reembed_needed, None,
        "a fresh FLOAT[896] store never triggered the 768 drop"
    );
}
