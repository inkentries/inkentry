use crate::plumbing_helpers;
use plumbing_helpers::{
    FIXTURE_PROJECT_ID, IndexEmbedResponder, inkentry_bin, inkentry_bin_in, mount_health,
    mount_index_embed,
};

use predicates::prelude::*;
use std::path::Path;
use tempfile::TempDir;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

// Hands the fixed-port fallback (step 3b) the mock's port via INKENTRY_TEST_DISCOVERY_PORT.
// Step 3a (`server.port`) needs a live inkentry-server pid and matching instance id, which a
// wiremock stand-in cannot be; the state dir is still created and redirected.
fn loopback_discovery_port(state_dir: &Path, url: &str) -> String {
    std::fs::create_dir_all(state_dir).expect("create state dir");
    url.rsplit(':')
        .next()
        .expect("uri has a port")
        .trim_end_matches('/')
        .to_string()
}

// Ambient INKENTRY_* vars are scrubbed so a developer/CI shell cannot change which tier is probed.
fn embed_loopback_cmd(
    home: &Path,
    project: &Path,
    state_dir: &Path,
    discovery_port: &str,
    config: &Path,
) -> assert_cmd::Command {
    let mut cmd = inkentry_bin_in(home);
    cmd.current_dir(project)
        .env_remove("INKENTRY_SERVER_URL")
        .env_remove("INKENTRY_MODE")
        .env_remove("INKENTRY_PROJECT_ID")
        .env_remove("INKENTRY_NO_SERVER")
        .env("INKENTRY_STATE_DIR", state_dir)
        .env("INKENTRY_TEST_DISCOVERY_PORT", discovery_port)
        .arg("--config")
        .arg(config)
        .arg("plumbing")
        .arg("embed");
    cmd
}

// `Config::load` honors `server_url`/`project_id` only from a project-level config or env,
// never the `--config` file, so the caller's Command must set `.current_dir(dir.path())`.
// `mode = "cloud_first"` makes the explicit `server_url` the inference target; under the default
// `local_first` it is a memory replica only.
fn write_server_config(dir: &TempDir, server_uri: &str) -> std::path::PathBuf {
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        "embedding_model = \"test-model\"\nmode = \"cloud_first\"\n",
    )
    .unwrap();
    plumbing_helpers::write_project_server_config(dir.path(), server_uri, FIXTURE_PROJECT_ID);
    config
}

#[tokio::test]
async fn embed_exits_0_with_empty_piped_stdin() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "capabilities": ["index.embed", "search.semantic"],
        })))
        .mount(&mock)
        .await;

    let tmp = TempDir::new().unwrap();
    let config = write_server_config(&tmp, &mock.uri());

    inkentry_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&config)
        .arg("plumbing")
        .arg("embed")
        .write_stdin("")
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

#[tokio::test]
async fn embed_document_mode_produces_jsonl_vector() {
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "capabilities": ["index.embed", "search.semantic"],
        })))
        .mount(&mock)
        .await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/projects/.+/index/embed$"))
        .respond_with(IndexEmbedResponder)
        .mount(&mock)
        .await;

    let tmp = TempDir::new().unwrap();
    let config = write_server_config(&tmp, &mock.uri());

    let output = inkentry_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&config)
        .arg("plumbing")
        .arg("embed")
        .write_stdin("fn greet(name: &str) -> String { format!(\"Hello, {}!\", name) }\n")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows = plumbing_helpers::parse_jsonl(&output);
    assert_eq!(rows.len(), 1, "one stdin line → one embedding");

    let row = &rows[0];
    // The config sets a different `embedding_model`; the reported model must be the pinned
    // constant regardless.
    assert_eq!(
        row.get("model").and_then(|v| v.as_str()),
        Some(inkentry_core::embeddings::MODEL_ID),
        "'model' must report the pinned model id, not a config value"
    );
    assert!(row.get("dimensions").is_some(), "missing 'dimensions'");
    assert!(row.get("vector").is_some(), "missing 'vector'");

    let dims = row["dimensions"].as_u64().unwrap_or(0);
    assert!(dims > 0, "dimensions should be positive");

    let vec_len = row["vector"].as_array().map(|a| a.len()).unwrap_or(0);
    assert_eq!(
        vec_len, dims as usize,
        "vector length must match dimensions"
    );
}

#[tokio::test]
async fn embed_query_mode_produces_jsonl_vector() {
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "capabilities": ["index.embed", "search.semantic"],
        })))
        .mount(&mock)
        .await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/projects/.+/index/embed$"))
        .respond_with(IndexEmbedResponder)
        .mount(&mock)
        .await;

    let tmp = TempDir::new().unwrap();
    let config = write_server_config(&tmp, &mock.uri());

    let output = inkentry_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&config)
        .arg("plumbing")
        .arg("embed")
        .arg("--query")
        .write_stdin("how does greet work?\n")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows = plumbing_helpers::parse_jsonl(&output);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].get("vector").is_some(), "missing 'vector'");
}

#[tokio::test]
async fn embed_multiple_lines_produce_multiple_vectors() {
    let mock = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "capabilities": ["index.embed", "search.semantic"],
        })))
        .mount(&mock)
        .await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/projects/.+/index/embed$"))
        .respond_with(IndexEmbedResponder)
        .mount(&mock)
        .await;

    let tmp = TempDir::new().unwrap();
    let config = write_server_config(&tmp, &mock.uri());

    let output = inkentry_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&config)
        .arg("plumbing")
        .arg("embed")
        .write_stdin("first line\nsecond line\nthird line\n")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows = plumbing_helpers::parse_jsonl(&output);
    assert_eq!(rows.len(), 3, "three stdin lines → three embeddings");
}

// No `server_url` and the default `local_first` mode: embed must still find the loopback
// server, as `search` and `memory search` do.
#[tokio::test]
async fn embed_finds_auto_discovered_loopback_server() {
    let mock = MockServer::start().await;
    mount_health(&mock).await;
    mount_index_embed(&mock).await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    // No `.inkentry/config.toml`: pure loopback auto-discovery.
    let state_dir = home.path().join("state");
    let discovery_port = loopback_discovery_port(&state_dir, &mock.uri());

    let config = project.path().join("config.toml");
    std::fs::write(&config, "embedding_model = \"test-model\"\n").unwrap();

    let output = embed_loopback_cmd(
        home.path(),
        project.path(),
        &state_dir,
        &discovery_port,
        &config,
    )
    .write_stdin("hello world\n")
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();

    let rows = plumbing_helpers::parse_jsonl(&output);
    assert_eq!(rows.len(), 1, "one stdin line → one embedding");
    assert!(rows[0].get("vector").is_some(), "missing 'vector'");
    assert_eq!(
        rows[0].get("model").and_then(|v| v.as_str()),
        Some(inkentry_core::embeddings::MODEL_ID),
    );
}

// `--query` goes through `embed_query_vec`, a code path distinct from the document branch.
#[tokio::test]
async fn embed_query_finds_auto_discovered_loopback_server() {
    let mock = MockServer::start().await;
    mount_health(&mock).await;
    mount_index_embed(&mock).await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let state_dir = home.path().join("state");
    let discovery_port = loopback_discovery_port(&state_dir, &mock.uri());

    let config = project.path().join("config.toml");
    std::fs::write(&config, "embedding_model = \"test-model\"\n").unwrap();

    let output = embed_loopback_cmd(
        home.path(),
        project.path(),
        &state_dir,
        &discovery_port,
        &config,
    )
    .arg("--query")
    .write_stdin("how does greet work?\n")
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();

    let rows = plumbing_helpers::parse_jsonl(&output);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].get("vector").is_some(), "missing 'vector'");
}

// With no server reachable (`INKENTRY_NO_SERVER=1` keeps this deterministic) embed still fails
// with the actionable `requires inkentry-server` error.
#[test]
fn embed_exits_nonzero_when_no_server_configured() {
    let tmp = TempDir::new().unwrap();
    let config = tmp.path().join("config.toml");
    std::fs::write(&config, "embedding_model = \"test-model\"\n").unwrap();

    inkentry_bin()
        .env("INKENTRY_NO_SERVER", "1")
        .arg("--config")
        .arg(&config)
        .arg("plumbing")
        .arg("embed")
        .write_stdin("some text\n")
        .assert()
        .failure()
        .stderr(predicate::str::contains("requires inkentry-server"));
}
