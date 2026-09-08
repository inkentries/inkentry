// Regression tests for `inkentry index`'s primary embed phase local-vs-remote
// tier routing: the foreground embed phase (`index/mod.rs`'s phase 2) and
// the `--detach-embed` worker it can hand off to.
//
// Mirrors the loopback-vs-explicit-`server_url` routing bug already fixed
// for `memory search` / `memory add` / `memory reindex` et al: under the
// default `local_first` mode, inference must always prefer the local
// loopback embedder, even when an explicit (here, deliberately unroutable)
// `server_url` is configured. `cloud_first` is the one mode where an
// explicit `server_url` legitimately serves inference too (test 2 is a
// regression guard for that path).
//
// The mock loopback server is wired in via loopback auto-discovery
// (real auto-discovery), not `server_url`, so a routing regression surfaces
// as a genuine connection/DNS failure against the deliberately-unroutable
// `server_url` rather than a silently-passing test.

use crate::plumbing_helpers;
use plumbing_helpers::{FIXTURE_PROJECT_ID, inkentry_bin_in, mount_health, mount_index_embed};

use std::path::Path;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// Failsafe only: hit solely if the detached child never finishes.
const CHILD_TIMEOUT: Duration = Duration::from_secs(60);

// ── fixture project ───────────────────────────────────────────────────────

// A tiny project: enough source for a couple of chunks, so the embed phase
// has real work without slowing the suite down.
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

// Write `<project_dir>/.inkentry/config.toml` with `server_url` + `project_id`.
//
// `ProjectConfig` (`inkentry-core/src/config/mod.rs`) only deserializes
// `server_url`/`project_id`/`server_ca`/`index` from this file; any other
// key (notably `mode`) is silently dropped by serde. `mode` must go through
// `INKENTRY_MODE` (or the personal global `--config` file) instead.
fn write_server_config(project_dir: &Path, server_url: &str) {
    let inkentry_dir = project_dir.join(".inkentry");
    std::fs::create_dir_all(&inkentry_dir).expect("create .inkentry dir");
    let cfg = format!("server_url = {server_url:?}\nproject_id = {FIXTURE_PROJECT_ID:?}\n");
    std::fs::write(inkentry_dir.join("config.toml"), cfg).expect("write project config");
}

// Point loopback auto-discovery at `url` and return the port to hand its
// fixed-port fallback (step 3b) through `INKENTRY_TEST_DISCOVERY_PORT`.
//
// Not the `server.port` file (step 3a): that step now uses a responder only
// when the pid recorded beside the port is a live `inkentry-server` process and
// the instance id it reports is the recorded one, neither of which a wiremock
// stand-in can be. The state dir is still created and still redirected, so
// nothing here reaches the developer's own daemon or state.
fn loopback_discovery_port(state_dir: &Path, url: &str) -> String {
    std::fs::create_dir_all(state_dir).expect("create state dir");
    url.rsplit(':')
        .next()
        .expect("uri has a port")
        .trim_end_matches('/')
        .to_string()
}

// `GET /v1/health` reporting an embedder still `loading` (no `index.embed`
// capability advertised yet).
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

// Build a `inkentry index --db <db> .` command against `project`, defensively
// scrubbed of every `INKENTRY_*` env var these tests care about isolating
// (an ambient value in the developer/CI shell must never leak into the
// child and quietly change which tier gets probed). Callers add back
// exactly the env each scenario needs.
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

// ── foreground embed phase (mod.rs's phase 2) ────────────────────────────

// Test 1 (the routing bug): `local_first` (default) with an explicit
// unroutable `server_url` and a loopback mock present must embed via the
// loopback mock, never attempt the unroutable `server_url`.
#[tokio::test]
async fn local_first_foreground_embeds_via_loopback_not_unroutable_server_url() {
    let loopback = MockServer::start().await;
    mount_health(&loopback).await;
    mount_index_embed(&loopback).await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path());
    // Deliberately unroutable: local_first must never fall back to this. An
    // accidental fallback surfaces as a connection/DNS error, not a silent
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

// Test 2 (regression guard): `cloud_first` with an explicit `server_url`
// that DOES advertise `index.embed` must still route embedding to and
// succeed against that `server_url`, unchanged by this fix.
#[tokio::test]
async fn cloud_first_foreground_still_embeds_via_explicit_server_url() {
    let mock = MockServer::start().await;
    mount_health(&mock).await;
    mount_index_embed(&mock).await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path());
    write_server_config(project.path(), &mock.uri());
    // `mode` is not a recognized `.inkentry/config.toml` project-level field
    // (see `write_server_config`); set it via env so `cloud_first` actually
    // takes effect, rather than silently falling back to `local_first`.
    let state_dir = home.path().join("state"); // never written to: nothing recorded

    let db = project.path().join("index.db");
    index_cmd(home.path(), project.path(), &db)
        .env("INKENTRY_MODE", "cloud_first")
        // Defensive: an empty state dir with the fixed-port fallback disabled
        // means any accidental fallback to `local_first`'s loopback probe fails
        // loudly, instead of silently hitting a real inkentry-server daemon
        // that happens to be running on this machine's default port.
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

// Test 3 (unaffected): no `server_url` configured at all (pure loopback
// auto-discovery, the default no-team-server case) must embed via loopback
// exactly as before this fix.
#[tokio::test]
async fn no_server_url_configured_embeds_via_loopback_auto_discovery() {
    let loopback = MockServer::start().await;
    mount_health(&loopback).await;
    mount_index_embed(&loopback).await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path());
    // No `.inkentry/config.toml` at all: no server_url, no project_id.
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

// Test 4: explicit offline (`INKENTRY_NO_SERVER=1`) skips the embed phase and
// names the switch. It used to assert `inkentry server start` while setting the
// switch that makes starting one pointless; it now asserts that advice is
// absent. No server is contacted either way.
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

// Test 5 (unchanged): a loopback server present but with the embedder still
// `loading` at index time keeps the existing "still loading, skipped"
// notice for the foreground path.
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

// ── detached-worker path (--detach-embed) ─────────────────────────────────

// Test 6 (the routing bug, detached path): the same scenario as test 1, but
// through `--detach-embed`/`--_embed-phases`: the detached worker must poll
// and embed via the loopback mock, not the explicit unroutable `server_url`.
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

// ── background log contents (--detach-embed) ──────────────────────────────

// `init` points the user at `index-background.log` when it hands the embed
// pass to a detached worker. Detached from a terminal that worker used to say
// nothing at all until it was done, so the file the product names sat empty
// for the whole run and a working index read exactly like one that never
// started. These two tests pin the content that tells them apart.

// Poll `path` until it contains `needle`, returning the whole file. The
// detached child outlives the command that spawned it, so the lines arrive
// after the parent has already returned.
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

// Test 7: a detached embed leaves a start line, batch progress and a finish
// line in the log, not an empty file.
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

// Test 8: when the worker stops early, the log carries the reason. This is
// the moment a user actually opens the file, and the case where an empty one
// was most misleading.
#[test]
fn a_continuation_worker_that_stops_early_reports_the_reason() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path());
    // A `server_url` with no `project_id` fails config validation, which is
    // before any phase begins: the reason has to reach the log from the
    // wrapper around the whole run, not from the embed pass.
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

    // The child's stderr is the log file itself (see
    // `continuation::redirect_to_background_log`), so its stderr is what the
    // file would hold.
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
