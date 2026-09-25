// Inspects the `Authorization` header wiremock received per origin, so a key
// going to the wrong server is caught rather than trusting the CLI's stdout.

use crate::plumbing_helpers;
use plumbing_helpers::inkentry_bin_in;

use std::path::Path;
use tempfile::TempDir;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PROJECT_ID: &str = "test-org/test-project";

async fn mount_health_and_pull(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path_regex(r"^/v1/health$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "version": "test",
            "capabilities": ["memory"],
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/v1/projects/.+/memory/since$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"entries": []})))
        .mount(server)
        .await;
}

// `server_url`/`project_id` come from env: `Config::load` honors them only
// from a project-level `.inkentry/config.toml` or env, never from the `--config`
// file swapped per origin here.
fn write_server_config(dir: &Path, name: &str) -> std::path::PathBuf {
    let config_path = dir.join(format!("{name}.toml"));
    std::fs::write(&config_path, "").unwrap();
    config_path
}

fn set_key(home: &Path, server: &str, key: &str) {
    inkentry_bin_in(home)
        .arg("auth")
        .arg("set-key")
        .arg("--server")
        .arg(server)
        .write_stdin(format!("{key}\n"))
        .assert()
        .success();
}

// Both origins share one HOME, so they share one secret-store map; no env key
// is ever set.
#[tokio::test]
async fn two_servers_two_keys_each_gets_only_its_own_bearer_over_the_wire() {
    let server_a = MockServer::start().await;
    let server_b = MockServer::start().await;
    mount_health_and_pull(&server_a).await;
    mount_health_and_pull(&server_b).await;

    let home = TempDir::new().unwrap();
    let cfg_dir = TempDir::new().unwrap();

    set_key(home.path(), &server_a.uri(), "sk-project-a-secret");
    set_key(home.path(), &server_b.uri(), "sk-project-b-secret");

    let config_a = write_server_config(cfg_dir.path(), "a");
    let config_b = write_server_config(cfg_dir.path(), "b");

    // `plumbing pull` derives the memory store from `--db`'s sibling; an empty
    // pull exits 1, so this inspects the request, not the exit code.
    let index_db = cfg_dir.path().join("index.db");

    inkentry_bin_in(home.path())
        .env_remove("INKENTRY_SERVER_KEY")
        .env("INKENTRY_SERVER_URL", server_a.uri())
        .env("INKENTRY_PROJECT_ID", PROJECT_ID)
        .arg("--config")
        .arg(&config_a)
        .arg("plumbing")
        .arg("--db")
        .arg(&index_db)
        .arg("pull")
        .output()
        .unwrap();

    inkentry_bin_in(home.path())
        .env_remove("INKENTRY_SERVER_KEY")
        .env("INKENTRY_SERVER_URL", server_b.uri())
        .env("INKENTRY_PROJECT_ID", PROJECT_ID)
        .arg("--config")
        .arg(&config_b)
        .arg("plumbing")
        .arg("--db")
        .arg(&index_db)
        .arg("pull")
        .output()
        .unwrap();

    let requests_a = server_a.received_requests().await.unwrap();
    let since_req_a = requests_a
        .iter()
        .find(|r| r.url.path().ends_with("/memory/since"))
        .expect("server A received a /memory/since request");
    assert_eq!(
        since_req_a
            .headers
            .get("authorization")
            .map(|v| v.to_str().unwrap()),
        Some("Bearer sk-project-a-secret"),
        "server A must receive exactly its own key"
    );

    let requests_b = server_b.received_requests().await.unwrap();
    let since_req_b = requests_b
        .iter()
        .find(|r| r.url.path().ends_with("/memory/since"))
        .expect("server B received a /memory/since request");
    assert_eq!(
        since_req_b
            .headers
            .get("authorization")
            .map(|v| v.to_str().unwrap()),
        Some("Bearer sk-project-b-secret"),
        "server B must receive exactly its own key, never A's"
    );
}

// A flat key left by a pre-per-origin client is read for nothing: no
// `Authorization` header, the rejection names `auth set-key --server <url>`,
// and nothing is migrated into the per-origin map.
#[tokio::test]
async fn a_flat_key_from_an_older_client_is_not_migrated_and_the_failure_names_the_fix() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/v1/health$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "version": "test",
            "capabilities": ["memory"],
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/v1/projects/.+/memory/since$"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let home = TempDir::new().unwrap();
    let cfg_dir = TempDir::new().unwrap();

    // `auth set-key` only writes the per-origin map, so simulating an upgraded
    // install needs `secrets.toml` written directly.
    let secrets_dir = home.path().join(".config").join("inkentry");
    std::fs::create_dir_all(&secrets_dir).unwrap();
    std::fs::write(
        secrets_dir.join("secrets.toml"),
        "server_key = \"sk-legacy-preupgrade\"\n",
    )
    .unwrap();

    let config_path = write_server_config(cfg_dir.path(), "legacy");
    let index_db = cfg_dir.path().join("index.db");

    let out = inkentry_bin_in(home.path())
        .env_remove("INKENTRY_SERVER_KEY")
        .env("INKENTRY_SERVER_URL", server.uri())
        .env("INKENTRY_PROJECT_ID", PROJECT_ID)
        .arg("--config")
        .arg(&config_path)
        .arg("plumbing")
        .arg("--db")
        .arg(&index_db)
        .arg("pull")
        .output()
        .unwrap();

    let requests = server.received_requests().await.unwrap();
    let since_req = requests
        .iter()
        .find(|r| r.url.path().ends_with("/memory/since"))
        .expect("server received a /memory/since request");
    assert!(
        since_req.headers.get("authorization").is_none(),
        "the flat key must not be sent as a bearer"
    );

    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains(&format!("inkentry auth set-key --server {}", server.uri())),
        "the failure must name the fix, got:\n{stderr}"
    );

    inkentry_bin_in(home.path())
        .arg("auth")
        .arg("list-servers")
        .assert()
        .success()
        .stdout(predicates::prelude::predicate::str::contains(
            "No server keys stored",
        ));
}
