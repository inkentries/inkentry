//! Shared helpers for plumbing command component tests.
//!
//! Every test that needs an indexed project DB should call
//! `index_fixture_project()`.  Tests that need no index still share helpers
//! for constructing `Command` instances.
#![allow(dead_code, unused_imports)]

use assert_cmd::Command;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

// Git-config isolation lives once in `inkentry_core::test_support`, reached
// here via the `test-support`-featured dev-dependency in this crate's
// Cargo.toml. `scripts/check-git-isolation.sh` enforces that a test file
// spawning `git` wires in `isolate_git_config`/`git_command`, however
// qualified, including through this re-export.
pub use inkentry_core::test_support::isolate_git_config;

// A spawned `inkentry` binary registers sqlite-vec for its own process, but a
// `rusqlite::Connection` opened here does not inherit that: without this, any
// query touching a `vec0` table (`embeddings`, memory vectors) fails, and an
// assertion reading through `unwrap_or(0)` misreports the error as "empty".
// `sqlite3_auto_extension` is process-global, so the `OnceLock` is what keeps
// concurrent tests in one binary from racing on it.
pub fn register_sqlite_vec() {
    static INIT: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    INIT.get_or_init(|| {
        #[allow(clippy::missing_transmute_annotations)]
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}

/// Create a git repo in `dir` with a fabricated identity and one initial
/// commit, isolated from the developer's ambient git config.
///
/// Every setup git step is asserted: a silent setup failure surfaces later as
/// an unrelated assertion on the command under test.
pub fn init_git_repo(dir: &Path) {
    isolate_git_config();
    let run = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    };
    run(&["init", "-q"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "Test"]);
    std::fs::write(dir.join("README.md"), "hello\n").expect("write README.md");
    run(&["add", "."]);
    run(&["commit", "-q", "-m", "initial commit"]);
}

/// Build a `inkentry` test command that never touches the OS keychain and never
/// reads or writes the developer's real `~/.config/inkentry`.
///
/// Every integration test spawns the real `inkentry` binary, and each spawned
/// process runs `Config::load`, which resolves a secret store. In auto mode
/// (`INKENTRY_SECRET_STORE` unset) that selects the OS keychain whenever one is
/// present — which on macOS is always — triggering a Keychain access prompt on
/// every test binary. "Always Allow" never sticks because each rebuilt test
/// binary is a fresh app to the Keychain ACL. CI (Linux, no Secret Service) is
/// silent only by accident of the auto fallback.
///
/// This constructor pins two things on the child process:
///
/// * `INKENTRY_SECRET_STORE=file` — force the plaintext file store, so a spawned
///   CLI can never reach the keychain. Production behaviour is unchanged: real
///   users still get the keychain in auto mode; only tests pin the backend.
/// * `HOME` / `XDG_CONFIG_HOME` redirected to a throwaway temp dir — so the file
///   store writes its `secrets.toml` into an isolated `~/.config/inkentry` under
///   the temp HOME, never the developer's real config dir.
///
/// The temp dir is intentionally leaked (its path is kept for the lifetime of
/// the process) so the child can read/write it after this function returns; the
/// OS reclaims it on the next reboot. Tests that already manage their own `HOME`
/// (e.g. for registry isolation) should use [`inkentry_bin_in`] instead so the
/// keychain pin composes with their existing home dir.
pub fn inkentry_bin() -> Command {
    let home = TempDir::new()
        .expect("create temp HOME for inkentry test command")
        .keep();
    inkentry_bin_in(&home)
}

/// Like [`inkentry_bin`], but uses the caller-supplied `home` directory as the
/// isolated `HOME` instead of allocating a throwaway temp dir.
///
/// Use this from tests that set `HOME` themselves (registry isolation, seeded
/// config under `<home>/.config/inkentry`, etc.). It applies the same keychain
/// pin (`INKENTRY_SECRET_STORE=file`) and home redirection so those tests stop
/// prompting for Keychain access while keeping their existing home semantics.
pub fn inkentry_bin_in(home: &Path) -> Command {
    let mut cmd = Command::cargo_bin("inkentry").unwrap();
    cmd.env("INKENTRY_SECRET_STORE", "file") // never the OS keychain in tests
        // Loopback auto-discovery step 3b probes a fixed port, so without this
        // a test run finds the developer's own daemon and sends it real
        // embedding work — invisible to `egress_containment`, which permits
        // loopback, and invisible to CI, where nothing listens (inkentry-oss^5).
        // Pointing INKENTRY_STATE_DIR at an empty dir does NOT cover this: that
        // only defeats step 3a, and 3b is what reaches off the test's world.
        .env("INKENTRY_TEST_DISCOVERY_PORT", "0")
        .env("HOME", home)
        // `inkentry_config_dir()` uses `dirs::home_dir()` (HOME on Unix); unset
        // XDG_CONFIG_HOME so the file store lands under `<home>/.config/inkentry`
        // and never the developer's real config dir.
        .env_remove("XDG_CONFIG_HOME")
        // `dirs::home_dir()` 6.x on Windows calls `SHGetKnownFolderPath` (a
        // Registry lookup) rather than reading `HOME`/`USERPROFILE`, so the
        // `HOME` redirect above is a no-op there: every subprocess this spawns
        // would otherwise land on the same real `%USERPROFILE%\.config\inkentry`,
        // and concurrent tests racing on one `secrets.toml` corrupt it (see the
        // identical, already-documented gap on `INKENTRY_STATE_DIR` in
        // `capability/probe.rs` and `INKENTRY_SCRIPTS_DIR` in `memory/add.rs`).
        // `INKENTRY_CONFIG_DIR` bypasses `dirs::home_dir()` entirely and works
        // identically on every platform.
        .env("INKENTRY_CONFIG_DIR", home.join(".config").join("inkentry"))
        // The HOME redirect above hides `~/.gitconfig` from the git this child
        // spawns, but an exported GIT_CONFIG_GLOBAL outranks HOME and would
        // still reach it. Not a Windows path, but git skips a scope whenever
        // its var is set, whatever the path resolves to.
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    cmd
}

/// Path (relative to the workspace root) of the synthetic fixture project.
pub const FIXTURE_DIR: &str = "tests/fixtures/simple-project";

/// Project ID used by the fixture mock server.
pub const FIXTURE_PROJECT_ID: &str = "test-org/test-project";

/// Build a `inkentry plumbing --db <db>` Command pre-configured to use the
/// given DB and config file.  Callers add the specific plumbing subcommand
/// args (e.g. `cmd.arg("cat-chunks").arg("src/lib.rs")`).
///
/// Note: `--db` is a flag on the `plumbing` subcommand, not the top-level
/// command.  The correct invocation shape is:
///   inkentry --config <cfg> plumbing --db <db> <subcommand> [args]
pub fn inkentry_cmd(db_path: &Path, config_path: &Path) -> Command {
    let mut cmd = inkentry_bin();
    cmd.arg("--config")
        .arg(config_path)
        .arg("plumbing")
        .arg("--db")
        .arg(db_path);
    cmd
}

/// Write a minimal config file pointing at `db_path` and an optional
/// API base URL.  Returns the config file path.
pub fn write_config(dir: &Path, db_path: &Path, api_base: &str) -> PathBuf {
    let cfg = format!(
        "db_path = {:?}\napi_base_url = {:?}\nllm_model = \"test-chat\"\n",
        db_path, api_base
    );
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, cfg).expect("write config");
    config_path
}

// Like `write_config` but also configures `server_url` + `project_id` for
// Tier 1 operation (server-based embedding during `inkentry index`).
//
// `Config::load` only honors `server_url`/`project_id` from a project-level
// `.inkentry/config.toml` (discovered by walking up from CWD) or
// `INKENTRY_SERVER_URL`/`INKENTRY_PROJECT_ID` env, never from the `--config`
// file, which is the global personal config. So this writes those two fields
// to `<project_dir>/.inkentry/config.toml` instead of the returned global
// file. The caller's `Command` must set `.current_dir(project_dir)` (or
// wherever `project_dir` resolves to) or the discovery walk will never find
// it.
pub fn write_config_with_server(
    dir: &Path,
    db_path: &Path,
    api_base: &str,
    server_url: &str,
    project_dir: &Path,
) -> PathBuf {
    let config_path = write_config(dir, db_path, api_base);
    write_project_server_config(project_dir, server_url, FIXTURE_PROJECT_ID);
    config_path
}

// Write `<project_dir>/.inkentry/config.toml` with `server_url` + `project_id`,
// the only config file `Config::load` honors those fields from (besides env).
// The caller's `Command` must set `.current_dir(project_dir)`.
//
// An empty `project_id` is omitted entirely (rather than written as `""`) so
// a loopback-only test that doesn't need one (see `Config::validate_with_project`)
// leaves `project_id` genuinely unset, not set to an empty string.
pub fn write_project_server_config(project_dir: &Path, server_url: &str, project_id: &str) {
    let inkentry_dir = project_dir.join(".inkentry");
    std::fs::create_dir_all(&inkentry_dir).expect("create .inkentry dir");
    let mut cfg = format!("server_url = {server_url:?}\n");
    if !project_id.is_empty() {
        cfg.push_str(&format!("project_id = {project_id:?}\n"));
    }
    std::fs::write(inkentry_dir.join("config.toml"), cfg).expect("write project config");
}

/// Dynamic responder for `POST /v1/projects/{id}/index/embed`.
///
/// Parses the request body `{"chunks":[{"chunk_id":"…","content":"…"}]}` and
/// returns the server's wire format: raw little-endian f32 bytes, one constant
/// 896-dim vector per request chunk in order (no chunk_id framing — the CLI maps
/// response[i] → request chunk[i] by position).
pub struct IndexEmbedResponder;

impl wiremock::Respond for IndexEmbedResponder {
    fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
        #[derive(serde::Deserialize)]
        struct ReqBody {
            chunks: Vec<serde_json::Value>,
        }

        let body: ReqBody =
            serde_json::from_slice(&request.body).unwrap_or(ReqBody { chunks: vec![] });

        let mut bytes = Vec::with_capacity(body.chunks.len() * 896 * 4);
        for _ in &body.chunks {
            for _ in 0..896 {
                bytes.extend_from_slice(&0.1f32.to_le_bytes());
            }
        }

        wiremock::ResponseTemplate::new(200)
            .insert_header("content-type", "application/octet-stream")
            .set_body_bytes(bytes)
    }
}

/// Build the SSE body `ServerInferenceClient::llm_complete` expects: one
/// `token` event carrying the whole payload, then a `done` terminator.
/// Event boundary is `\n\n`, with a `data: ` prefix per line.
pub fn sse_token_response(content: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\n",
        serde_json::json!({"kind": "token", "content": content}),
        serde_json::json!({"kind": "done"}),
    )
}

/// Mount `GET /v1/health` advertising the full Tier 1 capability set.
pub async fn mount_health(server: &wiremock::MockServer) {
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/v1/health"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ok",
                "version": "test",
                // `llm.complete` is what LLM routing keys on; a legacy
                // capability must never stand in for it.
                "capabilities": [
                    "memory", "index.embed", "search.semantic", "plan", "llm.complete"
                ],
            })),
        )
        .mount(server)
        .await;
}

/// Mount `POST /v1/projects/{id}/index/embed` with [`IndexEmbedResponder`], so
/// parsing/embedding succeeds and the summary pass is reached.
pub async fn mount_index_embed(server: &wiremock::MockServer) {
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path_regex(
            r"^/v1/projects/.+/index/embed$",
        ))
        .respond_with(IndexEmbedResponder)
        .mount(server)
        .await;
}

/// Run `inkentry index <fixture_dir>` backed by a mock inkentry-server.
///
/// The mock server handles:
/// - `GET /v1/health` — Tier 1 capability probe
/// - `POST /v1/embeddings` — legacy endpoint used by `inkentry plumbing embed`
/// - `POST /v1/projects/{id}/index/embed` — new Tier 1 index embedding
///
/// Returns `(TempDir, db_path, config_path)`.  The `TempDir` must be kept
/// alive for the duration of the test.
pub fn index_fixture_project() -> (TempDir, PathBuf, PathBuf) {
    // Resolve the fixture directory relative to the workspace root.
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_DIR);
    index_project_dir(&fixture)
}

/// Like [`index_fixture_project`], but indexes an arbitrary project directory
/// instead of the shared `tests/fixtures/simple-project` fixture. Useful for
/// tests that need full control over the exact source files being indexed
/// (e.g. secret-scanner regression tests).
///
/// Returns `(TempDir, db_path, config_path)`.  The `TempDir` must be kept
/// alive for the duration of the test.
pub fn index_project_dir(project_dir: &Path) -> (TempDir, PathBuf, PathBuf) {
    let tmp = TempDir::new().expect("create temp dir");
    // Keep the index under `<tmp>/.inkentry/` so `<tmp>` is a real project that a
    // bare (no `--db`) command run from that CWD discovers (ADR-067). Plumbing
    // callers pass the returned `db_path` via `--db`, so the location is
    // transparent to them.
    std::fs::create_dir_all(tmp.path().join(".inkentry")).expect("create .inkentry");
    let db_path = tmp.path().join(".inkentry").join("index.db");

    let rt = tokio::runtime::Runtime::new().unwrap();
    let _mock_server = rt.block_on(async {
        use wiremock::matchers::{method, path, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        // Health probe — returns full Tier 1 capability set.
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ok",
                "version": "test",
                // `llm.complete` is what LLM routing keys on; a legacy
                // capability must never stand in for it.
                "capabilities": [
                    "memory", "index.embed", "search.semantic", "plan", "llm.complete"
                ],
            })))
            .mount(&server)
            .await;

        // Legacy /v1/embeddings — used by `inkentry plumbing embed --query`.
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{ "embedding": vec![0.1f32; 896], "index": 0 }],
                "model": "test-model",
                "object": "list",
                "usage": { "prompt_tokens": 5, "total_tokens": 5 },
            })))
            .mount(&server)
            .await;

        // New Tier 1 index/embed — echoes back chunk_ids with constant vectors.
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/index/embed$"))
            .respond_with(IndexEmbedResponder)
            .mount(&server)
            .await;

        server
    });

    let mock_url = _mock_server.uri();
    let config_path =
        write_config_with_server(tmp.path(), &db_path, &mock_url, &mock_url, tmp.path());

    // Pass `--db` explicitly so the index is written to our temp DB path,
    // not to `<project_dir>/.inkentry/index.db` (the default project-local location).
    // `.current_dir(tmp.path())`: the project-level config discovery walks up
    // from CWD, not from the `project_dir` positional arg (which may be an
    // unrelated source tree, e.g. the shared fixture).
    //
    // `INKENTRY_MODE=cloud_first`: this fixture's whole point is a Tier 1
    // index with real embeddings landed via the mock `server_url` above, not
    // exercising local-vs-remote routing. Under the default `local_first`
    // mode an explicit `server_url` with no loopback embedder configured is
    // now correctly refused, which would leave chunks unembedded and every
    // KNN-dependent consumer of this fixture broken. `.inkentry/config.toml`
    // doesn't recognize a `mode` key (see `write_project_server_config`), so
    // this must go through the env var.
    inkentry_bin_in(tmp.path())
        .current_dir(tmp.path())
        .env("INKENTRY_MODE", "cloud_first")
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg("--db")
        .arg(&db_path)
        .arg(project_dir)
        .assert()
        .success();

    (tmp, db_path, config_path)
}

/// Absolute path to the fixture project used by `index_fixture_project()`.
pub fn fixture_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_DIR)
}

/// Parse every line of `stdout` as JSON; return the parsed values.
/// Panics if any line is not valid JSON.
pub fn parse_jsonl(stdout: &[u8]) -> Vec<serde_json::Value> {
    let text = std::str::from_utf8(stdout).expect("stdout is utf-8");
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("invalid JSON line {l:?}: {e}")))
        .collect()
}

// ── Team-server (push/pull) scaffolding ──────────────────────────────────────
//
// `inkentry plumbing push`/`pull` speak to an explicit team `server_url` over
// `POST /memory/batch` and `GET /memory/since`. These helpers stand up a mock of
// that server and a local project seeded with real notes, so the transfer
// commands can be driven end to end through the compiled binary. They mirror the
// proven setup in `memory_push_sync_total_failure.rs`.

// Cloud project slug for the push/pull mock tests. Chosen so
// `encode_project_id` percent-encodes none of it, letting the mocked route
// paths be matched literally.
pub const TEAM_PROJECT_SLUG: &str = "acme-widget";

// Mount `GET /v1/health` advertising a minimal Tier 1 memory server.
// `require_tier1` only checks `tier.is_server()`, so a bare `memory` capability
// is enough to unlock push/pull; omitting `accepts_pushed_vectors` keeps the
// push text-only (no local embedder needed in the test).
pub async fn mount_team_health(server: &wiremock::MockServer) {
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/v1/health"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ok",
                "capabilities": ["memory"],
            })),
        )
        .mount(server)
        .await;
}

// Mount `POST /v1/projects/{TEAM_PROJECT_SLUG}/memory/batch` with a fixed
// `BatchPushResult` body.
pub async fn mount_memory_batch(server: &wiremock::MockServer, body: serde_json::Value) {
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path(format!(
            "/v1/projects/{TEAM_PROJECT_SLUG}/memory/batch"
        )))
        .respond_with(wiremock::ResponseTemplate::new(207).set_body_json(body))
        .mount(server)
        .await;
}

// Mount `GET /v1/projects/{TEAM_PROJECT_SLUG}/memory/since` (the `?since_id=`
// cursor mode `pull_since` uses) returning a fixed `{entries: [...]}` envelope.
pub async fn mount_memory_since(server: &wiremock::MockServer, body: serde_json::Value) {
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path_regex(format!(
            r"^/v1/projects/{TEAM_PROJECT_SLUG}/memory/since$"
        )))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

// Write a global config (db_path + a placeholder llm_model) plus a project-level
// `.inkentry/config.toml` carrying `server_url` + `TEAM_PROJECT_SLUG`. The
// caller's `Command` must run with `.current_dir(dir)` so project discovery
// finds the `.inkentry/config.toml`. Returns the global config path.
pub fn write_team_config(dir: &Path, server_url: &str) -> PathBuf {
    let db_path = dir.join(".inkentry").join("index.db");
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!("db_path = {db_path:?}\nllm_model = \"test-chat\"\n"),
    )
    .expect("write config.toml");
    write_project_server_config(dir, server_url, TEAM_PROJECT_SLUG);
    config_path
}

// Create the `.inkentry/` marker so the fail-closed project gate resolves `dir`
// as a real local project.
pub fn init_local_project(dir: &Path) {
    std::fs::create_dir_all(dir.join(".inkentry")).expect("create .inkentry");
}

// Seed one local memory entry via a real `inkentry memory add` subprocess, so a
// subsequent push/pull has something to operate on.
pub fn seed_memory_note(home: &Path, proj: &Path, config_path: &Path, title: &str) {
    inkentry_bin_in(home)
        .current_dir(proj)
        .arg("--config")
        .arg(config_path)
        .args([
            "memory", "add", "--kind", "note", "--title", title, "--body", "seeded",
        ])
        .assert()
        .success();
}
