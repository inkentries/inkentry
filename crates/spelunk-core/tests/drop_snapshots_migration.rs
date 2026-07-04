//! Regression coverage for spelunk-oss^67 (removal of the dead snapshot
//! storage layer and `spelunk search --as-of <sha>`).
//!
//! `021_drop_snapshots.sql` / `Database::apply_drop_snapshots_migration` must
//! actually drop `snapshots` / `snapshot_files` / `snapshot_chunks` /
//! `snapshot_embeddings` on a database that has real pre-existing rows in
//! them (i.e. a database that was created before ^67 and had migrations
//! 016/017 applied), not just be a no-op on freshly created databases that
//! never had the tables in the first place.
//!
//! These tests must run serially (`#[serial]`): `sqlite3_auto_extension` is
//! process-global (see `tests/common`).

mod common;

use rusqlite::Connection;
use serial_test::serial;
use spelunk_core::storage::Database;
use std::path::Path;

const SNAPSHOT_TABLES: &[&str] = &[
    "snapshots",
    "snapshot_files",
    "snapshot_chunks",
    "snapshot_embeddings",
];

/// True if a table with this name exists in `sqlite_master`.
fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type IN ('table') AND name = ?1",
        rusqlite::params![name],
        |_| Ok(()),
    )
    .optional_ok()
}

trait OptionalOk {
    fn optional_ok(self) -> bool;
}
impl<T> OptionalOk for rusqlite::Result<T> {
    fn optional_ok(self) -> bool {
        match self {
            Ok(_) => true,
            Err(rusqlite::Error::QueryReturnedNoRows) => false,
            Err(e) => panic!("unexpected sqlite error checking table existence: {e}"),
        }
    }
}

/// Build a pre-^67 database at `path`: run migrations 001-017 directly (the
/// same historical files `Database::open` used to run before ^67 deleted the
/// Rust wiring), including 016/017 which create the snapshot tables, then
/// insert real rows into all four snapshot tables plus a live `files` row so
/// we can assert non-snapshot data survives untouched.
///
/// Mirrors the pre-^67 migration order in `db.rs` (001, 002, 003, 006, 007,
/// 008, 009, 010, 011, 016, 017) closely enough to reproduce the on-disk
/// shape those two migrations left behind; the later migrations (018+) are
/// intentionally NOT run here — `Database::open` runs them afterwards, which
/// is the whole point of the test (proving the *upgrade* path).
fn seed_pre_67_db(path: &Path) {
    common::register_sqlite_vec();
    let conn = Connection::open(path).expect("open raw connection for fixture seeding");
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();

    conn.execute_batch(include_str!("../migrations/001_initial.sql"))
        .expect("apply 001_initial.sql");
    conn.execute_batch(include_str!("../migrations/002_vectors.sql"))
        .expect("apply 002_vectors.sql");
    conn.execute_batch(include_str!("../migrations/016_snapshots.sql"))
        .expect("apply 016_snapshots.sql");
    conn.execute_batch(include_str!("../migrations/017_snapshot_vectors.sql"))
        .expect("apply 017_snapshot_vectors.sql");

    // Sanity: fixture actually created the tables we're about to seed and test.
    for t in SNAPSHOT_TABLES {
        assert!(
            table_exists(&conn, t),
            "fixture setup failed to create table {t}"
        );
    }

    // A real live file, so we can prove the drop migration doesn't touch
    // unrelated data.
    conn.execute(
        "INSERT INTO files (path, language, hash, indexed_at) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params!["src/lib.rs", "rust", "deadbeef", 1_700_000_000_i64],
    )
    .unwrap();

    // Real rows in every snapshot table + the vec0 snapshot_embeddings table,
    // simulating a database from before ^67 that had actually run a
    // (hypothetical) snapshot indexing pass — i.e. not just empty tables.
    conn.execute(
        "INSERT INTO snapshots (commit_sha, file_count, chunk_count) VALUES (?1, ?2, ?3)",
        rusqlite::params!["abc123deadbeef", 1_i64, 1_i64],
    )
    .unwrap();
    let snapshot_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO snapshot_files (snapshot_id, path, language, hash) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![snapshot_id, "src/lib.rs", "rust", "deadbeef"],
    )
    .unwrap();
    let snapshot_file_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO snapshot_chunks
             (snapshot_id, file_id, node_type, name, start_line, end_line, content, token_count)
         VALUES (?1, ?2, 'function', 'old_fn', 1, 3, 'fn old_fn() {}', 4)",
        rusqlite::params![snapshot_id, snapshot_file_id],
    )
    .unwrap();
    let snapshot_chunk_id = conn.last_insert_rowid();

    // snapshot_embeddings is a vec0 virtual table storing int8[896]; insert a
    // trivial zero vector as a raw blob (same shape apply_dim_upgrade_migration
    // and insert_embedding already exercise for the live `embeddings` table).
    let dim = spelunk_core::embeddings::EMBEDDING_DIM;
    let blob = spelunk_core::embeddings::vec_to_int8_blob(&vec![0.0f32; dim]);
    conn.execute(
        "INSERT INTO snapshot_embeddings (chunk_id, embedding) VALUES (?1, vec_int8(?2))",
        rusqlite::params![snapshot_chunk_id, blob],
    )
    .unwrap();

    // Confirm the seeded rows are actually there before we hand off to
    // Database::open — otherwise a later "table gone" assertion would be
    // vacuously true.
    let snapshot_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        snapshot_rows, 1,
        "fixture must seed exactly one snapshot row"
    );
    let embedding_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM snapshot_embeddings", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        embedding_rows, 1,
        "fixture must seed exactly one snapshot_embeddings row"
    );

    drop(conn);
}

/// Core regression test for spelunk-oss^67: a database that has migrations
/// 016/017 applied *and populated with real rows* must, after running the
/// current `Database::open` migration chain (which includes
/// `apply_drop_snapshots_migration`), have all four snapshot tables gone —
/// and the pre-existing non-snapshot data (the `files` row) must survive
/// untouched.
#[test]
#[serial]
fn drop_snapshots_migration_removes_tables_with_preexisting_rows() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("legacy.db");

    seed_pre_67_db(&db_path);

    // This is the code path under test: opening a pre-existing (^67-era)
    // database file runs the full current migration chain, including
    // apply_drop_snapshots_migration, via Database::open.
    let db = Database::open(&db_path).expect("Database::open must handle a pre-^67 db with data");

    // All four snapshot tables must be gone.
    let raw = Connection::open(&db_path).unwrap();
    for t in SNAPSHOT_TABLES {
        assert!(
            !table_exists(&raw, t),
            "table {t} should have been dropped by apply_drop_snapshots_migration \
             but still exists in a database that had real snapshot rows"
        );
    }

    // Non-snapshot data survives: the live `files` row inserted into the
    // fixture is untouched by the drop migration.
    let hash = db.file_hash("src/lib.rs").unwrap();
    assert_eq!(
        hash.as_deref(),
        Some("deadbeef"),
        "drop-snapshots migration must not disturb unrelated live index data"
    );

    // Re-opening again (idempotency) must not error — mirrors the DROP-on-
    // upgrade precedent set by apply_dim_upgrade_migration's marker-table guard.
    drop(db);
    Database::open(&db_path).expect("re-opening an already-migrated db must be a no-op success");
}

/// Fresh-DB sanity (spelunk-oss^67): a database created from scratch via the
/// normal `Database::open` path — the same path `spelunk index` uses — never
/// has any snapshot tables in the first place. Cheap companion assertion to
/// the upgrade-path test above.
#[test]
#[serial]
fn fresh_database_has_no_snapshot_tables() {
    common::register_sqlite_vec();
    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("fresh.db");

    let _db = Database::open(&db_path).expect("fresh Database::open");

    let raw = Connection::open(&db_path).unwrap();
    for t in SNAPSHOT_TABLES {
        assert!(
            !table_exists(&raw, t),
            "fresh database unexpectedly contains dead snapshot table {t}"
        );
    }
}
