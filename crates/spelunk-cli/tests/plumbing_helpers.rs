//! Shared helpers for plumbing command component tests.
//!
//! Every test that needs an indexed project DB should call
//! `index_fixture_project()`.  Tests that need no index still share helpers
//! for constructing `Command` instances.
#![allow(dead_code)]

use assert_cmd::Command;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Build a `spelunk` test command that never touches the OS keychain and never
/// reads or writes the developer's real `~/.config/spelunk`.
///
/// Every integration test spawns the real `spelunk` binary, and each spawned
/// process runs `Config::load`, which resolves a secret store. In auto mode
/// (`SPELUNK_SECRET_STORE` unset) that selects the OS keychain whenever one is
/// present — which on macOS is always — triggering a Keychain access prompt on
/// every test binary. "Always Allow" never sticks because each rebuilt test
/// binary is a fresh app to the Keychain ACL. CI (Linux, no Secret Service) is
/// silent only by accident of the auto fallback.
///
/// This constructor pins two things on the child process:
///
/// * `SPELUNK_SECRET_STORE=file` — force the plaintext file store, so a spawned
///   CLI can never reach the keychain. Production behaviour is unchanged: real
///   users still get the keychain in auto mode; only tests pin the backend.
/// * `HOME` / `XDG_CONFIG_HOME` redirected to a throwaway temp dir — so the file
///   store writes its `secrets.toml` into an isolated `~/.config/spelunk` under
///   the temp HOME, never the developer's real config dir.
///
/// The temp dir is intentionally leaked (its path is kept for the lifetime of
/// the process) so the child can read/write it after this function returns; the
/// OS reclaims it on the next reboot. Tests that already manage their own `HOME`
/// (e.g. for registry isolation) should use [`spelunk_bin_in`] instead so the
/// keychain pin composes with their existing home dir.
pub fn spelunk_bin() -> Command {
    let home = TempDir::new()
        .expect("create temp HOME for spelunk test command")
        .keep();
    spelunk_bin_in(&home)
}

/// Like [`spelunk_bin`], but uses the caller-supplied `home` directory as the
/// isolated `HOME` instead of allocating a throwaway temp dir.
///
/// Use this from tests that set `HOME` themselves (registry isolation, seeded
/// config under `<home>/.config/spelunk`, etc.). It applies the same keychain
/// pin (`SPELUNK_SECRET_STORE=file`) and home redirection so those tests stop
/// prompting for Keychain access while keeping their existing home semantics.
pub fn spelunk_bin_in(home: &Path) -> Command {
    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.env("SPELUNK_SECRET_STORE", "file") // never the OS keychain in tests
        .env("HOME", home)
        // `spelunk_config_dir()` uses `dirs::home_dir()` (HOME on Unix); unset
        // XDG_CONFIG_HOME so the file store lands under `<home>/.config/spelunk`
        // and never the developer's real config dir.
        .env_remove("XDG_CONFIG_HOME");
    cmd
}

/// Path (relative to the workspace root) of the synthetic fixture project.
pub const FIXTURE_DIR: &str = "tests/fixtures/simple-project";

/// Project ID used by the fixture mock server.
pub const FIXTURE_PROJECT_ID: &str = "test-org/test-project";

/// Build a `spelunk plumbing --db <db>` Command pre-configured to use the
/// given DB and config file.  Callers add the specific plumbing subcommand
/// args (e.g. `cmd.arg("cat-chunks").arg("src/lib.rs")`).
///
/// Note: `--db` is a flag on the `plumbing` subcommand, not the top-level
/// command.  The correct invocation shape is:
///   spelunk --config <cfg> plumbing --db <db> <subcommand> [args]
pub fn spelunk_cmd(db_path: &Path, config_path: &Path) -> Command {
    let mut cmd = spelunk_bin();
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
        "db_path = {:?}\napi_base_url = {:?}\nembedding_model = \"test-model\"\nllm_model = \"test-chat\"\n",
        db_path, api_base
    );
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, cfg).expect("write config");
    config_path
}

/// Like `write_config` but also configures `server_url` + `project_id` for
/// Tier 1 operation (server-based embedding during `spelunk index`).
pub fn write_config_with_server(
    dir: &Path,
    db_path: &Path,
    api_base: &str,
    server_url: &str,
) -> PathBuf {
    let cfg = format!(
        concat!(
            "db_path = {:?}\n",
            "api_base_url = {:?}\n",
            "embedding_model = \"test-model\"\n",
            "llm_model = \"test-chat\"\n",
            "server_url = {:?}\n",
            "project_id = {:?}\n",
        ),
        db_path, api_base, server_url, FIXTURE_PROJECT_ID,
    );
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, cfg).expect("write config");
    config_path
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
                "capabilities": ["memory", "index.embed", "search.semantic", "explore", "plan"],
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

/// Run `spelunk index <fixture_dir>` backed by a mock spelunk-server.
///
/// The mock server handles:
/// - `GET /v1/health` — Tier 1 capability probe
/// - `POST /v1/embeddings` — legacy endpoint used by `spelunk plumbing embed`
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
    // Keep the index under `<tmp>/.spelunk/` so `<tmp>` is a real project that a
    // bare (no `--db`) command run from that CWD discovers (ADR-067). Plumbing
    // callers pass the returned `db_path` via `--db`, so the location is
    // transparent to them.
    std::fs::create_dir_all(tmp.path().join(".spelunk")).expect("create .spelunk");
    let db_path = tmp.path().join(".spelunk").join("index.db");

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
                "capabilities": ["memory", "index.embed", "search.semantic", "explore", "plan"],
            })))
            .mount(&server)
            .await;

        // Legacy /v1/embeddings — used by `spelunk plumbing embed --query`.
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
    let config_path = write_config_with_server(tmp.path(), &db_path, &mock_url, &mock_url);

    // Pass `--db` explicitly so the index is written to our temp DB path,
    // not to `<project_dir>/.spelunk/index.db` (the default project-local location).
    spelunk_bin_in(tmp.path())
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
