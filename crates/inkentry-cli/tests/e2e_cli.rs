use predicates::prelude::*;
use std::fs;
use tempfile::tempdir;

mod plumbing_helpers;
use plumbing_helpers::{
    FIXTURE_PROJECT_ID, IndexEmbedResponder, inkentry_bin, inkentry_bin_in,
    write_config_with_server, write_project_server_config,
};

#[test]
fn test_help_output() {
    let mut cmd = inkentry_bin();
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            // On Windows clap prints `inkentry.exe [OPTIONS]`; match only the stable prefix.
            "Usage: inkentry",
        ))
        .stdout(predicate::str::contains("Commands:"));
}

// Presence of a corrected token or absence of a stale one, not exact prose, so copy edits do not break it.
#[test]
fn test_help_text_accuracy_guards() {
    inkentry_bin()
        .args(["memory", "add", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("antipattern"));

    inkentry_bin()
        .args(["memory", "harvest", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("failures"))
        .stdout(predicate::str::contains("ADR-").not());

    inkentry_bin()
        .args(["sync", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("shorthand"))
        .stdout(predicate::str::contains("alias").not());
}

#[test]
fn harvest_is_a_top_level_command_with_a_hidden_working_alias() {
    // "backfill" appears only in the harvest command's about, so it proves the command is listed.
    inkentry_bin()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("backfill"));

    inkentry_bin()
        .args(["harvest", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("git"))
        .stdout(predicate::str::contains("claude-code"))
        .stdout(predicate::str::contains("failures"))
        .stdout(predicate::str::contains("--source"))
        .stdout(predicate::str::contains("--db"))
        .stdout(predicate::str::contains("--backend"))
        .stdout(predicate::str::contains("ADR-").not());

    inkentry_bin()
        .args(["memory", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("harvest").not());

    inkentry_bin()
        .args(["memory", "harvest", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("failures"))
        .stdout(predicate::str::contains("ADR-").not());
}

#[test]
fn test_help_does_not_list_explore() {
    inkentry_bin()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("explore").not());
}

#[test]
fn test_explore_subcommand_is_gone() {
    inkentry_bin()
        .args(["explore", "how does auth work"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand"));
}

#[test]
fn test_help_does_not_list_check() {
    inkentry_bin()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("check").not());
}

#[test]
fn test_check_subcommand_is_gone() {
    inkentry_bin()
        .args(["check"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand"))
        .stderr(predicate::str::contains("check"));
}

#[test]
fn test_check_porcelain_flags_are_gone() {
    inkentry_bin()
        .args(["check", "--format", "porcelain", "--files"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand"));
}

#[test]
fn test_invalid_command() {
    let mut cmd = inkentry_bin();
    cmd.arg("nonexistent-command")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "error: unrecognized subcommand 'nonexistent-command'",
        ));
}

#[test]
fn test_languages_output() {
    let mut cmd = inkentry_bin();
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

    let mut cmd = inkentry_bin();
    cmd.current_dir(temp.path())
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .assert()
        .success()
        // An un-init'd dir reports no project rather than describing the global store.
        .stdout(predicate::str::contains("No inkentry project here"));
}

use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn test_index_and_status() {
    let mock_server = MockServer::start().await;
    let project_id = FIXTURE_PROJECT_ID;

    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "capabilities": ["memory", "index.embed", "search.semantic", "plan"],
        })))
        .mount(&mock_server)
        .await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/projects/.+/index/embed$"))
        .respond_with(IndexEmbedResponder)
        .mount(&mock_server)
        .await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/projects/.+/search$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "mode": "hybrid",
            "query_vector": vec![0.1f32; 896],
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

    fs::write(
        &config_path,
        format!(
            concat!("db_path = {:?}\n", "llm_model = \"test-chat-model\"\n",),
            db_path,
        ),
    )
    .unwrap();
    // `server_url`/`project_id` are read only from the project `.inkentry/config.toml` or env, not `--config`.
    write_project_server_config(&project_dir, &mock_server.uri(), project_id);

    // A bare `server_url` under `local_first` never routes embedding/search to it, so opt into
    // `cloud_first` on every command to reach the mock.
    const CLOUD_FIRST: (&str, &str) = ("INKENTRY_MODE", "cloud_first");

    let mut cmd = inkentry_bin();
    cmd.current_dir(&project_dir)
        .env(CLOUD_FIRST.0, CLOUD_FIRST.1)
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    let mut cmd = inkentry_bin();
    cmd.current_dir(&project_dir)
        .env(CLOUD_FIRST.0, CLOUD_FIRST.1)
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("Project:"))
        .stdout(predicate::str::contains("my-project"))
        .stdout(predicate::str::contains("Files:      1"))
        .stdout(predicate::str::contains("Chunks:     1"));

    let mut cmd = inkentry_bin();
    cmd.current_dir(&project_dir)
        .env(CLOUD_FIRST.0, CLOUD_FIRST.1)
        .arg("--config")
        .arg(&config_path)
        .arg("search")
        .arg("hello")
        .assert()
        .success()
        .stdout(predicate::str::contains("main.rs"))
        .stdout(predicate::str::contains("fn main()"));
}

// Asserts on the raw request path: a naive `format!` of a slug like `local/<hex>` would still
// match the mock via `path_regex`, so the segment count and `%2F` are checked directly.
#[tokio::test]
async fn test_index_encodes_project_id_with_slashes_as_single_segment() {
    for project_id in [
        // No-remote repo: derive_local_fallback() shape.
        "local/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcd",
        // Remote repo: normalise_git_url() shape.
        "github.com/owner/repo",
    ] {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ok",
                "capabilities": ["memory", "index.embed", "search.semantic", "plan"],
            })))
            .mount(&mock_server)
            .await;

        // Match any `/index/embed` shape, including an unencoded-slash split, so a regression fails
        // the path-shape assertion below rather than as an opaque 404.
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.*/index/embed$"))
            .respond_with(IndexEmbedResponder)
            .mount(&mock_server)
            .await;

        let temp = tempdir().unwrap();
        let project_dir = temp.path().join("project");
        fs::create_dir(&project_dir).unwrap();
        fs::write(
            project_dir.join("main.rs"),
            "fn main() { println!(\"hello\"); }",
        )
        .unwrap();

        let config_path = temp.path().join("config.toml");
        let db_path = temp.path().join("test_index.db");

        fs::write(
            &config_path,
            format!(
                concat!("db_path = {:?}\n", "llm_model = \"test-chat-model\"\n",),
                db_path,
            ),
        )
        .unwrap();

        // `server_url` loads only from the project config or env, never the global config.
        write_project_server_config(&project_dir, &mock_server.uri(), project_id);

        // Needs an explicit `server_url` to serve embedding; `local_first` refuses that routing and
        // the project config has no `mode` key, so force `cloud_first` via env.
        inkentry_bin()
            .current_dir(&project_dir)
            .env("INKENTRY_MODE", "cloud_first")
            .arg("--config")
            .arg(&config_path)
            .arg("index")
            .arg(&project_dir)
            .assert()
            .success();

        let received = mock_server.received_requests().await.unwrap();
        let embed_reqs: Vec<_> = received
            .iter()
            .filter(|r| r.url.path().ends_with("/index/embed"))
            .collect();
        assert!(
            !embed_reqs.is_empty(),
            "expected at least one /index/embed request for project_id {project_id:?}, got: {:?}",
            received.iter().map(|r| r.url.path()).collect::<Vec<_>>()
        );

        for req in &embed_reqs {
            let raw_path = req.url.path();
            let segments: Vec<&str> = raw_path.trim_start_matches('/').split('/').collect();

            // `v1/projects/<id>/index/embed` is five segments; a raw `/` in the slug would add one or two.
            assert_eq!(
                segments.len(),
                5,
                "project_id {project_id:?} produced a path with the wrong \
                 number of segments (slug `/` not percent-encoded?): {raw_path:?}"
            );
            assert_eq!(segments[0], "v1");
            assert_eq!(segments[1], "projects");
            assert_eq!(segments[3], "index");
            assert_eq!(segments[4], "embed");

            let encoded_segment = segments[2];
            assert!(
                !encoded_segment.contains('/'),
                "project_id segment must not contain a raw `/`: {encoded_segment:?}"
            );
            assert!(
                encoded_segment.contains("%2F") || encoded_segment.contains("%2f"),
                "project_id {project_id:?} contains `/` and must be percent-encoded \
                 as a single segment (expected `%2F` in {encoded_segment:?})"
            );

            let decoded = percent_encoding::percent_decode_str(encoded_segment)
                .decode_utf8()
                .expect("encoded project_id segment must decode as utf-8");
            assert_eq!(
                decoded, project_id,
                "decoded project_id segment must round-trip to the original slug"
            );
        }
    }
}

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
            "db_path = {:?}\napi_base_url = \"http://127.0.0.1:1234\"\nllm_model = \"test\"\n",
            db_path
        ),
    )
    .unwrap();

    let mut cmd = inkentry_bin();
    cmd.env("INKENTRY_NO_SERVER", "1") // ensure offline even if a local server is running
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    let mut cmd = inkentry_bin();
    cmd.env("INKENTRY_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("Capability tier:"))
        .stdout(predicate::str::contains("Offline"))
        .stdout(predicate::str::contains("search          text"))
        .stdout(predicate::str::contains("sqlite (local)"))
        // The kill-switch is why this run is offline, so the hint must name it rather than
        // recommend `server_url`, which the switch short-circuits.
        .stdout(predicate::str::contains("INKENTRY_NO_SERVER"))
        .stdout(predicate::str::contains("server_url").not());
}

#[tokio::test]
async fn test_status_offline_without_the_kill_switch_points_at_the_local_daemon() {
    let temp = tempdir().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(project_dir.join("main.rs"), "fn main() {}").unwrap();

    let config_path = temp.path().join("config.toml");
    let db_path = temp.path().join("index.db");
    fs::write(&config_path, format!("db_path = {:?}\n", db_path)).unwrap();

    let mut cmd = inkentry_bin();
    cmd.env("INKENTRY_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    // `inkentry_bin` isolates HOME and disables the fixed-port fallback, so discovery finds
    // nothing and the tier is offline without the kill-switch.
    let stdout = inkentry_bin()
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("Offline"))
        .stdout(predicate::str::contains("inkentry server start"))
        .stdout(predicate::str::contains("INKENTRY_NO_SERVER").not())
        .get_output()
        .stdout
        .clone();

    let out = String::from_utf8_lossy(&stdout);
    let hint = out
        .lines()
        .find(|l| l.contains("inkentry server start"))
        .expect("the offline search line carries the hint");
    let daemon_at = hint.find("inkentry server start").unwrap();
    if let Some(url_at) = hint.find("server_url") {
        assert!(
            daemon_at < url_at,
            "the local daemon must be suggested before server_url: {hint}"
        );
    }
}

#[tokio::test]
async fn test_status_shows_server_tier() {
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
    fs::write(project_dir.join("main.rs"), "fn main() {}").unwrap();

    let db_path = temp.path().join("index.db");
    let config_path = write_config_with_server(
        temp.path(),
        &db_path,
        &mock_server.uri(),
        &mock_server.uri(),
        &project_dir,
    );

    let mut cmd = inkentry_bin();
    cmd.current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    let mut cmd = inkentry_bin();
    cmd.current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("Capability tier:"))
        .stdout(predicate::str::contains("Server"))
        .stdout(predicate::str::contains("semantic"))
        // An explicit team server_url is still `local_first`, so the store is local sqlite.
        .stdout(predicate::str::contains("sqlite (local)"));
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
        &project_dir,
    );

    let mut cmd = inkentry_bin();
    cmd.current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    let output = inkentry_bin()
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
    // `plan` is a reserved protocol field: even when advertised it must not surface in status JSON.
    assert!(body["capabilities"]["plan"].is_null());
    assert!(body["capabilities"]["explore"].is_null());
    // Tier and sync mode are independent: a reachable server with no `mode` override is still `local_first`.
    assert_eq!(body["mode"], "local_first", "got: {body}");
}

// Top-level keys must stay present with stable types (additive changes only).
#[tokio::test]
async fn test_status_json_stable_schema() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{ "embedding": vec![0.1f64; 896], "index": 0 }],
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
            "db_path = {:?}\napi_base_url = {:?}\nllm_model = \"test\"\n",
            db_path,
            mock_server.uri()
        ),
    )
    .unwrap();

    inkentry_bin()
        .env("INKENTRY_NO_SERVER", "1") // ensure offline even if a local server is running
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    let output = inkentry_bin()
        .env("INKENTRY_NO_SERVER", "1")
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

    assert!(
        body["version"].is_string(),
        "version must be a string, got: {}",
        body["version"]
    );
    // `project` may be null if the project was not registered via `inkentry init`.
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
    assert!(body["languages"].is_array(), "languages must be an array");
    let langs = body["languages"].as_array().unwrap();
    assert!(!langs.is_empty(), "languages must not be empty");
    for lang in langs {
        assert!(lang["name"].is_string(), "language name must be string");
        assert!(
            lang["file_count"].as_i64().is_some(),
            "language file_count must be integer"
        );
    }
    // embedding_dim is null when no embedder is available in test mode.
    assert!(
        body["embedding_dim"].as_u64().is_some() || body["embedding_dim"].is_null(),
        "embedding_dim must be a positive integer or null, got: {}",
        body["embedding_dim"]
    );
    assert_eq!(
        body["has_semantic_search"].as_bool(),
        Some(false),
        "has_semantic_search must be false in offline mode"
    );
    assert!(
        body["last_indexed_at"].is_string(),
        "last_indexed_at must be a string after indexing"
    );
    let ts = body["last_indexed_at"].as_str().unwrap();
    assert!(
        ts.contains('T') && ts.ends_with('Z'),
        "last_indexed_at must be ISO-8601 UTC, got: {ts}"
    );
    assert!(
        body["memory_entries"].as_i64().is_some(),
        "memory_entries must be an integer"
    );
    assert_eq!(body["mode"], "offline", "got: {body}");
}

// Locks the exact key set so a field cannot be silently renamed, dropped or added.
#[tokio::test]
async fn test_status_json_top_level_keys_are_exactly_the_documented_set() {
    let temp = tempdir().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(project_dir.join("main.rs"), "fn main() {}").unwrap();

    let db_path = temp.path().join("index.db");
    let config_path = temp.path().join("config.toml");
    fs::write(
        &config_path,
        format!(
            "db_path = {:?}\napi_base_url = \"http://127.0.0.1:1\"\nllm_model = \"test\"\n",
            db_path
        ),
    )
    .unwrap();

    inkentry_bin()
        .env("INKENTRY_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    let output = inkentry_bin()
        .env("INKENTRY_NO_SERVER", "1")
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
    let mut got: Vec<&str> = body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    got.sort_unstable();

    let mut want = vec![
        "version",
        "project",
        "db_path",
        "indexed_files",
        "file_count",
        "total_chunks",
        "languages",
        "embedding_dim",
        "has_semantic_search",
        "last_indexed_at",
        "memory_embedding_pending",
        "memory_entries",
        "memory_backend",
        "tier",
        "mode",
        "sync_pending",
        "sync_last_synced_at",
        "server_url",
        "capabilities",
        "embedder_state",
        "embedding_count",
        "embedding_pending",
        "text_only_count",
        "embedding_refresh_pending",
        "summary_scheme",
        // Distinguishes an index this build emptied from one never indexed.
        "index_rebuilt_from",
        "embed_worker_alive",
        "embed_tokens",
        "drift_candidates",
        "usage_7d",
        "metrics",
    ];
    want.sort_unstable();
    assert_eq!(
        got, want,
        "status --format json top-level key set changed; if this is an \
         intentional additive field, add it to `want` here and to the doc \
         comment on `status()`"
    );
}

// No shared server/port/filesystem state to race on; flakes are child-process spawn
// contention on loaded runners. A named serial group avoids serializing against unrelated tests.
#[tokio::test]
#[serial_test::serial(e2e_process_spawn_sensitive)]
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
            "db_path = {:?}\napi_base_url = \"http://127.0.0.1:1234\"\nllm_model = \"test\"\n",
            db_path
        ),
    )
    .unwrap();

    let mut cmd = inkentry_bin();
    // Run in the temp project like the sibling tests, else the project-config walk-up
    // reaches the repo's own .inkentry/config.toml (server_url set) and suppresses the notice.
    cmd.env("INKENTRY_NO_SERVER", "1") // ensure offline even if a local server is running
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success()
        // The notice names the recorded probe reason: here the kill-switch, so the step is
        // unsetting it; `inkentry server start` would be advice that cannot take effect.
        .stderr(predicate::str::contains("INKENTRY_NO_SERVER is set"))
        .stderr(predicate::str::contains("inkentry server start").not());
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
            "db_path = {:?}\napi_base_url = \"http://127.0.0.1:1234\"\nllm_model = \"test\"\n",
            db_path
        ),
    )
    .unwrap();

    let mut cmd = inkentry_bin();
    cmd.env("INKENTRY_NO_SERVER", "1") // ensure offline even if a local server is running
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    let output = inkentry_bin()
        .env("INKENTRY_NO_SERVER", "1")
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

#[test]
fn test_search_no_index_funnels_to_init() {
    let temp = tempdir().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(
        project_dir.join("lib.rs"),
        "pub fn greet(name: &str) -> String { format!(\"hello {name}\") }",
    )
    .unwrap();

    let config_path = temp.path().join("config.toml");
    let db_path = temp.path().join("nonexistent.db"); // deliberately absent
    fs::write(
        &config_path,
        format!(
            "db_path = {:?}\napi_base_url = \"http://127.0.0.1:1234\"\nllm_model = \"test\"\n",
            db_path
        ),
    )
    .unwrap();

    let mut cmd = inkentry_bin();
    cmd.env("INKENTRY_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("search")
        .arg("greet")
        .assert()
        .failure()
        .stderr(predicate::str::contains("inkentry init"));
}

#[test]
fn test_search_index_but_no_embedder_falls_back_to_full_text() {
    let temp = tempdir().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(
        project_dir.join("lib.rs"),
        "pub fn compute(x: i32) -> i32 { x * 2 }",
    )
    .unwrap();

    let config_path = temp.path().join("config.toml");
    let db_path = temp.path().join("index.db");
    fs::write(
        &config_path,
        format!(
            "db_path = {:?}\napi_base_url = \"http://127.0.0.1:19999\"\nllm_model = \"test\"\n",
            db_path
        ),
    )
    .unwrap();

    // INKENTRY_NO_SERVER=1 keeps the embed phase from auto-discovering a loopback server
    // on 127.0.0.1:4655.
    inkentry_bin()
        .env("INKENTRY_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    // INKENTRY_NO_SERVER pins "no embedder" regardless of what listens on the default
    // loopback port.
    let mut cmd = inkentry_bin();
    let assert = cmd
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("search")
        .arg("compute")
        .assert()
        .success();

    assert.stdout(predicate::str::contains("Make sure the index has embeddings").not());
}

#[test]
fn test_server_status_not_running() {
    let tmp = tempdir().unwrap();
    inkentry_bin_in(tmp.path())
        .arg("server")
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("not started"));
}

#[test]
fn test_server_logs_missing_file() {
    let tmp = tempdir().unwrap();
    inkentry_bin()
        .env("HOME", tmp.path())
        .arg("server")
        .arg("logs")
        .assert()
        .failure()
        .stderr(predicate::str::contains("No log file"));
}

#[test]
fn test_server_stop_not_running() {
    let tmp = tempdir().unwrap();
    inkentry_bin()
        .env("HOME", tmp.path())
        .arg("server")
        .arg("stop")
        .assert()
        .failure()
        .stderr(predicate::str::contains("server.pid"))
        .stderr(predicate::str::contains("ps ax | grep inkentry-server"));
}

// `--bin` with a nonexistent path rather than `PATH=""`: in CI both binaries share
// `target/debug/`, so the sibling lookup would find the real one.
#[test]
fn test_server_start_binary_not_found() {
    let tmp = tempdir().unwrap();
    let nonexistent = tmp.path().join("inkentry-server-does-not-exist-xyzzy");
    inkentry_bin()
        .env("HOME", tmp.path())
        .arg("server")
        .arg("start")
        .arg("--bin")
        .arg(&nonexistent)
        .assert()
        .failure()
        .stderr(predicate::str::contains("inkentry-server binary not found"));
}

#[test]
fn test_init_non_tty_prints_skip_notice() {
    let tmp = tempdir().unwrap();
    std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(tmp.path())
        .status()
        .expect("git init");
    std::process::Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(tmp.path())
        .status()
        .expect("git config email");
    std::process::Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(tmp.path())
        .status()
        .expect("git config name");

    let config_path = tmp.path().join("config.toml");
    fs::write(&config_path, "").unwrap();

    // assert_cmd pipes stdin, so `is_terminal()` is false and the non-interactive branch runs.
    inkentry_bin()
        .current_dir(tmp.path())
        .env("HOME", tmp.path())
        .env("INKENTRY_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .args(["init", "--no-index"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "server not running - semantic search skipped",
        ));
}

fn git_init_repo(dir: &std::path::Path) {
    for args in [
        &["init", "-q"][..],
        &["config", "user.email", "test@test.com"][..],
        &["config", "user.name", "Test"][..],
    ] {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git setup");
    }
}

#[test]
fn test_init_does_not_write_claude_md() {
    let tmp = tempdir().unwrap();
    git_init_repo(tmp.path());

    let config_path = tmp.path().join("config.toml");
    fs::write(&config_path, "").unwrap();

    inkentry_bin()
        .current_dir(tmp.path())
        .env("HOME", tmp.path())
        .env("INKENTRY_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .args(["init", "--no-index"])
        .assert()
        .success()
        .stdout(predicate::str::contains("CLAUDE.md written").not());

    assert!(
        !tmp.path().join("CLAUDE.md").exists(),
        "init must not create a CLAUDE.md in the project root"
    );
}

#[test]
fn test_init_leaves_existing_claude_md_untouched() {
    let tmp = tempdir().unwrap();
    git_init_repo(tmp.path());

    let claude_md = tmp.path().join("CLAUDE.md");
    let sentinel = b"# my own CLAUDE.md\n\ndo not touch\n";
    fs::write(&claude_md, sentinel).unwrap();

    let config_path = tmp.path().join("config.toml");
    fs::write(&config_path, "").unwrap();

    inkentry_bin()
        .current_dir(tmp.path())
        .env("HOME", tmp.path())
        .env("INKENTRY_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .args(["init", "--no-index"])
        .assert()
        .success();

    assert_eq!(
        fs::read(&claude_md).unwrap(),
        sentinel,
        "init must not modify a pre-existing CLAUDE.md"
    );
}

// Auto-discovery, end to end: no `server_url`, `INKENTRY_NO_SERVER` unset, a mock on
// loopback reached through the fixed-port fallback. The server is inference-only, so
// add/search/timeline all use the local `memory.db` and the server only embeds the query.
// HOME and the state dir are redirected so discovery never touches the developer's real state.
// `memory harvest` is uncovered: it needs mocked `git log` plus a streaming `/llm/complete` round-trip.

// `INKENTRY_STATE_DIR` is needed because `dirs::home_dir()` on Windows ignores
// HOME/USERPROFILE. The dir stays empty: discovery reaches the mock via the fixed-port
// fallback's test override, since `server.port` is honoured only for a live inkentry-server pid.
fn isolated_state_dir(home: &std::path::Path) -> std::path::PathBuf {
    let state_dir = home.join(".local").join("state").join("inkentry");
    fs::create_dir_all(&state_dir).expect("create state dir");
    state_dir
}

fn port_from_uri(uri: &str) -> u16 {
    uri.rsplit(':')
        .next()
        .expect("uri has a port")
        .trim_end_matches('/')
        .parse()
        .expect("uri port is numeric")
}

// Mounts only the inference endpoints; the auto-discovered server is never a memory
// backend. The embed mock returns a constant vector so KNN over the local store is deterministic.
async fn mount_auto_discovery_inference_endpoints(server: &wiremock::MockServer) {
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, ResponseTemplate};

    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "version": "test",
            "capabilities": ["memory", "index.embed", "search.semantic", "plan"]
        })))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/projects/.+/index/embed$"))
        .respond_with(IndexEmbedResponder)
        .mount(server)
        .await;

    // Guard: the server's memory endpoint must never be hit; `expect(0)` fails the test on
    // any matching request when the `MockServer` drops.
    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/projects/.+/memory/search$"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(server)
        .await;
}

#[tokio::test]
async fn test_memory_add_then_search_round_trip_on_local_store_with_auto_discovered_server() {
    let mock_server = MockServer::start().await;
    mount_auto_discovery_inference_endpoints(&mock_server).await;

    let temp = tempdir().unwrap();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let state_dir = isolated_state_dir(&home);
    let discovery_port = port_from_uri(&mock_server.uri()).to_string();

    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(project_dir.join("main.rs"), "fn main() {}").unwrap();

    // No `server_url` or `project_id`: the defining trait of the auto-discovered path.
    let config_path = temp.path().join("config.toml");
    let db_path = temp.path().join("index.db");
    fs::write(
        &config_path,
        format!(
            "db_path = {:?}\napi_base_url = \"http://127.0.0.1:1\"\nllm_model = \"test\"\n",
            db_path
        ),
    )
    .unwrap();

    // A local index gives memory commands a DB to resolve `mem_path` from.
    inkentry_bin()
        .env("HOME", &home)
        .env("INKENTRY_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    inkentry_bin()
        .env("HOME", &home)
        .env("INKENTRY_STATE_DIR", &state_dir)
        .env("INKENTRY_TEST_DISCOVERY_PORT", &discovery_port)
        .env_remove("INKENTRY_NO_SERVER")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "Unified memory storage round-trip",
            "--body",
            "Memory lives in memory.db; the loopback server is inference-only.",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Stored [decision]"));

    // The result must be the locally stored note; the `/memory/search` guard proves no
    // memory rows came from the server.
    inkentry_bin()
        .env("HOME", &home)
        .env("INKENTRY_STATE_DIR", &state_dir)
        .env("INKENTRY_TEST_DISCOVERY_PORT", &discovery_port)
        .env_remove("INKENTRY_NO_SERVER")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .args(["search", "unified memory storage", "--only-memory"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Unified memory storage round-trip",
        ))
        .stdout(predicate::str::contains("[decision]"));

    // Cross-check: `memory list` (reads memory.db) sees the same note.
    inkentry_bin()
        .env("HOME", &home)
        .env("INKENTRY_STATE_DIR", &state_dir)
        .env("INKENTRY_TEST_DISCOVERY_PORT", &discovery_port)
        .env_remove("INKENTRY_NO_SERVER")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .args(["memory", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Unified memory storage round-trip",
        ));
}

#[tokio::test]
async fn test_memory_add_then_search_round_trip_local_first_with_explicit_server_url() {
    let mock_server = MockServer::start().await;
    mount_auto_discovery_inference_endpoints(&mock_server).await;

    let temp = tempdir().unwrap();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let state_dir = isolated_state_dir(&home);
    let discovery_port = port_from_uri(&mock_server.uri()).to_string();

    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(project_dir.join("main.rs"), "fn main() {}").unwrap();

    // `server_url` is set (so `local_first`) but points at an address nothing mounts anything on:
    // a fallback to it for inference would surface as a connection error, never a silent pass.
    let config_path = temp.path().join("config.toml");
    let db_path = temp.path().join("index.db");
    fs::write(
        &config_path,
        format!(
            "db_path = {:?}\napi_base_url = \"http://127.0.0.1:1\"\nllm_model = \"test\"\nserver_url = \"https://cloud.invalid.example:1\"\nproject_id = \"team/proj\"\n",
            db_path
        ),
    )
    .unwrap();

    inkentry_bin()
        .env("HOME", &home)
        .env("INKENTRY_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    inkentry_bin()
        .env("HOME", &home)
        .env("INKENTRY_STATE_DIR", &state_dir)
        .env("INKENTRY_TEST_DISCOVERY_PORT", &discovery_port)
        .env_remove("INKENTRY_NO_SERVER")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "Local first with cloud server_url",
            "--body",
            "server_url is a sync replica only; inference stays on loopback.",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Stored [decision]"));

    inkentry_bin()
        .env("HOME", &home)
        .env("INKENTRY_STATE_DIR", &state_dir)
        .env("INKENTRY_TEST_DISCOVERY_PORT", &discovery_port)
        .env_remove("INKENTRY_NO_SERVER")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .args(["search", "local first cloud server_url", "--only-memory"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Local first with cloud server_url",
        ))
        .stdout(predicate::str::contains("[decision]"));
}

#[tokio::test]
async fn test_memory_timeline_reads_local_store_with_auto_discovered_server() {
    let mock_server = MockServer::start().await;
    mount_auto_discovery_inference_endpoints(&mock_server).await;

    let temp = tempdir().unwrap();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let state_dir = isolated_state_dir(&home);
    let discovery_port = port_from_uri(&mock_server.uri()).to_string();

    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(project_dir.join("main.rs"), "fn main() {}").unwrap();

    let config_path = temp.path().join("config.toml");
    let db_path = temp.path().join("index.db");
    fs::write(
        &config_path,
        format!(
            "db_path = {:?}\napi_base_url = \"http://127.0.0.1:1\"\nllm_model = \"test\"\n",
            db_path
        ),
    )
    .unwrap();

    inkentry_bin()
        .env("HOME", &home)
        .env("INKENTRY_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    inkentry_bin()
        .env("HOME", &home)
        .env("INKENTRY_STATE_DIR", &state_dir)
        .env("INKENTRY_TEST_DISCOVERY_PORT", &discovery_port)
        .env_remove("INKENTRY_NO_SERVER")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "Loopback server is inference-only",
            "--body",
            "Probe 127.0.0.1 when no server_url is configured; memory stays local.",
        ])
        .assert()
        .success();

    inkentry_bin()
        .env("HOME", &home)
        .env("INKENTRY_STATE_DIR", &state_dir)
        .env("INKENTRY_TEST_DISCOVERY_PORT", &discovery_port)
        .env_remove("INKENTRY_NO_SERVER")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("timeline")
        .arg("loopback server")
        .assert()
        .success()
        .stdout(predicate::str::contains("Timeline: loopback server"))
        .stdout(predicate::str::contains(
            "Loopback server is inference-only",
        ));
}

// Git notes hang off a commit, so unlike `git_init_repo` this makes one.
fn git_init_repo_with_commit(dir: &std::path::Path) {
    plumbing_helpers::isolate_git_config();
    for args in [
        &["init", "-q"][..],
        &["config", "user.email", "test@test.com"][..],
        &["config", "user.name", "Test"][..],
    ] {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git setup");
    }
    fs::write(dir.join("README.md"), "seed\n").unwrap();
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(dir)
        .status()
        .expect("git add");
    std::process::Command::new("git")
        .args(["commit", "-q", "--no-gpg-sign", "-m", "seed"])
        .current_dir(dir)
        .status()
        .expect("git commit");
}

// Built as a `serde_json::Value`: the `NoteRecord` type is crate-private.
fn git_note_record_line(id: i64, kind: &str, title: &str, body: &str) -> String {
    serde_json::json!({
        "schema_version": 1,
        "id": id,
        "kind": kind,
        "title": title,
        "body": body,
        "tags": [],
        "linked_files": [],
        // Fixed timestamps → a stable content key, so a re-run dedups exactly.
        "created_at": 1_700_000_000_i64 + id,
        "status": "active",
    })
    .to_string()
}

fn seed_git_notes(dir: &std::path::Path, jsonl: &str) {
    let notes_file = tempfile::NamedTempFile::new().expect("notes tempfile");
    fs::write(notes_file.path(), jsonl).unwrap();
    let status = std::process::Command::new("git")
        .args(["notes", "--ref=inkentry", "add", "-f", "-F"])
        .arg(notes_file.path())
        .args(["--", "HEAD"])
        .current_dir(dir)
        .status()
        .expect("git notes add");
    assert!(status.success(), "seeding git notes must succeed");
}

#[test]
fn test_init_imports_git_notes_memory_and_is_idempotent() {
    let tmp = tempdir().unwrap();
    git_init_repo_with_commit(tmp.path());

    let l1 = git_note_record_line(
        1,
        "decision",
        "Adopt sqlite for memory",
        "portable, no server",
    );
    let l2 = git_note_record_line(
        2,
        "requirement",
        "Notes survive a clone",
        "git-notes travel",
    );
    seed_git_notes(tmp.path(), &format!("{l1}\n{l2}\n"));

    let config_path = tmp.path().join("config.toml");
    fs::write(&config_path, "").unwrap();

    inkentry_bin()
        .current_dir(tmp.path())
        .env("HOME", tmp.path())
        .env("INKENTRY_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .args(["init", "--no-index"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "imported 2 entries from git notes",
        ));

    inkentry_bin()
        .current_dir(tmp.path())
        .env("HOME", tmp.path())
        .env("INKENTRY_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .args(["memory", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Adopt sqlite for memory"))
        .stdout(predicate::str::contains("Notes survive a clone"));

    inkentry_bin()
        .current_dir(tmp.path())
        .env("HOME", tmp.path())
        .env("INKENTRY_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .args(["init", "--no-index"])
        .assert()
        .success()
        .stdout(predicate::str::contains("from git notes").not());

    let output = inkentry_bin()
        .current_dir(tmp.path())
        .env("HOME", tmp.path())
        .env("INKENTRY_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .args(["memory", "list", "--format", "json", "--limit", "100"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let notes: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("memory list --format json");
    assert_eq!(
        notes.as_array().map(Vec::len),
        Some(2),
        "re-running init must not duplicate imported rows"
    );
}

#[test]
fn test_init_without_git_repo_skips_notes_import() {
    let tmp = tempdir().unwrap();
    let config_path = tmp.path().join("config.toml");
    fs::write(&config_path, "").unwrap();

    inkentry_bin()
        .current_dir(tmp.path())
        .env("HOME", tmp.path())
        .env("INKENTRY_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .args(["init", "--no-index"])
        .assert()
        .success()
        .stdout(predicate::str::contains("from git notes").not());
}

// Chunks stored, zero embeddings, no recorded worker. The index lands at
// `<project_dir>/.inkentry/index.db`, the path the worker's state files are keyed on.
fn offline_indexed_project(home: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let project_dir = home.join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(
        project_dir.join("lib.rs"),
        "pub fn compute(x: i32) -> i32 { x * 2 }\npub fn helper() -> i32 { 7 }\n",
    )
    .unwrap();
    let config_path = home.join("config.toml");
    fs::write(
        &config_path,
        "api_base_url = \"http://127.0.0.1:19999\"\nllm_model = \"test\"\n",
    )
    .unwrap();
    inkentry_bin_in(home)
        .env("INKENTRY_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();
    (project_dir, config_path)
}

// Replicates the worker's keying (blake3 of the canonicalised index path, first 16 hex
// chars), deliberately duplicated so writer/reader drift fails this test loudly.
#[cfg(unix)]
fn embed_worker_pid_file_in(
    state_dir: &std::path::Path,
    db_path: &std::path::Path,
) -> std::path::PathBuf {
    let canonical = inkentry_core::utils::canonicalize(db_path);
    let key = blake3::hash(canonical.to_string_lossy().as_bytes())
        .to_hex()
        .to_string();
    state_dir.join(format!("embed-worker-{}.pid", &key[..16]))
}

#[cfg(unix)]
fn embed_worker_pid_file(home: &std::path::Path, db_path: &std::path::Path) -> std::path::PathBuf {
    embed_worker_pid_file_in(&home.join(".local").join("state").join("inkentry"), db_path)
}

#[test]
fn test_status_json_embed_state_extensions_when_pending() {
    let home = tempfile::TempDir::new().unwrap();
    let (project_dir, config_path) = offline_indexed_project(home.path());

    let output = inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .args(["status", "--format", "json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let body: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");

    for key in [
        "version",
        "db_path",
        "indexed_files",
        "total_chunks",
        "languages",
        "embedding_dim",
        "has_semantic_search",
        "memory_entries",
        "memory_backend",
        "tier",
        "embedding_count",
    ] {
        assert!(
            body.get(key).is_some(),
            "stable/extension key `{key}` missing from status JSON"
        );
    }

    let total_chunks = body["total_chunks"].as_i64().unwrap();
    assert!(total_chunks > 0, "fixture must produce chunks");
    assert_eq!(body["embedding_count"].as_i64(), Some(0));
    assert_eq!(
        body["embedding_pending"].as_i64(),
        Some(total_chunks),
        "everything is pending on an offline-built index"
    );
    assert_eq!(
        body["embed_worker_alive"].as_bool(),
        Some(false),
        "no recorded worker must read as alive=false, never a guess"
    );
    let tokens = &body["embed_tokens"];
    assert!(
        tokens.is_object(),
        "embed_tokens must be an object: {tokens}"
    );
    let total_tokens = tokens["total_tokens"].as_i64().unwrap();
    let pending_tokens = tokens["pending_tokens"].as_i64().unwrap();
    assert!(total_tokens > 0, "token counts are written at parse time");
    assert_eq!(
        pending_tokens, total_tokens,
        "zero embeddings means every token is pending"
    );
}

#[test]
fn test_status_reports_incomplete_when_no_worker_is_recorded() {
    let home = tempfile::TempDir::new().unwrap();
    let (project_dir, config_path) = offline_indexed_project(home.path());

    inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("Embedding incomplete"))
        .stdout(predicate::str::contains("resume with `inkentry index .`"))
        .stdout(predicate::str::contains("Embedding in progress").not())
        .stdout(predicate::str::contains("may be running").not());
}

#[cfg(unix)]
#[test]
fn test_status_cleans_stale_dead_worker_pid_and_reports_incomplete() {
    let home = tempfile::TempDir::new().unwrap();
    let (project_dir, config_path) = offline_indexed_project(home.path());
    let db_path = project_dir.join(".inkentry").join("index.db");
    assert!(db_path.exists(), "offline index must exist");

    // A pid that was real and is now certainly dead: spawn and reap a child.
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let dead_pid = child.id();
    child.wait().unwrap();

    let pid_file = embed_worker_pid_file(home.path(), &db_path);
    fs::create_dir_all(pid_file.parent().unwrap()).unwrap();
    fs::write(&pid_file, format!("{dead_pid}\n")).unwrap();
    fs::write(pid_file.with_extension("baseline"), "0 1000\n").unwrap();

    inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("Embedding incomplete"))
        .stdout(predicate::str::contains("Embedding in progress").not());

    assert!(
        !pid_file.exists(),
        "a dead worker's stale pid record must be cleaned up on read"
    );
}

#[cfg(unix)]
#[test]
fn test_status_foreign_pid_reuse_never_reads_as_live_worker() {
    let home = tempfile::TempDir::new().unwrap();
    let (project_dir, config_path) = offline_indexed_project(home.path());
    let db_path = project_dir.join(".inkentry").join("index.db");

    // This test process is definitely alive, and its `ps` command line (the
    // e2e test binary plus a test-name filter) is not a inkentry index run.
    let foreign_pid = std::process::id();

    let pid_file = embed_worker_pid_file(home.path(), &db_path);
    fs::create_dir_all(pid_file.parent().unwrap()).unwrap();
    fs::write(&pid_file, format!("{foreign_pid}\n")).unwrap();

    inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("Embedding in progress").not())
        .stdout(predicate::str::contains("Embedding incomplete"));

    assert!(
        !pid_file.exists(),
        "a foreign (recycled) pid record must be cleaned up on read"
    );
}

// HOME and INKENTRY_STATE_DIR point at different dirs and the pid file exists only under
// the override, so status must resolve the override to find it.
#[cfg(unix)]
#[test]
fn test_status_honors_state_dir_override_for_embed_worker_pid() {
    let home = tempfile::TempDir::new().unwrap();
    let state_override = tempfile::TempDir::new().unwrap();
    let (project_dir, config_path) = offline_indexed_project(home.path());
    let db_path = project_dir.join(".inkentry").join("index.db");
    assert!(db_path.exists(), "offline index must exist");

    let mut child = std::process::Command::new("true").spawn().unwrap();
    let dead_pid = child.id();
    child.wait().unwrap();

    let pid_file = embed_worker_pid_file_in(state_override.path(), &db_path);
    fs::create_dir_all(pid_file.parent().unwrap()).unwrap();
    fs::write(&pid_file, format!("{dead_pid}\n")).unwrap();
    fs::write(pid_file.with_extension("baseline"), "0 1000\n").unwrap();

    let home_pid_file = embed_worker_pid_file(home.path(), &db_path);
    assert!(
        !home_pid_file.exists(),
        "fixture bug: pid file must only exist under the override"
    );

    inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .env("INKENTRY_STATE_DIR", state_override.path())
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("Embedding incomplete"))
        .stdout(predicate::str::contains("Embedding in progress").not());

    assert!(
        !pid_file.exists(),
        "the reader must resolve INKENTRY_STATE_DIR (not HOME) to find and clean up the stale pid record"
    );
}

// Re-stamps the index with an unaccepted schema version so the next open rebuilds it; the
// rebuild branches on the stamp alone, so this matches a genuinely older index.
fn downstamp_index(project_dir: &std::path::Path, to: i32) -> std::path::PathBuf {
    let db_path = project_dir.join(".inkentry").join("index.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch(&format!("PRAGMA user_version = {to};"))
        .unwrap();
    drop(conn);
    db_path
}

// A rebuilt index and a never-indexed project print the same zeros, so the rebuild must
// state itself on the run that performs it and stay attributable afterwards.
#[test]
fn a_rebuilt_index_states_itself_and_stays_attributable_until_reindexed() {
    let home = tempfile::TempDir::new().unwrap();
    let (project_dir, config_path) = offline_indexed_project(home.path());
    downstamp_index(&project_dir, 15);

    inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .args(["search", "compute"])
        .assert()
        .success()
        .stderr(predicate::str::contains("rebuilt empty"))
        .stderr(predicate::str::contains("schema version 15"))
        .stdout(
            predicate::str::contains("No results found (")
                .and(predicate::str::contains("inkentry index .")),
        );

    // A later run rebuilds nothing so must not claim to, but the emptiness is still explained.
    inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .args(["search", "compute"])
        .assert()
        .success()
        .stderr(predicate::str::contains("rebuilt empty").not())
        .stdout(predicate::str::contains("rebuilt from schema version 15"));

    inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("emptied by a rebuild"))
        .stdout(predicate::str::contains("inkentry index ."));

    // The rebuild is not a gate: the requested reindex clears the fact.
    inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("emptied by a rebuild").not());

    inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .args(["search", "compute"])
        .assert()
        .success()
        .stdout(predicate::str::contains("lib.rs"));
}

#[test]
fn status_json_reports_the_rebuild_that_emptied_the_index() {
    let home = tempfile::TempDir::new().unwrap();
    let (project_dir, config_path) = offline_indexed_project(home.path());

    let out = inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .args(["status", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        v["index_rebuilt_from"],
        serde_json::Value::Null,
        "an index no rebuild touched must not be reported as emptied"
    );

    downstamp_index(&project_dir, 15);

    let out = inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .args(["status", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["index_rebuilt_from"], serde_json::json!(15));
    assert_eq!(
        v["indexed_files"],
        serde_json::json!(0),
        "the emptiness and its cause have to be readable together"
    );
}

#[test]
fn test_search_zero_coverage_degrades_to_full_text_with_warmup_notice() {
    let home = tempfile::TempDir::new().unwrap();
    let (project_dir, config_path) = offline_indexed_project(home.path());

    inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .args(["search", "compute"])
        .assert()
        .success()
        .stderr(predicate::str::contains("warmup"))
        .stderr(predicate::str::contains("0/"))
        .stderr(predicate::str::contains("full-text search"))
        .stderr(predicate::str::contains("ast-grep").not());
}

// The default arm is a regression guard: the warmup caveat keeps a missing hit from
// reading as "not in the codebase", so it must survive `--quiet` existing.
#[test]
fn test_search_quiet_suppresses_the_stderr_notices_and_leaves_results_intact() {
    let home = tempfile::TempDir::new().unwrap();
    let (project_dir, config_path) = offline_indexed_project(home.path());

    let loud = inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .args(["search", "compute", "--format", "json"])
        .output()
        .unwrap();
    assert!(loud.status.success());
    let loud_stderr = String::from_utf8_lossy(&loud.stderr);
    assert!(
        loud_stderr.contains("warmup"),
        "the warmup caveat must still be printed by default: {loud_stderr}"
    );
    assert!(
        loud_stderr.contains("full-text search"),
        "the degraded-ranking notice must still be printed by default: {loud_stderr}"
    );

    let quiet = inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .args(["search", "compute", "--format", "json", "--quiet"])
        .output()
        .unwrap();
    assert!(quiet.status.success());
    let quiet_stderr = String::from_utf8_lossy(&quiet.stderr);
    assert!(
        quiet_stderr.trim().is_empty(),
        "--quiet must leave stderr clean, got: {quiet_stderr}"
    );

    let loud_json: serde_json::Value = serde_json::from_slice(&loud.stdout).unwrap();
    let quiet_json: serde_json::Value = serde_json::from_slice(&quiet.stdout).unwrap();
    assert_eq!(
        loud_json, quiet_json,
        "--quiet must change nothing about the results"
    );
}

// The sink swallows notices, not failures: -q must not leave a non-zero exit unexplained.
#[test]
fn test_search_quiet_still_reports_a_genuine_error() {
    let home = tempfile::TempDir::new().unwrap();
    let empty = home.path().join("not-a-project");
    fs::create_dir_all(&empty).unwrap();

    let out = inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&empty)
        .args(["search", "compute", "--quiet"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "an uninitialised directory must fail"
    );
    assert!(
        !String::from_utf8_lossy(&out.stderr).trim().is_empty(),
        "-q must not silence the reason for a non-zero exit"
    );
}

// The recorded-server warning fires when a stopped or stale server also triggers the
// ranking notice, so `--quiet` must silence it too.
#[test]
fn test_search_quiet_suppresses_the_recorded_server_warning() {
    let home = tempfile::TempDir::new().unwrap();
    let (project_dir, config_path) = offline_indexed_project(home.path());

    // Claim a free port and drop it, so the recording names one nothing answers.
    let dead_port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let state_dir = home.path().join("state");
    fs::create_dir_all(&state_dir).unwrap();
    fs::write(state_dir.join("server.port"), dead_port.to_string()).unwrap();

    let loud = inkentry_bin_in(home.path())
        .env("INKENTRY_STATE_DIR", &state_dir)
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .args(["search", "compute", "--format", "json"])
        .output()
        .unwrap();
    assert!(loud.status.success());
    let loud_stderr = String::from_utf8_lossy(&loud.stderr);
    assert!(
        loud_stderr.contains("did not answer"),
        "a recorded server that is gone must still be announced by default: {loud_stderr}"
    );

    let quiet = inkentry_bin_in(home.path())
        .env("INKENTRY_STATE_DIR", &state_dir)
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .args(["search", "compute", "--format", "json", "--quiet"])
        .output()
        .unwrap();
    assert!(quiet.status.success());
    let quiet_stderr = String::from_utf8_lossy(&quiet.stderr);
    assert!(
        quiet_stderr.trim().is_empty(),
        "--quiet must silence the recorded-server warning too, got: {quiet_stderr}"
    );
    let _: serde_json::Value =
        serde_json::from_slice(&quiet.stdout).expect("stdout must stay machine-clean JSON");
}

#[test]
fn test_search_quiet_also_suppresses_the_stale_index_warning() {
    let home = tempfile::TempDir::new().unwrap();
    let (project_dir, config_path) = offline_indexed_project(home.path());

    fs::write(
        project_dir.join("lib.rs"),
        "pub fn compute(x: i32) -> i32 { x * 3 }\npub fn helper() -> i32 { 8 }\npub fn added() -> i32 { 9 }\n",
    )
    .unwrap();

    let loud = inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .args(["search", "compute"])
        .output()
        .unwrap();
    assert!(loud.status.success());
    let loud_stderr = String::from_utf8_lossy(&loud.stderr);
    assert!(
        loud_stderr.contains("index may be stale"),
        "the edit should have made the index stale: {loud_stderr}"
    );

    let quiet = inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .args(["search", "compute", "--quiet"])
        .output()
        .unwrap();
    assert!(quiet.status.success());
    let quiet_stderr = String::from_utf8_lossy(&quiet.stderr);
    assert!(
        quiet_stderr.trim().is_empty(),
        "--quiet must silence the stale-index warning too, got: {quiet_stderr}"
    );
}

// The post-commit hook captures this stream into a file that nothing strips escapes from.
#[test]
fn test_cli_log_output_to_a_pipe_carries_no_escape_bytes() {
    let home = tempfile::TempDir::new().unwrap();
    let (project_dir, config_path) = offline_indexed_project(home.path());

    // A non-numeric discovery port logs a warning deterministically and disables the
    // fixed-port fallback, keeping the probe inside this test's world.
    let output = inkentry_bin_in(home.path())
        .env("INKENTRY_STATE_DIR", home.path().join("state"))
        .env("INKENTRY_TEST_DISCOVERY_PORT", "notaport")
        .env("RUST_LOG", "warn")
        .env_remove("NO_COLOR")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .args(["search", "compute", "--only-text"])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("disabling loopback discovery"),
        "the run should have logged a warning: {stdout}"
    );
    assert!(
        !output.stdout.contains(&0x1b),
        "log output to a pipe must be plain text: {stdout}"
    );
}

#[tokio::test]
async fn test_search_auto_partial_coverage_emits_warmup_notice_on_stderr() {
    let mock = MockServer::start().await;
    plumbing_helpers::mount_health(&mock).await;
    plumbing_helpers::mount_index_embed(&mock).await;

    let home = tempfile::TempDir::new().unwrap();
    let project_dir = home.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(
        project_dir.join("lib.rs"),
        "pub fn compute(x: i32) -> i32 { x * 2 }\n",
    )
    .unwrap();
    let db_ignored = home.path().join("unused.db");
    let config_path = write_config_with_server(
        home.path(),
        &db_ignored,
        &mock.uri(),
        &mock.uri(),
        &project_dir,
    );

    // Needs an explicit `server_url` to serve embedding; `local_first` refuses that routing,
    // so force `cloud_first` via env, which outranks both config files.
    inkentry_bin_in(home.path())
        .env("INKENTRY_MODE", "cloud_first")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    // The offline re-index stores the new chunks unembedded, so coverage drops below 100%.
    fs::write(
        project_dir.join("extra.rs"),
        "pub fn extra_helper() -> i32 { 41 }\npub fn another_helper() -> i32 { 42 }\n",
    )
    .unwrap();
    inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    let output = inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .args(["search", "compute", "--format", "json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("warmup: searchable"),
        "partial coverage must emit the warmup notice, got stderr: {stderr}"
    );
    assert!(
        stderr.contains("front-loaded by importance and recency"),
        "the notice must name the prefix shape, got stderr: {stderr}"
    );
    assert!(
        stderr.contains("inkentry status"),
        "the notice must be actionable, got stderr: {stderr}"
    );
    let _: serde_json::Value = serde_json::from_slice(&output.stdout)
        .expect("stdout must stay machine-clean JSON with all notices on stderr");
}

// Gated to unix: the helper redirects HOME, which `dirs::home_dir()` ignores on Windows.
#[cfg(unix)]
#[test]
fn unread_personal_config_server_key_is_named_on_stderr() {
    let home = tempdir().unwrap();
    let config_dir = home.path().join(".config").join("inkentry");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        "server_key = \"sk-should-not-be-read\"\nllm_model = \"gpt-oss\"\n",
    )
    .unwrap();

    let project_dir = home.path().join("proj");
    fs::create_dir_all(project_dir.join(".inkentry")).unwrap();
    fs::write(
        project_dir.join(".inkentry").join("config.toml"),
        "project_id = \"team/proj\"\n",
    )
    .unwrap();

    let output = inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&project_dir)
        .args(["status", "--format", "json"])
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("server_key"),
        "the unread credential key must be named, got stderr: {stderr}"
    );
    assert!(
        stderr.contains("no longer read"),
        "the warning must say the field is not read, got stderr: {stderr}"
    );
    assert!(
        stderr.contains("auth set-key"),
        "the warning must name the replacement command, got stderr: {stderr}"
    );
    assert!(
        !stderr.contains("sk-should-not-be-read"),
        "the warning must never echo the key it names, got stderr: {stderr}"
    );
    assert!(
        output.status.success(),
        "an unread key must not fail a command"
    );
    serde_json::from_slice::<serde_json::Value>(&output.stdout)
        .expect("stdout must stay machine-clean JSON with the warning on stderr");

    let on_disk = fs::read_to_string(config_dir.join("config.toml")).unwrap();
    assert_eq!(
        on_disk, "server_key = \"sk-should-not-be-read\"\nllm_model = \"gpt-oss\"\n",
        "the file must be left exactly as it was, byte for byte"
    );
}

#[test]
fn unread_project_config_key_is_named_on_stderr() {
    let home = tempdir().unwrap();
    let project_dir = home.path().join("proj");
    fs::create_dir_all(project_dir.join(".inkentry")).unwrap();
    fs::write(
        project_dir.join(".inkentry").join("config.toml"),
        "project_id = \"team/proj\"\nlmstudio_base_url = \"http://127.0.0.1:1234\"\n",
    )
    .unwrap();

    let output = inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&project_dir)
        .args(["status", "--format", "json"])
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("lmstudio_base_url"),
        "the unread key must be named, got stderr: {stderr}"
    );
    assert!(
        stderr.contains("has no effect"),
        "the warning must say the key did nothing, got stderr: {stderr}"
    );
    assert!(
        output.status.success(),
        "an unread key must not fail a command"
    );
    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout)
        .expect("stdout must stay machine-clean JSON with the warning on stderr");
    assert_eq!(parsed["mode"], "offline");
}
