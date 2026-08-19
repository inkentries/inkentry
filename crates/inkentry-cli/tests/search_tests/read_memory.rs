//! Component tests for `inkentry plumbing read-memory`.

use crate::plumbing_helpers;
use plumbing_helpers::{inkentry_bin, inkentry_cmd, parse_jsonl, write_config};

use predicates::prelude::*;
use tempfile::TempDir;

// ── helpers ───────────────────────────────────────────────────────────────────

/// Index the fixture project and add a single memory note, both backed by the
/// same mock embedding server.  Returns `(TempDir, db_path, config_path)`.
fn indexed_project_with_memory_note() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf)
{
    use std::path::Path;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");

    let rt = tokio::runtime::Runtime::new().unwrap();
    let _server = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{ "embedding": vec![0.1f32; 896], "index": 0 }],
                "model": "test-model",
                "object": "list",
                "usage": { "prompt_tokens": 5, "total_tokens": 5 }
            })))
            .mount(&server)
            .await;
        server
    });

    let mock_url = _server.uri();
    let config_path = write_config(tmp.path(), &db_path, &mock_url);

    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/simple-project");

    // Index the fixture project. INKENTRY_NO_SERVER=1 forces offline so the embed
    // phase is skipped — these plumbing tests only need parsed chunks + a memory
    // note, not embeddings, and without it the index would auto-discover a
    // loopback inkentry-server on 127.0.0.1:7777 and fail on a dim mismatch.
    inkentry_bin()
        .env("INKENTRY_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg("--db")
        .arg(&db_path)
        .arg(&fixture)
        .assert()
        .success();

    // The memory DB lives next to the main DB (db_path.with_file_name("memory.db")).
    // We must pass it explicitly to `memory add --db` so both commands use the
    // same file (otherwise `memory add` would resolve from CWD and find the
    // workspace's .inkentry/memory.db instead).
    let mem_path = db_path.with_file_name("memory.db");

    inkentry_bin()
        // The git-notes carrier follows the process CWD and ignores `--db`, so
        // seeding from the repo under test would write the fixture into its
        // real notes ref.
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("--db")
        .arg(&mem_path)
        .arg("add")
        .arg("--kind")
        .arg("note")
        .arg("--title")
        .arg("Test note for plumbing tests")
        .arg("--body")
        .arg("body content here")
        .assert()
        .success();

    (tmp, db_path, config_path)
}

// ── happy path: list all ──────────────────────────────────────────────────────

#[test]
fn read_memory_emits_jsonl_when_notes_exist() {
    let (_tmp, db_path, config_path) = indexed_project_with_memory_note();

    let output = inkentry_cmd(&db_path, &config_path)
        .arg("read-memory")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows = parse_jsonl(&output);
    assert!(!rows.is_empty(), "expected at least one memory note");

    for row in &rows {
        assert!(row.get("id").is_some(), "missing 'id': {row}");
        assert!(row.get("kind").is_some(), "missing 'kind': {row}");
        assert!(row.get("title").is_some(), "missing 'title': {row}");
        assert!(row.get("body").is_some(), "missing 'body': {row}");
    }
}

// ── happy path: filter by kind ────────────────────────────────────────────────

#[test]
fn read_memory_kind_filter_returns_matching_notes() {
    let (_tmp, db_path, config_path) = indexed_project_with_memory_note();

    let output = inkentry_cmd(&db_path, &config_path)
        .arg("read-memory")
        .arg("--kind")
        .arg("note")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows = parse_jsonl(&output);
    assert!(!rows.is_empty(), "expected at least one 'note' kind entry");
    for row in &rows {
        assert_eq!(
            row["kind"].as_str().unwrap_or(""),
            "note",
            "kind filter should only return 'note' entries"
        );
    }
}

// ── happy path: fetch by id ───────────────────────────────────────────────────

#[test]
fn read_memory_by_id_returns_single_note() {
    let (_tmp, db_path, config_path) = indexed_project_with_memory_note();

    // List all to find an id.
    let list_output = inkentry_cmd(&db_path, &config_path)
        .arg("read-memory")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows = parse_jsonl(&list_output);
    let first_id = rows[0]["id"].as_str().expect("id should be a string");
    assert!(
        uuid::Uuid::parse_str(first_id).is_ok(),
        "an emitted id must be a UUID, got {first_id:?}"
    );

    let output = inkentry_cmd(&db_path, &config_path)
        .arg("read-memory")
        .arg("--id")
        .arg(first_id)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let detail_rows = parse_jsonl(&output);
    assert_eq!(detail_rows.len(), 1, "expected exactly one note for --id");
    assert_eq!(detail_rows[0]["id"].as_str(), Some(first_id));
}

// ── no results (exit 1) ───────────────────────────────────────────────────────

#[test]
fn read_memory_exits_1_when_no_notes_of_kind() {
    let (_tmp, db_path, config_path) = indexed_project_with_memory_note();

    // 'handoff' kind was not added in the setup above.
    inkentry_cmd(&db_path, &config_path)
        .arg("read-memory")
        .arg("--kind")
        .arg("handoff")
        .assert()
        .code(1);
}

#[test]
fn read_memory_exits_1_for_nonexistent_id() {
    let (_tmp, db_path, config_path) = indexed_project_with_memory_note();

    inkentry_cmd(&db_path, &config_path)
        .arg("read-memory")
        .arg("--id")
        .arg("999999")
        .assert()
        .code(1);
}

// ── error path: missing store ─────────────────────────────────────────────────

// An absent store is not an empty one, so this is exit 2 and not the exit 1
// that means "no entries". The diagnostic names the memory store rather than
// the index: `read-memory` never reads a chunk and no longer takes the
// project's identity from `index.db`.
#[test]
fn read_memory_exits_nonzero_when_the_memory_store_is_missing() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("config.toml");
    let db_path = tmp.path().join("nonexistent.db");

    std::fs::write(
        &config_path,
        format!("db_path = {:?}\nllm_model = \"x\"\n", db_path),
    )
    .unwrap();

    inkentry_bin()
        .arg("--config")
        .arg(&config_path)
        .arg("plumbing")
        .arg("--db")
        .arg(&db_path)
        .arg("read-memory")
        .assert()
        .failure()
        .stderr(predicate::str::contains("No memory store found"));
}
