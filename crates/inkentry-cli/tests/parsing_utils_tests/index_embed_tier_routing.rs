// Under the default `local_first` mode `inkentry index` must embed via the local loopback
// embedder even when an explicit (here deliberately unroutable) `server_url` is configured;
// `cloud_first` is the only mode where `server_url` also serves inference.
// The mock loopback is wired via real auto-discovery, not `server_url`, so a routing regression
// surfaces as a connection/DNS failure rather than a silently passing test.

use crate::plumbing_helpers;
use plumbing_helpers::{FIXTURE_PROJECT_ID, inkentry_bin_in, mount_health, mount_index_embed};

use std::path::Path;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// Failsafe only: hit solely if the detached child never finishes.
const CHILD_TIMEOUT: Duration = Duration::from_secs(60);

fn write_project(dir: &Path) {
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).expect("create src dir");
    std::fs::write(
        src.join("lib.rs"),
        "pub fn greet(name: &str) -> String {\n    format!(\"hello, {name}\")\n}\n\
         pub fn farewell(name: &str) -> String {\n    format!(\"bye, {name}\")\n}\n",
    )
    .expect("write lib.rs");
}

// `mode` is not a project-config key (serde drops it); it must go through `INKENTRY_MODE` or
// the global `--config` file.
fn write_server_config(project_dir: &Path, server_url: &str) {
    let inkentry_dir = project_dir.join(".inkentry");
    std::fs::create_dir_all(&inkentry_dir).expect("create .inkentry dir");
    let cfg = format!("server_url = {server_url:?}\nproject_id = {FIXTURE_PROJECT_ID:?}\n");
    std::fs::write(inkentry_dir.join("config.toml"), cfg).expect("write project config");
}

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

async fn mount_health_loading(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "version": "test",
            "capabilities": ["memory"],
            "embedder": { "state": "loading", "detail": null },
        })))
        .mount(server)
        .await;
}

// Every INKENTRY_* var these tests isolate is scrubbed so an ambient shell value cannot change
// which tier is probed; callers add back what they need.
fn index_cmd(home: &Path, project: &Path, db: &Path) -> assert_cmd::Command {
    let mut cmd = inkentry_bin_in(home);
    cmd.current_dir(project)
        .env_remove("INKENTRY_SERVER_URL")
        .env_remove("INKENTRY_MODE")
        .env_remove("INKENTRY_PROJECT_ID")
        .env_remove("INKENTRY_NO_SERVER")
        .env_remove("INKENTRY_STATE_DIR")
        .arg("index")
        .arg("--db")
        .arg(db)
        .arg(".");
    cmd
}

fn ensure_sqlite_vec() {
    use std::sync::OnceLock;
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

fn count_embeddings(db_path: &Path) -> i64 {
    ensure_sqlite_vec();
    let conn = rusqlite::Connection::open(db_path).expect("open db");
    conn.query_row("SELECT COUNT(*) FROM embeddings", [], |r| r.get(0))
        .expect("count embeddings")
}

fn count_chunks(db_path: &Path) -> i64 {
    ensure_sqlite_vec();
    let conn = rusqlite::Connection::open(db_path).expect("open db");
    conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
        .expect("count chunks")
}

fn wait_for_embeddings(db_path: &Path) -> i64 {
    let deadline = Instant::now() + CHILD_TIMEOUT;
    loop {
        if db_path.exists() {
            let n = count_embeddings(db_path);
            if n > 0 {
                return n;
            }
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for embeddings to land in {db_path:?}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[tokio::test]
async fn local_first_foreground_embeds_via_loopback_not_unroutable_server_url() {
    let loopback = MockServer::start().await;
    mount_health(&loopback).await;
    mount_index_embed(&loopback).await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path());
    // Deliberately unroutable: a fallback surfaces as a connection/DNS error, not a silently
    // unembedded index.
    write_server_config(project.path(), "https://cloud.invalid.example:1");
    let state_dir = home.path().join("state");
    let discovery_port = loopback_discovery_port(&state_dir, &loopback.uri());

    let db = project.path().join("index.db");
    index_cmd(home.path(), project.path(), &db)
        .env("INKENTRY_STATE_DIR", &state_dir)
        .env("INKENTRY_TEST_DISCOVERY_PORT", &discovery_port)
        .assert()
        .success();

    assert!(
        count_embeddings(&db) > 0,
        "local_first must embed via the loopback mock, not skip because the \
         unreachable explicit server_url was probed instead"
    );
}

#[tokio::test]
async fn cloud_first_foreground_still_embeds_via_explicit_server_url() {
    let mock = MockServer::start().await;
    mount_health(&mock).await;
    mount_index_embed(&mock).await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path());
    write_server_config(project.path(), &mock.uri());
    // `mode` is not a project-config field; set it via env so `cloud_first` takes effect.
    let state_dir = home.path().join("state");

    let db = project.path().join("index.db");
    index_cmd(home.path(), project.path(), &db)
        .env("INKENTRY_MODE", "cloud_first")
        // An empty state dir with the fixed-port fallback disabled makes an accidental `local_first`
        // loopback probe fail loudly instead of hitting a real daemon on this machine.
        .env("INKENTRY_STATE_DIR", &state_dir)
        .env("INKENTRY_TEST_DISCOVERY_PORT", "0")
        .assert()
        .success();

    assert!(
        count_embeddings(&db) > 0,
        "cloud_first must still embed via the explicit server_url"
    );
    let requests = mock.received_requests().await.expect("requests recorded");
    assert!(
        requests
            .iter()
            .any(|r| r.url.path().contains("/index/embed")),
        "the configured server_url must have actually been used for embedding; got: {:?}",
        requests
            .iter()
            .map(|r| (r.method.to_string(), r.url.path().to_string()))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn no_server_url_configured_embeds_via_loopback_auto_discovery() {
    let loopback = MockServer::start().await;
    mount_health(&loopback).await;
    mount_index_embed(&loopback).await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path());
    // No `.inkentry/config.toml`: no server_url, no project_id.
    let state_dir = home.path().join("state");
    let discovery_port = loopback_discovery_port(&state_dir, &loopback.uri());

    let db = project.path().join("index.db");
    index_cmd(home.path(), project.path(), &db)
        .env("INKENTRY_STATE_DIR", &state_dir)
        .env("INKENTRY_TEST_DISCOVERY_PORT", &discovery_port)
        .assert()
        .success();

    assert!(
        count_embeddings(&db) > 0,
        "a project with no server_url at all must still embed via loopback \
         auto-discovery, unaffected by this fix"
    );
}

// Explicit offline skips the embed phase and names the switch; it must not advise
// `inkentry server start`, which the switch makes pointless.
#[tokio::test]
async fn explicit_offline_skips_embed_phase_with_no_server_configured() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path());

    let db = project.path().join("index.db");
    let assert = index_cmd(home.path(), project.path(), &db)
        .env("INKENTRY_NO_SERVER", "1")
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();

    assert!(
        stderr.contains("INKENTRY_NO_SERVER is set"),
        "explicit offline must still print a skip notice, naming the switch in force: {stderr}"
    );
    assert!(
        !stderr.contains("inkentry server start"),
        "the kill-switch makes a server start inert, so the notice must not offer one: {stderr}"
    );
    assert_eq!(
        count_embeddings(&db),
        0,
        "explicit offline must never embed"
    );
    assert!(
        count_chunks(&db) > 0,
        "chunks must still be indexed for text/ast-grep search"
    );
}

#[tokio::test]
async fn loopback_embedder_loading_skips_foreground_embed_with_warmup_notice() {
    let loopback = MockServer::start().await;
    mount_health_loading(&loopback).await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path());
    let state_dir = home.path().join("state");
    let discovery_port = loopback_discovery_port(&state_dir, &loopback.uri());

    let db = project.path().join("index.db");
    let assert = index_cmd(home.path(), project.path(), &db)
        .env("INKENTRY_STATE_DIR", &state_dir)
        .env("INKENTRY_TEST_DISCOVERY_PORT", &discovery_port)
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();

    assert!(
        stderr.contains("warming up"),
        "a loading embedder must print the warm-up notice: {stderr}"
    );
    assert_eq!(
        count_embeddings(&db),
        0,
        "a loading embedder must not be embedded against"
    );
}

#[tokio::test]
async fn local_first_detached_embed_routes_to_loopback_not_unroutable_server_url() {
    let loopback = MockServer::start().await;
    mount_health(&loopback).await;
    mount_index_embed(&loopback).await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path());
    write_server_config(project.path(), "https://cloud.invalid.example:1");
    let state_dir = home.path().join("state");
    let discovery_port = loopback_discovery_port(&state_dir, &loopback.uri());

    let db = project.path().join("index.db");
    index_cmd(home.path(), project.path(), &db)
        .env("INKENTRY_STATE_DIR", &state_dir)
        .env("INKENTRY_TEST_DISCOVERY_PORT", &discovery_port)
        .arg("--detach-embed")
        .assert()
        .success();

    let n = wait_for_embeddings(&db);
    assert!(
        n > 0,
        "the detached worker must embed via the loopback mock, not skip \
         because the unreachable explicit server_url was polled instead"
    );
}

// `init` points users at `index-background.log`; these pin the content that tells a working
// detached worker from one that never started.

// The detached child outlives the spawning command, so lines arrive after the parent returned.
fn wait_for_log_containing(path: &Path, needle: &str) -> String {
    let deadline = Instant::now() + CHILD_TIMEOUT;
    loop {
        let body = std::fs::read_to_string(path).unwrap_or_default();
        if body.contains(needle) {
            return body;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for {needle:?} in {path:?}; log so far:\n{body}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[tokio::test]
async fn detached_embed_worker_writes_start_progress_and_finish_to_the_log() {
    let loopback = MockServer::start().await;
    mount_health(&loopback).await;
    mount_index_embed(&loopback).await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path());
    write_server_config(project.path(), &loopback.uri());
    let state_dir = home.path().join("state");
    let discovery_port = loopback_discovery_port(&state_dir, &loopback.uri());

    let db = project.path().join("index.db");
    index_cmd(home.path(), project.path(), &db)
        .env("INKENTRY_STATE_DIR", &state_dir)
        .env("INKENTRY_TEST_DISCOVERY_PORT", &discovery_port)
        .arg("--detach-embed")
        .assert()
        .success();

    let log = project.path().join("index-background.log");
    let body = wait_for_log_containing(&log, "background embed finished");

    assert!(
        body.contains("=== inkentry index . --_embed-phases"),
        "the header names the run that wrote the file:\n{body}"
    );
    assert!(
        body.contains("background embed started (pid "),
        "a started line with a pid is what says the worker exists:\n{body}"
    );
    assert!(
        body.lines()
            .any(|l| l.contains("embedding: ") && l.contains(" chunks (")),
        "progress lines are what say it is still moving:\n{body}"
    );
    assert!(
        !body.contains('\u{1b}'),
        "a file sink gets no terminal escapes, on any platform:\n{body}"
    );
}

#[test]
fn a_continuation_worker_that_stops_early_reports_the_reason() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path());
    // `server_url` without `project_id` fails config validation before any phase begins, so the
    // reason must reach the log from the wrapper around the whole run.
    let inkentry_dir = project.path().join(".inkentry");
    std::fs::create_dir_all(&inkentry_dir).expect("create .inkentry dir");
    std::fs::write(
        inkentry_dir.join("config.toml"),
        "server_url = \"https://cloud.invalid.example:1\"\n",
    )
    .expect("write project config");

    let db = project.path().join("index.db");
    let out = index_cmd(home.path(), project.path(), &db)
        .arg("--_embed-phases")
        .output()
        .expect("run the continuation mode directly");

    // The child's stderr is the log file itself, so it is what the file would hold.
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "the run failed: {err}");
    assert!(
        err.contains("background embed started (pid "),
        "the start line lands before the work does:\n{err}"
    );
    assert!(
        err.contains("background embed failed: server_url is set but project_id is missing"),
        "the reason a user came looking for:\n{err}"
    );
}
