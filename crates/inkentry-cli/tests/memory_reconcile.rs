mod plumbing_helpers;
use plumbing_helpers::{inkentry_bin, inkentry_bin_in, mount_health, mount_index_embed};

use assert_cmd::Command;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tempfile::TempDir;
use wiremock::MockServer;

// Must run before any `Connection::open` on a memory.db (it holds the `note_embeddings` vec0 table).
fn ensure_sqlite_vec() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        #[allow(clippy::missing_transmute_annotations)]
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}

// Creates `<dir>/.inkentry/`: `memory reconcile` fails closed without a project, and memory
// resolves there regardless of `db_path`, which is ignored.
fn write_config(dir: &Path, _db_path: &Path) -> (PathBuf, PathBuf) {
    let inkentry_dir = dir.join(".inkentry");
    std::fs::create_dir_all(&inkentry_dir).expect("create .inkentry");
    let index_db = inkentry_dir.join("index.db");
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "db_path = {:?}\nllm_model = \"test-model\"\n",
            index_db.display().to_string()
        ),
    )
    .expect("write config");
    let mem_path = index_db.with_file_name("memory.db");
    (config_path, mem_path)
}

// WAL mode so the CLI can open it read-only with `PRAGMA journal_mode=WAL`.
fn create_server_db(dir: &Path, slug: &str) -> (PathBuf, i64) {
    let path = dir.join("server.db");
    let conn = Connection::open(&path).expect("open server.db");

    conn.execute_batch("PRAGMA journal_mode=WAL;")
        .expect("set WAL");

    // Mirrors inkentry-server/migrations/server_001.sql.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS projects (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            slug          TEXT    NOT NULL UNIQUE,
            embedding_dim INTEGER NOT NULL DEFAULT 0,
            created_at    INTEGER NOT NULL DEFAULT (unixepoch())
         );
         CREATE TABLE IF NOT EXISTS notes (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            project_id    INTEGER NOT NULL REFERENCES projects(id),
            kind          TEXT    NOT NULL DEFAULT 'note',
            title         TEXT    NOT NULL,
            body          TEXT    NOT NULL,
            tags          TEXT,
            linked_files  TEXT,
            created_at    INTEGER NOT NULL DEFAULT (unixepoch()),
            status        TEXT    NOT NULL DEFAULT 'active',
            superseded_by INTEGER REFERENCES notes(id)
         );
         CREATE INDEX IF NOT EXISTS idx_notes_project ON notes(project_id);",
    )
    .expect("create server schema");

    conn.execute(
        "INSERT INTO projects (slug) VALUES (?1)",
        rusqlite::params![slug],
    )
    .expect("insert project");
    let project_id = conn.last_insert_rowid();

    (path, project_id)
}

#[allow(clippy::too_many_arguments)]
fn insert_server_note(
    conn: &Connection,
    project_id: i64,
    kind: &str,
    title: &str,
    body: &str,
    tags: Option<&str>,
    linked_files: Option<&str>,
    created_at: i64,
    status: &str,
    superseded_by: Option<i64>,
) -> i64 {
    conn.execute(
        "INSERT INTO notes \
         (project_id, kind, title, body, tags, linked_files, created_at, status, superseded_by) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            project_id,
            kind,
            title,
            body,
            tags,
            linked_files,
            created_at,
            status,
            superseded_by,
        ],
    )
    .expect("insert server note");
    conn.last_insert_rowid()
}

fn count_memory_notes(mem_path: &Path) -> i64 {
    ensure_sqlite_vec();
    let conn = Connection::open(mem_path).expect("open memory.db");
    conn.query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))
        .unwrap_or(0)
}

// Reads the first note only; callers have already asserted there is exactly one.
fn mem_tags_and_files(mem_path: &Path) -> (String, String) {
    ensure_sqlite_vec();
    let conn = Connection::open(mem_path).expect("open memory.db");
    let uuid: String = conn
        .query_row("SELECT uuid FROM notes LIMIT 1", [], |r| r.get(0))
        .expect("one note");
    let read = |table: &str, column: &str| -> String {
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {column} FROM {table} WHERE note_uuid = ?1 ORDER BY {column}"
            ))
            .expect("prepare");
        let rows: Vec<String> = stmt
            .query_map(rusqlite::params![uuid], |r| r.get(0))
            .expect("query")
            .collect::<rusqlite::Result<_>>()
            .expect("collect");
        rows.join(",")
    };
    (read("note_tags", "tag"), read("note_files", "path"))
}

fn read_memory_notes(mem_path: &Path) -> Vec<(String, String, String)> {
    ensure_sqlite_vec();
    let conn = Connection::open(mem_path).expect("open memory.db");
    let mut stmt = conn
        .prepare("SELECT kind, title, status FROM notes ORDER BY created_at ASC")
        .expect("prepare");
    stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })
    .expect("query")
    .collect::<rusqlite::Result<Vec<_>>>()
    .expect("collect")
}

// memory.db resolves from the config's `db_path`, not `memory --db`: that global arg would
// collide with the reconcile source path (hence `--source-db`). The cwd is the temp dir so
// `find_project_db()` cannot walk up into the repo and write to its real memory.db.
fn reconcile_cmd(config_path: &Path, server_db: &Path) -> Command {
    let tmp_dir = config_path
        .parent()
        .expect("config_path must have a parent");
    let mut cmd = inkentry_bin();
    cmd.current_dir(tmp_dir)
        .env("INKENTRY_NO_SERVER", "1")
        .env("INKENTRY_NO_RECONCILE_NUDGE", "1")
        .arg("--config")
        .arg(config_path)
        .arg("memory")
        .arg("reconcile")
        .arg("--source-db")
        .arg(server_db);
    cmd
}

#[test]
fn noop_when_server_db_absent() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");
    let (config_path, _mem_path) = write_config(tmp.path(), &db_path);
    let missing_server_db = tmp.path().join("nonexistent_server.db");

    reconcile_cmd(&config_path, &missing_server_db)
        .assert()
        .success();
}

#[test]
fn noop_when_server_db_absent_json_output_is_valid() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");
    let (config_path, _mem_path) = write_config(tmp.path(), &db_path);
    let missing_server_db = tmp.path().join("nonexistent_server.db");

    let output = reconcile_cmd(&config_path, &missing_server_db)
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = String::from_utf8(output).unwrap();
    let value: serde_json::Value =
        serde_json::from_str(text.trim()).expect("stdout should be valid JSON when --format json");
    assert_eq!(
        value["candidates"].as_i64(),
        Some(0),
        "candidates should be 0 when server.db absent"
    );
    assert_eq!(
        value["imported"].as_i64(),
        Some(0),
        "imported should be 0 when server.db absent"
    );
}

#[test]
fn inkentry_no_server_exits_cleanly_with_import() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");
    let (config_path, mem_path) = write_config(tmp.path(), &db_path);

    let slug = "test-project";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);
    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "decision",
        "Use SQLite for storage",
        "SQLite is the right choice because it is zero-infrastructure.",
        None,
        None,
        1_700_000_000,
        "active",
        None,
    );
    drop(conn);

    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();

    assert_eq!(
        count_memory_notes(&mem_path),
        1,
        "note should be imported even without embedding server"
    );
}

fn start_mock() -> (tokio::runtime::Runtime, MockServer) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let server = rt.block_on(async {
        let server = MockServer::start().await;
        mount_health(&server).await;
        mount_index_embed(&server).await;
        server
    });
    (rt, server)
}

#[test]
fn local_first_with_server_url_still_embeds_via_loopback() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");
    let (config_path, mem_path) = write_config(tmp.path(), &db_path);

    let slug = "loopback-embed-test";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);
    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "decision",
        "Prefer local embedding",
        "body one",
        None,
        None,
        1_700_005_000,
        "active",
        None,
    );
    drop(conn);

    let (_rt, mock) = start_mock();
    let state_dir = tmp.path().join("state");
    std::fs::create_dir_all(&state_dir).expect("create state dir");
    // Discovery reaches the mock via the fixed-port fallback's test override; the
    // `server.port` file is only trusted for a live `inkentry-server` process.
    let discovery_port = mock
        .uri()
        .rsplit(':')
        .next()
        .expect("uri has a port")
        .trim_end_matches('/')
        .to_string();

    let output = reconcile_cmd(&config_path, &server_db)
        .env_remove("INKENTRY_NO_SERVER")
        .env("INKENTRY_STATE_DIR", &state_dir)
        .env("INKENTRY_TEST_DISCOVERY_PORT", &discovery_port)
        // Unroutable: an accidental fallback to it must surface as a connection error,
        // not a silent unembedded import.
        .env("INKENTRY_SERVER_URL", "https://cloud.invalid.example:1")
        .env("INKENTRY_PROJECT_ID", slug)
        .arg("--all-projects")
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = String::from_utf8(output).unwrap();
    let value: serde_json::Value =
        serde_json::from_str(text.trim()).expect("stdout must be valid JSON");

    assert_eq!(value["imported"].as_i64(), Some(1));
    assert_eq!(
        value["imported_without_embedding"].as_i64(),
        Some(0),
        "local_first must embed via the loopback server even with an explicit \
         (and unroutable) server_url configured: {value}"
    );
    assert_eq!(count_memory_notes(&mem_path), 1);
}

#[test]
fn dedup_by_content_hash_not_rowid() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");
    let (config_path, mem_path) = write_config(tmp.path(), &db_path);

    let slug = "dedup-project";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);

    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "note",
        "First note",
        "body of first note",
        Some("tag-a"),
        None,
        1_700_000_001,
        "active",
        None,
    );
    insert_server_note(
        &conn,
        project_id,
        "note",
        "Second note",
        "body of second note",
        None,
        None,
        1_700_000_002,
        "active",
        None,
    );
    drop(conn);

    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();
    assert_eq!(
        count_memory_notes(&mem_path),
        2,
        "both notes should be imported on first run"
    );

    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();
    assert_eq!(
        count_memory_notes(&mem_path),
        2,
        "second run must not duplicate existing notes"
    );
}

#[test]
fn dedup_ignores_rowid_changes() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");
    let (config_path, mem_path) = write_config(tmp.path(), &db_path);

    let slug = "rowid-test";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);

    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "decision",
        "Always use UTC",
        "Timezones cause bugs. Use UTC everywhere.",
        None,
        None,
        1_700_000_100,
        "active",
        None,
    );
    drop(conn);

    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();
    assert_eq!(count_memory_notes(&mem_path), 1);

    let conn = Connection::open(&server_db).unwrap();
    conn.execute(
        "DELETE FROM notes WHERE project_id = ?1",
        rusqlite::params![project_id],
    )
    .unwrap();
    insert_server_note(
        &conn,
        project_id,
        "decision",
        "Always use UTC",
        "Timezones cause bugs. Use UTC everywhere.",
        None,
        None,
        1_700_000_100, // same created_at
        "active",
        None,
    );
    drop(conn);

    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();
    assert_eq!(
        count_memory_notes(&mem_path),
        1,
        "content-identical note with new rowid must not be re-imported"
    );
}

#[allow(clippy::too_many_arguments)]
fn insert_server_note_with_id(
    conn: &Connection,
    id: i64,
    project_id: i64,
    title: &str,
    body: &str,
    created_at: i64,
    status: &str,
    superseded_by: Option<i64>,
) {
    conn.execute(
        "INSERT INTO notes \
         (id, project_id, kind, title, body, tags, linked_files, created_at, status, superseded_by) \
         VALUES (?1, ?2, 'decision', ?3, ?4, NULL, NULL, ?5, ?6, ?7)",
        rusqlite::params![id, project_id, title, body, created_at, status, superseded_by],
    )
    .expect("insert server note with id");
}

#[test]
fn supersede_edge_resolves_across_differing_ids() {
    // An id-positional edge would break twice here: the source rows sit at server ids
    // 101/102 while memory.db mints its own, and the already-imported earlier note shifts
    // the pair's position between candidates and import set. entity_id resolution must survive both.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");
    let (config_path, mem_path) = write_config(tmp.path(), &db_path);

    let slug = "supersede-renumber";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);

    let conn = Connection::open(&server_db).unwrap();
    insert_server_note_with_id(
        &conn,
        100,
        project_id,
        "Unrelated earlier note",
        "already imported",
        1_700_000_100,
        "active",
        None,
    );
    drop(conn);
    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();
    assert_eq!(
        count_memory_notes(&mem_path),
        1,
        "phase 1 imported one note"
    );

    let conn = Connection::open(&server_db).unwrap();
    // Successor first: `superseded_by` is a FK, so 102 must exist before 101 can
    // reference it; import order follows created_at.
    insert_server_note_with_id(
        &conn,
        102,
        project_id,
        "New approach",
        "successor body",
        1_700_000_501,
        "active",
        None,
    );
    insert_server_note_with_id(
        &conn,
        101,
        project_id,
        "Old approach",
        "superseded body",
        1_700_000_500,
        "archived",
        Some(102), // → server rowid of the successor
    );
    drop(conn);

    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();

    let mem = Connection::open(&mem_path).unwrap();
    let (old_id, old_succ): (String, Option<String>) = mem
        .query_row(
            "SELECT uuid, superseded_by FROM notes WHERE title = 'Old approach'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let new_id: String = mem
        .query_row(
            "SELECT uuid FROM notes WHERE title = 'New approach'",
            [],
            |r| r.get(0),
        )
        .unwrap();

    assert!(
        old_id != "101" && new_id != "102",
        "memory.db must have minted its own ids ({old_id}, {new_id}) — \
         otherwise this proves nothing"
    );
    assert_eq!(
        old_succ,
        Some(new_id),
        "the supersede edge must point at the successor's local id"
    );
}

#[test]
fn dedup_key_excludes_created_at() {
    // A second machine recording the same decision cannot reproduce the timestamp.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");
    let (config_path, mem_path) = write_config(tmp.path(), &db_path);

    let slug = "hash-ts-test";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);

    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "note",
        "Identical title",
        "Identical body",
        None,
        None,
        1_700_000_001,
        "active",
        None,
    );
    insert_server_note(
        &conn,
        project_id,
        "note",
        "Identical title",
        "Identical body",
        None,
        None,
        1_700_000_002, // different created_at
        "active",
        None,
    );
    drop(conn);

    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();

    assert_eq!(
        count_memory_notes(&mem_path),
        1,
        "identical text at different times is one entity"
    );

    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();
    assert_eq!(count_memory_notes(&mem_path), 1, "re-run imports nothing");
}

#[test]
fn dedup_key_excludes_tags_which_union_on_collapse() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");
    let (config_path, mem_path) = write_config(tmp.path(), &db_path);

    let slug = "tag-union-test";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);

    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "decision",
        "Union title",
        "Union body",
        Some("alpha"),
        Some("a.rs"),
        1_700_000_001,
        "active",
        None,
    );
    insert_server_note(
        &conn,
        project_id,
        "decision",
        "Union title",
        "Union body",
        Some("beta"),
        Some("b.rs"),
        1_700_000_002,
        "active",
        None,
    );
    drop(conn);

    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();

    assert_eq!(count_memory_notes(&mem_path), 1, "one entity");

    let (tags, files) = mem_tags_and_files(&mem_path);
    for want in ["alpha", "beta"] {
        assert!(tags.contains(want), "tags {tags:?} must union {want}");
    }
    for want in ["a.rs", "b.rs"] {
        assert!(files.contains(want), "files {files:?} must union {want}");
    }
}

#[test]
fn collapse_onto_stored_row_unions_tags_rather_than_dropping_them() {
    // Tags/files are outside the key, so without the merge the losing copy's metadata
    // would be skipped silently as "already present".
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");
    let (config_path, mem_path) = write_config(tmp.path(), &db_path);

    let slug = "stored-union-test";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);

    let conn = Connection::open(&server_db).unwrap();
    let first_id = insert_server_note(
        &conn,
        project_id,
        "decision",
        "Union title",
        "Union body",
        Some("alpha"),
        Some("a.rs"),
        1_700_000_001,
        "active",
        None,
    );
    drop(conn);
    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();
    assert_eq!(count_memory_notes(&mem_path), 1, "pass 1 imports the entry");

    let conn = Connection::open(&server_db).unwrap();
    conn.execute(
        "DELETE FROM notes WHERE id = ?1",
        rusqlite::params![first_id],
    )
    .unwrap();
    insert_server_note(
        &conn,
        project_id,
        "decision",
        "Union title",
        "Union body",
        Some("beta"),
        Some("b.rs"),
        1_700_000_002,
        "active",
        None,
    );
    drop(conn);
    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();

    assert_eq!(
        count_memory_notes(&mem_path),
        1,
        "identical text stays one entity"
    );

    let (tags, files) = mem_tags_and_files(&mem_path);
    for want in ["alpha", "beta"] {
        assert!(
            tags.contains(want),
            "tags {tags:?} must keep {want} after collapsing onto the stored row"
        );
    }
    for want in ["a.rs", "b.rs"] {
        assert!(
            files.contains(want),
            "linked_files {files:?} must keep {want} after collapsing onto the stored row"
        );
    }
}

#[test]
fn dry_run_does_not_union_tags_into_a_stored_row() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");
    let (config_path, mem_path) = write_config(tmp.path(), &db_path);

    let slug = "dryrun-union-test";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);

    let conn = Connection::open(&server_db).unwrap();
    let first_id = insert_server_note(
        &conn,
        project_id,
        "decision",
        "Union title",
        "Union body",
        Some("alpha"),
        None,
        1_700_000_001,
        "active",
        None,
    );
    drop(conn);
    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();

    let conn = Connection::open(&server_db).unwrap();
    conn.execute(
        "DELETE FROM notes WHERE id = ?1",
        rusqlite::params![first_id],
    )
    .unwrap();
    insert_server_note(
        &conn,
        project_id,
        "decision",
        "Union title",
        "Union body",
        Some("beta"),
        None,
        1_700_000_002,
        "active",
        None,
    );
    drop(conn);

    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .arg("--dry-run")
        .assert()
        .success();

    let (tags, _files) = mem_tags_and_files(&mem_path);
    assert!(
        !tags.contains("beta"),
        "--dry-run must not merge tags; found {tags:?}"
    );
}

#[test]
fn json_counts_partition_the_source_rows() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");
    let (config_path, mem_path) = write_config(tmp.path(), &db_path);

    let slug = "partition-test";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);

    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "decision",
        "Stored",
        "stored body",
        None,
        None,
        1_700_000_001,
        "active",
        None,
    );
    drop(conn);
    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();
    assert_eq!(count_memory_notes(&mem_path), 1);

    let conn = Connection::open(&server_db).unwrap();
    for created_at in [1_700_000_010_i64, 1_700_000_011] {
        insert_server_note(
            &conn,
            project_id,
            "decision",
            "Twin",
            "twin body",
            None,
            None,
            created_at,
            "active",
            None,
        );
    }
    insert_server_note(
        &conn,
        project_id,
        "decision",
        "Fresh",
        "fresh body",
        None,
        None,
        1_700_000_012,
        "active",
        None,
    );
    drop(conn);

    let out = reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value =
        serde_json::from_slice(&out).expect("summary line must be valid JSON");

    let candidates = v["candidates"].as_i64().expect("candidates");
    let already = v["already_present"].as_i64().expect("already_present");
    let collapsed = v["collapsed_duplicates"]
        .as_i64()
        .expect("collapsed_duplicates");
    let imported = v["imported"].as_i64().expect("imported");

    assert_eq!(candidates, 4, "4 source rows: {v}");
    assert_eq!(already, 1, "the stored row is already present: {v}");
    assert_eq!(collapsed, 1, "the twin pair folds one row away: {v}");
    assert_eq!(imported, 2, "Twin (collapsed) and Fresh import: {v}");
    assert_eq!(
        candidates,
        already + collapsed + imported,
        "counts must partition the source rows exactly: {v}"
    );
    assert_eq!(count_memory_notes(&mem_path), 3, "Stored + Twin + Fresh");
}

#[test]
fn tag_reorder_does_not_reimport() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");
    let (config_path, mem_path) = write_config(tmp.path(), &db_path);

    let slug = "normalize-tags";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);

    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "note",
        "Tag normalization test",
        "body",
        Some("beta, alpha"),
        None,
        1_700_000_200,
        "active",
        None,
    );
    drop(conn);

    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();
    assert_eq!(count_memory_notes(&mem_path), 1);

    let conn = Connection::open(&server_db).unwrap();
    conn.execute(
        "UPDATE notes SET tags = 'alpha,beta' WHERE project_id = ?1",
        rusqlite::params![project_id],
    )
    .unwrap();
    drop(conn);

    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();
    assert_eq!(
        count_memory_notes(&mem_path),
        1,
        "reordering tags must not re-import"
    );
}

#[test]
fn server_db_not_modified_after_reconcile() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");
    let (config_path, _mem_path) = write_config(tmp.path(), &db_path);

    let slug = "readonly-test";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);

    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "requirement",
        "Server must not be written",
        "body",
        None,
        None,
        1_700_000_300,
        "active",
        None,
    );
    drop(conn);

    let count_before: i64 = {
        let conn = Connection::open(&server_db).unwrap();
        conn.query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))
            .unwrap()
    };

    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();

    let count_after: i64 = {
        let conn = Connection::open(&server_db).unwrap();
        conn.query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))
            .unwrap()
    };

    assert_eq!(
        count_before, count_after,
        "server.db note count must not change after reconcile"
    );
}

#[test]
fn server_db_opened_read_only_flag() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");
    let (config_path, mem_path) = write_config(tmp.path(), &db_path);

    let slug = "readonly-flag-test";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);

    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "note",
        "Read-only open",
        "body",
        None,
        None,
        1_700_000_400,
        "active",
        None,
    );
    drop(conn);

    let mut perms = std::fs::metadata(&server_db).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&server_db, perms).unwrap();

    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();

    // Restore permissions so temp dir cleanup can remove the file; PermissionsExt
    // avoids clippy::permissions_set_readonly_false.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o644);
        std::fs::set_permissions(&server_db, perms).unwrap();
    }
    #[cfg(not(unix))]
    {
        let mut perms = std::fs::metadata(&server_db).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(&server_db, perms).unwrap();
    }

    assert_eq!(count_memory_notes(&mem_path), 1);
}

#[test]
fn archived_rows_import_as_archived() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");
    let (config_path, mem_path) = write_config(tmp.path(), &db_path);

    let slug = "archived-test";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);

    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "decision",
        "Old approach - archived",
        "This approach was superseded.",
        None,
        None,
        1_700_000_500,
        "archived", // status in server.db
        None,
    );
    insert_server_note(
        &conn,
        project_id,
        "decision",
        "Current approach - active",
        "This approach is current.",
        None,
        None,
        1_700_000_501,
        "active",
        None,
    );
    drop(conn);

    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();

    let notes = read_memory_notes(&mem_path);
    assert_eq!(notes.len(), 2, "both notes should be imported");

    let archived: Vec<_> = notes
        .iter()
        .filter(|(_, title, _)| title.contains("archived"))
        .collect();
    assert_eq!(archived.len(), 1, "exactly one archived note expected");
    assert_eq!(
        archived[0].2, "archived",
        "archived note must have status='archived' in memory.db"
    );

    let active: Vec<_> = notes
        .iter()
        .filter(|(_, title, _)| title.contains("active"))
        .collect();
    assert_eq!(
        active[0].2, "active",
        "active note must have status='active'"
    );
}

#[test]
fn dry_run_does_not_write_to_memory_db() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");
    let (config_path, mem_path) = write_config(tmp.path(), &db_path);

    let slug = "dryrun-test";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);

    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "note",
        "Dry run note",
        "This note should not be written.",
        None,
        None,
        1_700_000_600,
        "active",
        None,
    );
    drop(conn);

    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .arg("--dry-run")
        .assert()
        .success();

    let written = if mem_path.exists() {
        count_memory_notes(&mem_path)
    } else {
        0
    };
    assert_eq!(
        written, 0,
        "--dry-run must not write any notes to memory.db"
    );
}

#[test]
fn dry_run_json_reports_would_import() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");
    let (config_path, _mem_path) = write_config(tmp.path(), &db_path);

    let slug = "dryrun-json-test";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);

    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "note",
        "Would-import note",
        "body",
        None,
        None,
        1_700_000_700,
        "active",
        None,
    );
    drop(conn);

    let output = reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .arg("--dry-run")
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = String::from_utf8(output).unwrap();
    let value: serde_json::Value =
        serde_json::from_str(text.trim()).expect("stdout should be valid JSON");

    assert!(
        value["would_import"].as_i64().unwrap_or(0) > 0,
        "would_import must be positive in dry-run mode: {value}"
    );
    assert_eq!(
        value["imported"].as_i64(),
        Some(0),
        "imported must be 0 in dry-run mode: {value}"
    );
}

#[test]
fn dry_run_on_empty_server_db_exits_zero() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");
    let (config_path, _mem_path) = write_config(tmp.path(), &db_path);

    let (server_db, _project_id) = create_server_db(tmp.path(), "empty-slug");

    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .arg("--dry-run")
        .assert()
        .success();
}

#[test]
fn rollback_on_mid_transaction_failure_leaves_no_partial_import() {
    // A BEFORE INSERT trigger aborts the 3rd note, so the batch fails mid-transaction
    // and must roll back.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");
    let (config_path, mem_path) = write_config(tmp.path(), &db_path);

    let slug = "rollback-test";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);

    let conn = Connection::open(&server_db).unwrap();
    for i in 0..3i64 {
        insert_server_note(
            &conn,
            project_id,
            "note",
            &format!("Rollback note {i}"),
            "body",
            None,
            None,
            1_700_001_000 + i,
            "active",
            None,
        );
    }
    drop(conn);

    // Bootstrap memory.db (full schema incl. `note_embeddings`) from a separate dir:
    // `create_server_db` always writes `server.db` and would overwrite the rollback one.
    {
        let boot_dir = tmp.path().join("boot");
        std::fs::create_dir_all(&boot_dir).unwrap();
        let boot_config_path = boot_dir.join("config.toml");
        let boot_db_path = boot_dir.join("inkentry.db");
        std::fs::write(
            &boot_config_path,
            format!(
                "db_path = {:?}\nllm_model = \"test-model\"\n",
                boot_db_path.display().to_string()
            ),
        )
        .unwrap();
        let boot_config_content = format!(
            "db_path = {:?}\nllm_model = \"test-model\"\n",
            tmp.path().join("inkentry.db").display().to_string()
        );
        std::fs::write(&boot_config_path, boot_config_content).unwrap();

        let (bootstrap_db, boot_pid) = create_server_db(&boot_dir, "boot-for-rollback");
        let bc = Connection::open(&bootstrap_db).unwrap();
        insert_server_note(
            &bc,
            boot_pid,
            "note",
            "Bootstrap note",
            "body",
            None,
            None,
            1_699_000_000,
            "active",
            None,
        );
        drop(bc);

        let mut boot_cmd = inkentry_bin();
        boot_cmd
            .current_dir(&boot_dir)
            .env("INKENTRY_NO_SERVER", "1")
            .env("INKENTRY_NO_RECONCILE_NUDGE", "1")
            .arg("--config")
            .arg(&boot_config_path)
            .arg("memory")
            .arg("reconcile")
            .arg("--source-db")
            .arg(&bootstrap_db)
            .arg("--all-projects")
            .assert()
            .success();

        assert!(
            mem_path.exists(),
            "memory.db must exist after bootstrap reconcile"
        );
    }

    {
        ensure_sqlite_vec();
        let mc = Connection::open(&mem_path).unwrap();
        mc.execute("DELETE FROM notes WHERE title = 'Bootstrap note'", [])
            .unwrap();
        mc.execute_batch(
            "CREATE TRIGGER IF NOT EXISTS sabotage_third_note \
             BEFORE INSERT ON notes \
             WHEN NEW.title = 'Rollback note 2' \
             BEGIN \
               SELECT RAISE(ABORT, 'sabotage: reject third note'); \
             END;",
        )
        .expect("install sabotage trigger");
    }

    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .failure();

    assert_eq!(
        count_memory_notes(&mem_path),
        0,
        "all inserts must be rolled back when mid-transaction failure occurs"
    );
}

#[test]
fn exit_0_on_success_import() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");
    let (config_path, _mem_path) = write_config(tmp.path(), &db_path);

    let slug = "exit-0-import";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);
    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "note",
        "Exit code note",
        "body",
        None,
        None,
        1_700_002_000,
        "active",
        None,
    );
    drop(conn);

    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();
}

#[test]
fn exit_0_on_noop_already_imported() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");
    let (config_path, _mem_path) = write_config(tmp.path(), &db_path);

    let slug = "exit-0-noop";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);
    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "note",
        "Existing note",
        "body",
        None,
        None,
        1_700_002_100,
        "active",
        None,
    );
    drop(conn);

    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();

    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();
}

#[test]
fn exit_0_on_no_rows_to_import() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");
    let (config_path, _mem_path) = write_config(tmp.path(), &db_path);

    let (server_db, _project_id) = create_server_db(tmp.path(), "empty-project");

    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();
}

#[test]
fn exit_nonzero_on_corrupt_server_db() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");
    let (config_path, _mem_path) = write_config(tmp.path(), &db_path);

    let corrupt_db = tmp.path().join("corrupt_server.db");
    std::fs::write(&corrupt_db, b"this is not a valid sqlite database file!!!")
        .expect("write corrupt db");

    reconcile_cmd(&config_path, &corrupt_db)
        .arg("--all-projects")
        .assert()
        .failure();
}

#[test]
fn json_summary_contains_expected_fields() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");
    let (config_path, _mem_path) = write_config(tmp.path(), &db_path);

    let slug = "json-fields-test";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);
    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "note",
        "JSON fields note",
        "body",
        None,
        None,
        1_700_003_000,
        "active",
        None,
    );
    drop(conn);

    let output = reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = String::from_utf8(output).unwrap();
    let value: serde_json::Value =
        serde_json::from_str(text.trim()).expect("stdout must be valid JSON");

    for field in &[
        "source_db",
        "project_slug",
        "candidates",
        "already_present",
        "imported",
        "would_import",
        "imported_without_embedding",
        "skipped_archived_supersede_unresolved",
        "errors",
    ] {
        assert!(
            value.get(field).is_some(),
            "JSON summary missing field '{field}': {value}"
        );
    }
}

#[test]
fn import_increments_count_correctly() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");
    let (config_path, _mem_path) = write_config(tmp.path(), &db_path);

    let slug = "count-test";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);
    let conn = Connection::open(&server_db).unwrap();
    for i in 0..3i64 {
        insert_server_note(
            &conn,
            project_id,
            "note",
            &format!("Note {i}"),
            "body",
            None,
            None,
            1_700_004_000 + i,
            "active",
            None,
        );
    }
    drop(conn);

    let output = reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let v1: serde_json::Value =
        serde_json::from_str(String::from_utf8(output).unwrap().trim()).unwrap();
    assert_eq!(v1["candidates"].as_i64(), Some(3));
    assert_eq!(v1["imported"].as_i64(), Some(3));
    assert_eq!(v1["already_present"].as_i64(), Some(0));

    let output2 = reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let v2: serde_json::Value =
        serde_json::from_str(String::from_utf8(output2).unwrap().trim()).unwrap();
    assert_eq!(v2["candidates"].as_i64(), Some(3));
    assert_eq!(v2["imported"].as_i64(), Some(0));
    assert_eq!(v2["already_present"].as_i64(), Some(3));
}

#[test]
fn sql_injection_payload_in_body_does_not_break_import() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");
    let (config_path, mem_path) = write_config(tmp.path(), &db_path);

    let slug = "sqli-test";
    let (server_db, project_id) = create_server_db(tmp.path(), slug);

    let injection_body = "'); DROP TABLE notes; --";
    let injection_title = "<script>alert('xss')</script>";
    let injection_tags = "tag1'; DELETE FROM notes; --";

    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "note",
        injection_title,
        injection_body,
        Some(injection_tags),
        None,
        1_700_005_000,
        "active",
        None,
    );
    drop(conn);

    reconcile_cmd(&config_path, &server_db)
        .arg("--all-projects")
        .assert()
        .success();

    ensure_sqlite_vec();
    let mem_conn = Connection::open(&mem_path).unwrap();
    let (title, body): (String, String) = mem_conn
        .query_row(
            "SELECT title, body FROM notes WHERE title = ?1",
            rusqlite::params![injection_title],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("injected note must be importable and table must still exist");

    assert_eq!(title, injection_title, "title stored verbatim");
    assert_eq!(body, injection_body, "body stored verbatim");
}

// The default source path must resolve through the same state-dir resolver that
// `server start` writes with; otherwise a daemon under an `INKENTRY_STATE_DIR` override
// is invisible and reconcile hits the "server.db absent" no-op instead of importing.
#[test]
fn default_source_db_honors_state_dir_override() {
    let home = TempDir::new().unwrap();
    let state_override = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let db_path = project.path().join("inkentry.db");
    let (config_path, mem_path) = write_config(project.path(), &db_path);

    let (server_db, project_id) = create_server_db(state_override.path(), "override-project");
    let conn = Connection::open(&server_db).unwrap();
    insert_server_note(
        &conn,
        project_id,
        "decision",
        "Use SQLite for storage",
        "SQLite is the right choice because it is zero-infrastructure.",
        None,
        None,
        1_700_000_000,
        "active",
        None,
    );
    drop(conn);

    let home_default = home
        .path()
        .join(".local")
        .join("state")
        .join("inkentry")
        .join("server.db");
    assert!(
        !home_default.exists(),
        "fixture bug: server.db must only exist under the override"
    );

    let mut cmd = inkentry_bin_in(home.path());
    cmd.current_dir(project.path())
        .env("INKENTRY_NO_SERVER", "1")
        .env("INKENTRY_NO_RECONCILE_NUDGE", "1")
        .env("INKENTRY_STATE_DIR", state_override.path())
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("reconcile")
        .arg("--all-projects");
    cmd.assert().success();

    assert_eq!(
        count_memory_notes(&mem_path),
        1,
        "reconcile must resolve server.db through INKENTRY_STATE_DIR when --source-db is omitted"
    );
}
