// ADR-097 schema step 18: a schema-17 index with real chunks, an embedding
// and stale graph edges migrates to 18 in place, and the very next `inkentry
// index` run re-extracts every file's edges (not only changed ones) without
// touching chunks or embeddings, then clears the owed-reextraction marker.

mod plumbing_helpers;

use plumbing_helpers::{inkentry_bin_in, isolate_git_config, register_sqlite_vec};
use rusqlite::Connection;
use tempfile::TempDir;

const REPO_SRC: &str =
    "pub fn helper() -> i32 {\n    1\n}\n\npub fn caller() -> i32 {\n    helper()\n}\n";

// Build a schema-17 `index.db` (the frozen shape `index_001_initial.sql`
// creates) already holding one file's chunks, one embedding and one
// deliberately stale graph edge, and stamp it at version 17 — the shape a
// real 1.0/1.1 index has by the time schema step 18 reaches it.
fn seed_schema_17_index(db_path: &std::path::Path) {
    let conn = Connection::open(db_path).expect("open legacy db");
    conn.execute_batch(include_str!(
        "../../inkentry-core/migrations/index_001_initial.sql"
    ))
    .expect("create v17 schema");

    let hash = format!("{}", blake3::hash(REPO_SRC.as_bytes()));
    conn.execute(
        "INSERT INTO files (path, language, hash, indexed_at, mtime) \
         VALUES ('src/lib.rs', 'rust', ?1, 0, 0)",
        rusqlite::params![hash],
    )
    .expect("insert file");
    let file_id = conn.last_insert_rowid();

    for name in ["helper", "caller"] {
        conn.execute(
            "INSERT INTO chunks (file_id, node_type, name, start_line, end_line, content) \
             VALUES (?1, 'function', ?2, 1, 3, ?2)",
            rusqlite::params![file_id, name],
        )
        .expect("insert chunk");
    }
    let caller_chunk_id: i64 = conn
        .query_row("SELECT id FROM chunks WHERE name = 'caller'", [], |r| {
            r.get(0)
        })
        .expect("read caller chunk id");

    let blob = inkentry_core::embeddings::vec_to_int8_blob(&vec![
        0.1f32;
        inkentry_core::embeddings::EMBEDDING_DIM
    ]);
    conn.execute(
        "INSERT INTO embeddings (chunk_id, embedding) VALUES (?1, vec_int8(?2))",
        rusqlite::params![caller_chunk_id, blob],
    )
    .expect("insert embedding");

    // Deliberately wrong: a real extraction of REPO_SRC finds `caller` calls
    // `helper`, never `stale_fn`. Left as-is, this row would prove nothing
    // about whether the pass actually re-extracted rather than left old rows
    // alone.
    conn.execute(
        "INSERT INTO graph_edges (source_file, source_name, target_name, kind, line) \
         VALUES ('src/lib.rs', 'caller', 'stale_fn', 'calls', 1)",
        [],
    )
    .expect("insert stale edge");

    conn.execute_batch("PRAGMA user_version = 17").unwrap();
}

#[test]
fn a_populated_schema_17_index_migrates_and_the_next_index_run_refreshes_its_edges_and_clears_the_marker()
 {
    register_sqlite_vec();
    isolate_git_config();

    let tmp = TempDir::new().expect("create temp dir");
    let project_dir = tmp.path().join("project");
    std::fs::create_dir_all(project_dir.join("src")).expect("create src dir");
    std::fs::write(project_dir.join("src/lib.rs"), REPO_SRC).expect("write src/lib.rs");

    let db_dir = tmp.path().join("db");
    std::fs::create_dir_all(&db_dir).expect("create db dir");
    let db_path = db_dir.join("index.db");
    seed_schema_17_index(&db_path);

    inkentry_bin_in(tmp.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("index")
        .arg(&project_dir)
        .arg("--db")
        .arg(&db_path)
        .assert()
        .success();

    let conn = Connection::open(&db_path).expect("reopen migrated db");
    let version: i32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 18, "the index must migrate to the current schema");

    let chunk_count: i64 = conn
        .query_row("SELECT count(*) FROM chunks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        chunk_count, 2,
        "an unchanged (hash-matching) file's chunks must not be rewritten"
    );
    let embedding_count: i64 = conn
        .query_row("SELECT count(*) FROM embeddings", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        embedding_count, 1,
        "the pre-existing embedding must survive — recomputing it is what this \
         migration step exists to avoid"
    );

    let stale_still_present: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM graph_edges WHERE target_name = 'stale_fn')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        !stale_still_present,
        "the graph-only pass must replace stale edges, not leave them behind"
    );
    let real_edge_present: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM graph_edges \
             WHERE source_name = 'caller' AND target_name = 'helper' AND kind = 'calls')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        real_edge_present,
        "the graph-only pass must extract the real caller -> helper edge from source"
    );

    let marker_cleared: bool = conn
        .query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM index_meta WHERE key = 'graph_edges_reextract')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        marker_cleared,
        "the reextraction marker must be cleared once the pass has run"
    );
}
