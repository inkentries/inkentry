#![allow(dead_code, unused_imports)]

use assert_cmd::Command;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

// scripts/check-git-isolation.sh requires test files that spawn `git` to wire this in, including via this re-export.
pub use inkentry_core::test_support::isolate_git_config;

// A spawned binary registers sqlite-vec itself; a Connection opened here does not,
// and a failing vec0 query read through `unwrap_or(0)` would misreport as "empty".
// The OnceLock keeps concurrent tests from racing on the process-global auto_extension.
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

// Pins INKENTRY_SECRET_STORE=file and a throwaway HOME so a spawned CLI neither prompts the
// macOS Keychain (each rebuilt test binary is a fresh app, so "Always Allow" never sticks)
// nor touches the developer's real ~/.config/inkentry. The temp dir is leaked so the child
// can use it after this returns.
pub fn inkentry_bin() -> Command {
    let home = TempDir::new()
        .expect("create temp HOME for inkentry test command")
        .keep();
    inkentry_bin_in(&home)
}

pub fn inkentry_bin_in(home: &Path) -> Command {
    let mut cmd = Command::cargo_bin("inkentry").unwrap();
    cmd.env("INKENTRY_SECRET_STORE", "file")
        // Loopback discovery step 3b probes a fixed port and would find the developer's own daemon;
        // INKENTRY_STATE_DIR only defeats step 3a.
        .env("INKENTRY_TEST_DISCOVERY_PORT", "0")
        .env("HOME", home)
        // Unset so the file store lands under `<home>/.config/inkentry`.
        .env_remove("XDG_CONFIG_HOME")
        // `dirs::home_dir()` on Windows ignores HOME; INKENTRY_CONFIG_DIR isolates config on every platform.
        .env("INKENTRY_CONFIG_DIR", home.join(".config").join("inkentry"))
        // An exported GIT_CONFIG_GLOBAL outranks the HOME redirect and would still reach ~/.gitconfig.
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    // Config::load walks up from CWD; under nextest that is inside the repo, whose dogfood
    // config pins `cloud = true` and would hit the live API. Pin CWD to the isolated home.
    cmd.current_dir(home);
    cmd
}

pub const FIXTURE_DIR: &str = "tests/fixtures/simple-project";

pub const FIXTURE_PROJECT_ID: &str = "test-org/test-project";

pub fn inkentry_cmd(db_path: &Path, config_path: &Path) -> Command {
    let mut cmd = inkentry_bin();
    cmd.arg("--config")
        .arg(config_path)
        .arg("plumbing")
        .arg("--db")
        .arg(db_path);
    cmd
}

pub fn write_config(dir: &Path, db_path: &Path, api_base: &str) -> PathBuf {
    let cfg = format!(
        "db_path = {:?}\napi_base_url = {:?}\nllm_model = \"test-chat\"\n",
        db_path, api_base
    );
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, cfg).expect("write config");
    config_path
}

// Config::load honors `server_url`/`project_id` only from a project-level
// `.inkentry/config.toml` (found by walking up from CWD) or env, never the `--config` file,
// so the caller's Command must set `.current_dir(project_dir)`.
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

// The caller's Command must set `.current_dir(project_dir)`. An empty `project_id` is
// omitted rather than written as `""`, so it stays genuinely unset.
pub fn write_project_server_config(project_dir: &Path, server_url: &str, project_id: &str) {
    let inkentry_dir = project_dir.join(".inkentry");
    std::fs::create_dir_all(&inkentry_dir).expect("create .inkentry dir");
    let mut cfg = format!("server_url = {server_url:?}\n");
    if !project_id.is_empty() {
        cfg.push_str(&format!("project_id = {project_id:?}\n"));
    }
    std::fs::write(inkentry_dir.join("config.toml"), cfg).expect("write project config");
}

// Returns raw little-endian f32 bytes: one constant 896-dim vector per request chunk, matched
// to it by position (no chunk_id framing).
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

pub fn sse_token_response(content: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\n",
        serde_json::json!({"kind": "token", "content": content}),
        serde_json::json!({"kind": "done"}),
    )
}

pub async fn mount_health(server: &wiremock::MockServer) {
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/v1/health"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ok",
                "version": "test",
                // LLM routing keys on `llm.complete` alone; a legacy capability must not stand in for it.
                "capabilities": [
                    "memory", "index.embed", "search.semantic", "plan", "llm.complete"
                ],
            })),
        )
        .mount(server)
        .await;
}

pub async fn mount_index_embed(server: &wiremock::MockServer) {
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path_regex(
            r"^/v1/projects/.+/index/embed$",
        ))
        .respond_with(IndexEmbedResponder)
        .mount(server)
        .await;
}

// The returned TempDir must outlive the test.
pub fn index_fixture_project() -> (TempDir, PathBuf, PathBuf) {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_DIR);
    index_project_dir(&fixture)
}

pub fn index_project_dir(project_dir: &Path) -> (TempDir, PathBuf, PathBuf) {
    let tmp = TempDir::new().expect("create temp dir");
    // Under `<tmp>/.inkentry/` so a bare command run from `<tmp>` discovers the index.
    std::fs::create_dir_all(tmp.path().join(".inkentry")).expect("create .inkentry");
    let db_path = tmp.path().join(".inkentry").join("index.db");

    let rt = tokio::runtime::Runtime::new().unwrap();
    let _mock_server = rt.block_on(async {
        use wiremock::matchers::{method, path, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ok",
                "version": "test",
                // LLM routing keys on `llm.complete` alone; a legacy capability must not stand in for it.
                "capabilities": [
                    "memory", "index.embed", "search.semantic", "plan", "llm.complete"
                ],
            })))
            .mount(&server)
            .await;

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

    // `--db` pins the index to our temp path. `.current_dir(tmp)`: config discovery walks up
    // from CWD, not from the `project_dir` positional arg.
    // INKENTRY_MODE=cloud_first: under local_first an explicit `server_url` with no loopback
    // embedder is refused, leaving chunks unembedded. `.inkentry/config.toml` has no `mode`
    // key, so it must go through the env var.
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

pub fn fixture_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_DIR)
}

pub fn parse_jsonl(stdout: &[u8]) -> Vec<serde_json::Value> {
    let text = std::str::from_utf8(stdout).expect("stdout is utf-8");
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("invalid JSON line {l:?}: {e}")))
        .collect()
}

// Slug chosen so `encode_project_id` percent-encodes none of it and mocked paths match literally.
pub const TEAM_PROJECT_SLUG: &str = "acme-widget";

// A bare `memory` capability unlocks push/pull (`require_tier1` checks only `tier.is_server()`);
// omitting `accepts_pushed_vectors` keeps push text-only, so no local embedder is needed.
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

pub async fn mount_memory_batch(server: &wiremock::MockServer, body: serde_json::Value) {
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path(format!(
            "/v1/projects/{TEAM_PROJECT_SLUG}/memory/batch"
        )))
        .respond_with(wiremock::ResponseTemplate::new(207).set_body_json(body))
        .mount(server)
        .await;
}

pub async fn mount_memory_since(server: &wiremock::MockServer, body: serde_json::Value) {
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path_regex(format!(
            r"^/v1/projects/{TEAM_PROJECT_SLUG}/memory/since$"
        )))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

// The caller's Command must run with `.current_dir(dir)` so project discovery finds
// `.inkentry/config.toml`.
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

// The `.inkentry/` marker is what the fail-closed project gate needs to treat `dir` as a local project.
pub fn init_local_project(dir: &Path) {
    std::fs::create_dir_all(dir.join(".inkentry")).expect("create .inkentry");
}

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
