// A total-failure `sync` batch (nothing landed) must exit non-zero and never print success
// framing. Spawns the real binary so reverting the command layer's `bail!` fails here.

use crate::plumbing_helpers;
use plumbing_helpers::inkentry_bin_in;

use predicates::prelude::*;
use std::path::Path;
use tempfile::TempDir;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

// No characters `encode_project_id` would percent-encode, so mocked routes match literally.
const PROJECT_SLUG: &str = "acme-widget";

// A bare `memory` capability is enough for `require_tier1` to unlock `sync`.
async fn mount_health(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "capabilities": ["memory"],
        })))
        .mount(server)
        .await;
}

// `created: 0, skipped: 0` with an empty `results[]`, so push falls back to the aggregate
// counts: the wire shape the command layer must read as a hard failure.
async fn mount_batch_total_failure(server: &MockServer, failed: u32) {
    Mock::given(method("POST"))
        .and(path(format!("/v1/projects/{PROJECT_SLUG}/memory/batch")))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": 0, "skipped": 0, "failed": failed, "results": []
        })))
        .mount(server)
        .await;
}

async fn mount_since_empty(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path_regex(format!(
            r"^/v1/projects/{PROJECT_SLUG}/memory/since$"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "entries": []
        })))
        .mount(server)
        .await;
}

// No `server_key`/`[auth]`, so `ensure_fresh_server_key` is a no-op and no WorkOS login is
// needed. `server_url`/`project_id` go in `<dir>/.inkentry/config.toml`: `Config::load` honors
// them only from project config or env.
fn write_config(dir: &Path, server_url: &str) -> std::path::PathBuf {
    let db_path = dir.join(".inkentry").join("index.db");
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "db_path = {db_path:?}\n\
             llm_model = \"test-chat\"\n"
        ),
    )
    .expect("write config.toml");
    plumbing_helpers::write_project_server_config(dir, server_url, PROJECT_SLUG);
    config_path
}

// The `.inkentry/` marker makes the fail-closed project gate treat `proj` as a project.
fn init_project(proj: &Path) {
    std::fs::create_dir_all(proj.join(".inkentry")).expect("create .inkentry");
}

fn seed_one_note(home: &Path, proj: &Path, config_path: &Path) {
    inkentry_bin_in(home)
        .current_dir(proj)
        .arg("--config")
        .arg(config_path)
        .args([
            "memory", "add", "--kind", "note", "--title", "T", "--body", "B",
        ])
        .assert()
        .success();
}

#[tokio::test]
async fn memory_sync_total_failure_exits_nonzero_and_does_not_print_sync_complete() {
    let server = MockServer::start().await;
    mount_health(&server).await;
    mount_batch_total_failure(&server, 1).await;
    mount_since_empty(&server).await;

    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    init_project(proj.path());
    let config_path = write_config(proj.path(), &server.uri());
    seed_one_note(home.path(), proj.path(), &config_path);

    let assert = inkentry_bin_in(home.path())
        .current_dir(proj.path())
        .arg("--config")
        .arg(&config_path)
        .arg("sync")
        .assert()
        .failure();

    let out = assert.get_output();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Sync failed"),
        "must surface the failure message; stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        !stdout.contains("Sync complete.") && !stderr.contains("Sync complete."),
        "a total-failure sync must never read as success; stdout={stdout:?} stderr={stderr:?}"
    );
}

// Both pulls reuse the pre-round cursor, so a stateless `/since` mock returns the entry
// twice; the second is deduped on `remote_id`, so the reported total must be the true
// count, not doubled.
#[tokio::test]
async fn memory_sync_total_failure_reports_the_full_two_pass_pull_count() {
    let server = MockServer::start().await;
    mount_health(&server).await;
    mount_batch_total_failure(&server, 1).await;
    Mock::given(method("GET"))
        .and(path_regex(format!(
            r"^/v1/projects/{PROJECT_SLUG}/memory/since$"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "entries": [{
                "id": "01890000-0000-7000-8000-000000000abc",
                "kind": "decision",
                "title": "Teammate",
                "body": "already on the server",
                "created_at": "2026-06-19T01:00:00Z"
            }]
        })))
        .mount(&server)
        .await;

    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    init_project(proj.path());
    let config_path = write_config(proj.path(), &server.uri());
    seed_one_note(home.path(), proj.path(), &config_path);

    let assert = inkentry_bin_in(home.path())
        .current_dir(proj.path())
        .arg("--config")
        .arg(&config_path)
        .arg("sync")
        .assert()
        .failure();

    let out = assert.get_output();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Sync failed"),
        "must still surface the push failure; stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        stderr.contains("pull still applied 1 new remote entries"),
        "the pull count must reflect the one genuinely new entry across both \
         reconciliation passes, not zero and not double-counted; stderr={stderr:?}"
    );
}

// Guards the other side: a real success must still exit zero with `Sync complete.`.
#[tokio::test]
async fn memory_sync_success_still_exits_zero_and_prints_sync_complete() {
    let server = MockServer::start().await;
    mount_health(&server).await;
    Mock::given(method("POST"))
        .and(path(format!("/v1/projects/{PROJECT_SLUG}/memory/batch")))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": 1, "skipped": 0, "failed": 0, "results": []
        })))
        .mount(&server)
        .await;
    mount_since_empty(&server).await;

    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    init_project(proj.path());
    let config_path = write_config(proj.path(), &server.uri());
    seed_one_note(home.path(), proj.path(), &config_path);

    inkentry_bin_in(home.path())
        .current_dir(proj.path())
        .arg("--config")
        .arg(&config_path)
        .arg("sync")
        .assert()
        .success()
        .stdout(predicate::str::contains("Sync complete."));
}
