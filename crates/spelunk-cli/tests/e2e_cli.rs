use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::tempdir;

mod plumbing_helpers;
use plumbing_helpers::{FIXTURE_PROJECT_ID, IndexEmbedResponder, write_config_with_server};

#[test]
fn test_help_output() {
    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Usage: spelunk [OPTIONS] <COMMAND>",
        ))
        .stdout(predicate::str::contains("Commands:"));
}

#[test]
fn test_invalid_command() {
    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.arg("nonexistent-command")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "error: unrecognized subcommand 'nonexistent-command'",
        ));
}

#[test]
fn test_languages_output() {
    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.arg("languages")
        .assert()
        .success()
        .stdout(predicate::str::contains("Supported languages:"))
        .stdout(predicate::str::contains("rust"))
        .stdout(predicate::str::contains("python"))
        .stdout(predicate::str::contains("javascript"));
}

#[test]
fn test_status_empty_project() {
    let temp = tempdir().unwrap();
    let config_path = temp.path().join("config.toml");
    // Pin db_path to a non-existent temp path so the test is machine-independent.
    let db_path = temp.path().join("nonexistent.db");
    fs::write(
        &config_path,
        format!(
            "llm_model = \"test-model\"\ndb_path = {:?}\n",
            db_path.display().to_string()
        ),
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.current_dir(temp.path())
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "No index found for the current directory",
        ));
}

use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn test_index_and_status() {
    let mock_server = MockServer::start().await;

    // Mock for index (1 file -> 1 chunk -> 1 request)
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{ "embedding": vec![0.1; 768], "index": 0 }],
            "model": "test-model",
            "object": "list",
            "usage": { "prompt_tokens": 10, "total_tokens": 10 }
        })))
        .mount(&mock_server)
        .await;

    let temp = tempdir().unwrap();
    let project_dir = temp.path().join("my-project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(
        project_dir.join("main.rs"),
        "fn main() { println!(\"hello\"); }",
    )
    .unwrap();

    let config_path = temp.path().join("config.toml");
    let db_path = temp.path().join("test_index.db");

    fs::write(&config_path, format!(
        "db_path = {:?}\napi_base_url = {:?}\nembedding_model = \"test-model\"\nllm_model = \"test-chat-model\"\n",
        db_path, mock_server.uri()
    )).unwrap();

    // 1. Index the project
    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    // 2. Check status
    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("Project:"))
        .stdout(predicate::str::contains("my-project"))
        .stdout(predicate::str::contains("Files:      1"))
        .stdout(predicate::str::contains("Chunks:     1"));

    // 3. Search for the function
    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("search")
        .arg("hello")
        .assert()
        .success()
        .stdout(predicate::str::contains("main.rs"))
        .stdout(predicate::str::contains("fn main()"));
}

// ── Capability tier E2E tests ────────────────────────────────────────────────

#[tokio::test]
async fn test_status_shows_offline_tier() {
    let temp = tempdir().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(project_dir.join("main.rs"), "fn main() {}").unwrap();

    let config_path = temp.path().join("config.toml");
    let db_path = temp.path().join("index.db");
    fs::write(
        &config_path,
        format!(
            "db_path = {:?}\napi_base_url = \"http://127.0.0.1:1234\"\nembedding_model = \"test\"\nllm_model = \"test\"\n",
            db_path
        ),
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("Capability tier:"))
        .stdout(predicate::str::contains("Offline"))
        .stdout(predicate::str::contains("ast-grep + text"))
        .stdout(predicate::str::contains("git-notes (local)"))
        .stdout(predicate::str::contains("set server_url to enable"));
}

#[tokio::test]
async fn test_status_shows_server_tier() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "version": "test",
            "capabilities": ["memory", "index.embed", "search.semantic", "explore", "plan"]
        })))
        .mount(&mock_server)
        .await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/projects/.+/index/embed$"))
        .respond_with(IndexEmbedResponder)
        .mount(&mock_server)
        .await;

    let temp = tempdir().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(project_dir.join("main.rs"), "fn main() {}").unwrap();

    let db_path = temp.path().join("index.db");
    let config_path = write_config_with_server(
        temp.path(),
        &db_path,
        &mock_server.uri(),
        &mock_server.uri(),
    );

    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("Capability tier:"))
        .stdout(predicate::str::contains("Server"))
        .stdout(predicate::str::contains("semantic"))
        .stdout(predicate::str::contains("server sync"));
}

#[tokio::test]
async fn test_status_json_includes_tier_fields() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "version": "test",
            "capabilities": ["memory", "index.embed", "search.semantic", "plan"]
        })))
        .mount(&mock_server)
        .await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/projects/.+/index/embed$"))
        .respond_with(IndexEmbedResponder)
        .mount(&mock_server)
        .await;

    let temp = tempdir().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(
        project_dir.join("lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }",
    )
    .unwrap();

    let db_path = temp.path().join("index.db");
    let config_path = write_config_with_server(
        temp.path(),
        &db_path,
        &mock_server.uri(),
        &mock_server.uri(),
    );

    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    let output = Command::cargo_bin("spelunk")
        .unwrap()
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();

    assert!(output.status.success());
    let body: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON output");
    assert_eq!(body["tier"], "server");
    assert!(body["server_url"].is_string());
    assert!(body["capabilities"].is_object());
    assert!(body["capabilities"]["search_semantic"].as_bool().unwrap());
    assert!(body["capabilities"]["index_embed"].as_bool().unwrap());
    assert!(body["capabilities"]["plan"].as_bool().unwrap());
    assert!(!body["capabilities"]["explore"].as_bool().unwrap());
}

/// Validate the *stable* JSON schema introduced by issue #269.
///
/// Asserted top-level keys must be present in every future release; their
/// types must remain stable (additive changes only).
#[tokio::test]
async fn test_status_json_stable_schema() {
    // Offline mode — no server URL configured; embed locally.
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{ "embedding": vec![0.1f64; 768], "index": 0 }],
            "model": "test-model",
            "object": "list",
            "usage": { "prompt_tokens": 5, "total_tokens": 5 }
        })))
        .mount(&mock_server)
        .await;

    let temp = tempdir().unwrap();
    let project_dir = temp.path().join("myproject");
    fs::create_dir(&project_dir).unwrap();
    fs::write(
        project_dir.join("main.rs"),
        "fn main() { println!(\"hello\"); }",
    )
    .unwrap();

    let db_path = temp.path().join("index.db");
    let config_path = temp.path().join("config.toml");
    fs::write(
        &config_path,
        format!(
            "db_path = {:?}\napi_base_url = {:?}\nembedding_model = \"test-model\"\nllm_model = \"test\"\n",
            db_path,
            mock_server.uri()
        ),
    )
    .unwrap();

    // Index the project so there is data to query.
    Command::cargo_bin("spelunk")
        .unwrap()
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    let output = Command::cargo_bin("spelunk")
        .unwrap()
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();

    assert!(output.status.success(), "status --format json failed");
    let body: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("output must be valid JSON");

    // ── Stable schema assertions (issue #269) ────────────────────────────────
    assert!(
        body["version"].is_string(),
        "version must be a string, got: {}",
        body["version"]
    );
    // `project` may be null if the project was not registered via `spelunk init`.
    assert!(
        body["project"].is_string() || body["project"].is_null(),
        "project must be string or null"
    );
    assert!(
        body["db_path"].is_string(),
        "db_path must be a string, got: {}",
        body["db_path"]
    );
    assert_eq!(
        body["indexed_files"].as_i64().unwrap(),
        1,
        "expected 1 indexed file"
    );
    assert!(
        body["total_chunks"].as_i64().unwrap() >= 1,
        "expected at least 1 chunk"
    );
    // languages must be an array; Rust file should appear.
    assert!(body["languages"].is_array(), "languages must be an array");
    let langs = body["languages"].as_array().unwrap();
    assert!(!langs.is_empty(), "languages must not be empty");
    // Each language entry must have name (string) and file_count (integer).
    for lang in langs {
        assert!(lang["name"].is_string(), "language name must be string");
        assert!(
            lang["file_count"].as_i64().is_some(),
            "language file_count must be integer"
        );
    }
    // embedding_dim: must be an integer or null (768 when embeddings are stored,
    // null when the local embedding server is not available in CI/test mode).
    assert!(
        body["embedding_dim"].as_u64().is_some() || body["embedding_dim"].is_null(),
        "embedding_dim must be a positive integer or null, got: {}",
        body["embedding_dim"]
    );
    // has_semantic_search: false in offline mode (no server_url).
    assert_eq!(
        body["has_semantic_search"].as_bool(),
        Some(false),
        "has_semantic_search must be false in offline mode"
    );
    // last_indexed_at: ISO-8601 string when files are indexed.
    assert!(
        body["last_indexed_at"].is_string(),
        "last_indexed_at must be a string after indexing"
    );
    let ts = body["last_indexed_at"].as_str().unwrap();
    assert!(
        ts.contains('T') && ts.ends_with('Z'),
        "last_indexed_at must be ISO-8601 UTC, got: {ts}"
    );
    // memory_entries: integer (0 is valid when no entries exist yet).
    assert!(
        body["memory_entries"].as_i64().is_some(),
        "memory_entries must be an integer"
    );
}

#[tokio::test]
async fn test_check_reports_server_reachable() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "version": "test",
            "capabilities": ["memory", "search.semantic", "explore"]
        })))
        .mount(&mock_server)
        .await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/projects/.+/index/embed$"))
        .respond_with(IndexEmbedResponder)
        .mount(&mock_server)
        .await;

    let temp = tempdir().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(project_dir.join("main.rs"), "fn main() {}").unwrap();

    let db_path = temp.path().join("index.db");
    let config_path = write_config_with_server(
        temp.path(),
        &db_path,
        &mock_server.uri(),
        &mock_server.uri(),
    );

    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("check")
        .assert()
        .success()
        .stdout(predicate::str::contains("Server:"))
        .stdout(predicate::str::contains("semantic search"))
        .stdout(predicate::str::contains("explore"));
}

#[tokio::test]
async fn test_check_reports_server_unreachable() {
    let temp = tempdir().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(project_dir.join("main.rs"), "fn main() {}").unwrap();

    let db_path = temp.path().join("index.db");
    let config_path = temp.path().join("config.toml");
    let bad_url = "http://127.0.0.1:19999";
    fs::write(
        &config_path,
        format!(
            "db_path = {:?}\napi_base_url = {:?}\nembedding_model = \"test\"\nllm_model = \"test\"\nserver_url = {:?}\nproject_id = {:?}\n",
            db_path, bad_url, bad_url, FIXTURE_PROJECT_ID
        ),
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("check")
        .assert()
        .success()
        .stdout(predicate::str::contains("Server:"))
        .stdout(predicate::str::contains("unreachable"));
}

#[tokio::test]
async fn test_index_prints_note_when_no_server_configured() {
    let temp = tempdir().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(project_dir.join("main.rs"), "fn main() {}").unwrap();

    let config_path = temp.path().join("config.toml");
    let db_path = temp.path().join("index.db");
    fs::write(
        &config_path,
        format!(
            "db_path = {:?}\napi_base_url = \"http://127.0.0.1:1234\"\nembedding_model = \"test\"\nllm_model = \"test\"\n",
            db_path
        ),
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success()
        .stderr(predicate::str::contains("configure server_url"));
}

#[test]
fn test_status_json_offline_tier() {
    let temp = tempdir().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(project_dir.join("lib.rs"), "pub fn answer() -> i32 { 42 }").unwrap();

    let config_path = temp.path().join("config.toml");
    let db_path = temp.path().join("index.db");
    fs::write(
        &config_path,
        format!(
            "db_path = {:?}\napi_base_url = \"http://127.0.0.1:1234\"\nembedding_model = \"test\"\nllm_model = \"test\"\n",
            db_path
        ),
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    let output = Command::cargo_bin("spelunk")
        .unwrap()
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();

    assert!(output.status.success());
    let body: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON output");
    assert_eq!(body["tier"], "offline");
    assert!(body["server_url"].is_null());
    assert!(body["capabilities"].is_null());
}
