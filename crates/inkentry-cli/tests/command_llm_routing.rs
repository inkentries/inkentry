// LLM routing for `inkentry memory harvest`.
//
// Harvest uses one inference client for two concerns: LLM completion and the
// embedding it needs for dedup vectors. Once LLM and embed can resolve to
// different servers, one client is wrong, so these tests assert on which mock
// received which route, not only on the outcome.
//
// Unlike `index` summaries, harvest cannot do its job without an LLM, so an
// unavailable LLM is an error with a non-zero exit here.

mod plumbing_helpers;
use plumbing_helpers::{
    FIXTURE_PROJECT_ID, IndexEmbedResponder, inkentry_bin_in, isolate_git_config,
    sse_token_response,
};

use std::path::Path;
use tempfile::TempDir;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ── mocks ─────────────────────────────────────────────────────────────────

fn health_body(llm: bool) -> serde_json::Value {
    let mut caps = vec!["memory", "index.embed", "search.semantic"];
    if llm {
        caps.push("llm.complete");
    }
    serde_json::json!({
        "status": "ok",
        "version": "test",
        "capabilities": caps,
        "embedding_dim": 896,
        "embedder": { "state": "ready", "detail": null },
    })
}

// A inkentry-server mock. `llm_payload` is the text the `/llm/complete` SSE
// stream carries; it is only mounted when the server advertises an LLM, so a
// server without one cannot accidentally answer a misrouted request.
async fn server_mock(llm_payload: Option<String>) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(health_body(llm_payload.is_some())))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/projects/.+/index/embed$"))
        .respond_with(IndexEmbedResponder)
        .mount(&server)
        .await;
    if let Some(payload) = llm_payload {
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/projects/.+/llm/complete$"))
            .respond_with(move |_: &wiremock::Request| {
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_token_response(&payload))
            })
            .mount(&server)
            .await;
    }
    server
}

async fn count_path(server: &MockServer, needle: &str) -> usize {
    server
        .received_requests()
        .await
        .expect("requests recorded")
        .iter()
        .filter(|r| r.url.path().contains(needle))
        .count()
}

// ── project fixtures ──────────────────────────────────────────────────────

fn write_loopback_state(state_dir: &Path, url: &str) {
    std::fs::create_dir_all(state_dir).expect("create state dir");
    let port: u16 = url
        .rsplit(':')
        .next()
        .expect("uri has a port")
        .trim_end_matches('/')
        .parse()
        .expect("uri port is numeric");
    std::fs::write(state_dir.join("server.port"), format!("{port}\n")).expect("write server.port");
}

fn write_server_config(project_dir: &Path, server_url: &str) {
    let inkentry_dir = project_dir.join(".inkentry");
    std::fs::create_dir_all(&inkentry_dir).expect("create .inkentry dir");
    std::fs::write(
        inkentry_dir.join("config.toml"),
        format!("server_url = {server_url:?}\nproject_id = {FIXTURE_PROJECT_ID:?}\n"),
    )
    .expect("write project config");
}

// A git project with one substantive commit, so harvest has something to
// extract.
fn write_git_project(dir: &Path) {
    isolate_git_config();
    let run = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    };
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).expect("create src dir");
    std::fs::write(
        src.join("lib.rs"),
        "pub fn greet(name: &str) -> String {\n    format!(\"hello, {name}\")\n}\n",
    )
    .expect("write lib.rs");
    run(&["init", "-q"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "Test"]);
    run(&["add", "."]);
    run(&[
        "commit",
        "-q",
        "-m",
        "feat: choose sqlite over postgres for the local index",
    ]);
}

fn base_cmd(home: &Path, project: &Path) -> assert_cmd::Command {
    let mut cmd = inkentry_bin_in(home);
    cmd.current_dir(project)
        .env_remove("INKENTRY_SERVER_URL")
        .env_remove("INKENTRY_MODE")
        .env_remove("INKENTRY_PROJECT_ID")
        .env_remove("INKENTRY_NO_SERVER")
        .env_remove("INKENTRY_STATE_DIR")
        .env_remove("INKENTRY_LLM_URL")
        .env_remove("INKENTRY_LLM_MODEL");
    cmd
}

// Build an index so project-scoped commands have a `.inkentry/` project and a
// db to work from. Offline, so it contacts nothing.
fn seed_index(home: &Path, project: &Path, db: &Path) {
    base_cmd(home, project)
        .env("INKENTRY_NO_SERVER", "1")
        .arg("index")
        .arg("--db")
        .arg(db)
        .arg("--no-summaries")
        .arg(".")
        .assert()
        .success();
}

fn combined(output: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_no_internal_names(output: &str) {
    for jargon in [
        "ServerInferenceClient",
        "ServerLlmClient",
        "ServerLlmAdapter",
        "ServerEmbedAdapter",
        "Capabilities",
        "inference_url",
        "llm.complete",
    ] {
        assert!(
            !output.contains(jargon),
            "user-facing output leaked {jargon:?}:\n{output}"
        );
    }
}

// A harvest extraction reply matching the command's JSON schema.
fn harvest_payload() -> String {
    serde_json::json!({
        "entries": [{
            "sha": "HEAD",
            "kind": "decision",
            "title": "Use sqlite for the local index",
            "body": "Chosen over postgres to keep the local setup dependency free.",
            "tags": ["storage"],
        }]
    })
    .to_string()
}

// ── inkentry memory harvest ────────────────────────────────────────────────

fn harvest_cmd(home: &Path, project: &Path, db: &Path, state_dir: &Path) -> assert_cmd::Command {
    let mut cmd = base_cmd(home, project);
    cmd.env("INKENTRY_STATE_DIR", state_dir)
        .arg("memory")
        .arg("harvest")
        .arg("--db")
        .arg(db)
        .arg("--branch")
        .arg("HEAD");
    cmd
}

#[tokio::test]
async fn harvest_uses_the_loopback_for_both_extraction_and_dedup_embedding() {
    let loopback = server_mock(Some(harvest_payload())).await;
    let remote = server_mock(Some("SHOULD NOT BE USED".to_string())).await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_git_project(project.path());
    let db = project.path().join("index.db");
    seed_index(home.path(), project.path(), &db);
    write_server_config(project.path(), &remote.uri());
    let state_dir = home.path().join("state");
    write_loopback_state(&state_dir, &loopback.uri());

    let mem = project.path().join("memory.db");
    let output = harvest_cmd(home.path(), project.path(), &mem, &state_dir)
        .output()
        .expect("run harvest");
    let text = combined(&output);

    assert!(
        count_path(&loopback, "/llm/complete").await > 0,
        "extraction must run on the local LLM:\n{text}"
    );
    assert_eq!(
        count_path(&remote, "/llm/complete").await,
        0,
        "a usable local LLM must not send commit content to the remote:\n{text}"
    );
}

// The split, observable: extraction on the remote, dedup vectors on the
// loopback, in the same command.
#[tokio::test]
async fn harvest_splits_extraction_to_the_remote_and_embedding_to_the_loopback() {
    let loopback = server_mock(None).await;
    let remote = server_mock(Some(harvest_payload())).await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_git_project(project.path());
    let db = project.path().join("index.db");
    seed_index(home.path(), project.path(), &db);
    write_server_config(project.path(), &remote.uri());
    let state_dir = home.path().join("state");
    write_loopback_state(&state_dir, &loopback.uri());

    let mem = project.path().join("memory.db");
    let output = harvest_cmd(home.path(), project.path(), &mem, &state_dir)
        .output()
        .expect("run harvest");
    let text = combined(&output);

    assert!(
        count_path(&remote, "/llm/complete").await > 0,
        "the remote is the only LLM available and must serve extraction:\n{text}"
    );
    assert!(
        count_path(&loopback, "/index/embed").await > 0,
        "dedup vectors must still be embedded locally:\n{text}"
    );
    assert_eq!(
        count_path(&remote, "/index/embed").await,
        0,
        "routing extraction to the remote must not divert embedding there:\n{text}"
    );
}

#[tokio::test]
async fn harvest_stops_with_the_restart_message_when_the_local_llm_is_not_served() {
    let loopback = server_mock(None).await;
    let remote = server_mock(Some(harvest_payload())).await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_git_project(project.path());
    let db = project.path().join("index.db");
    seed_index(home.path(), project.path(), &db);
    write_server_config(project.path(), &remote.uri());
    let state_dir = home.path().join("state");
    write_loopback_state(&state_dir, &loopback.uri());

    let mem = project.path().join("memory.db");
    let output = harvest_cmd(home.path(), project.path(), &mem, &state_dir)
        .env("INKENTRY_LLM_URL", "http://127.0.0.1:1234")
        .output()
        .expect("run harvest");
    let text = combined(&output);

    assert!(!output.status.success(), "{text}");
    assert!(
        text.contains("inkentry server stop") && text.contains("inkentry server start"),
        "the restart is the only useful instruction here:\n{text}"
    );
    // The privacy guard rendered as prose: never nudge a user who asked for a
    // local LLM toward the remote this run deliberately avoided.
    assert!(
        !text.contains("server_url"),
        "the message must not offer the remote as a way out:\n{text}"
    );
    assert_eq!(
        count_path(&remote, "/llm/complete").await,
        0,
        "commit content must never reach a remote LLM the user did not choose:\n{text}"
    );
    assert_no_internal_names(&text);
}

#[tokio::test]
async fn harvest_stops_with_the_no_llm_message_when_none_is_available() {
    let loopback = server_mock(None).await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_git_project(project.path());
    let db = project.path().join("index.db");
    seed_index(home.path(), project.path(), &db);
    let state_dir = home.path().join("state");
    write_loopback_state(&state_dir, &loopback.uri());

    let mem = project.path().join("memory.db");
    let output = harvest_cmd(home.path(), project.path(), &mem, &state_dir)
        .output()
        .expect("run harvest");
    let text = combined(&output);

    assert!(!output.status.success(), "{text}");
    assert!(
        text.contains("llm_url"),
        "must offer the local route:\n{text}"
    );
    assert!(
        text.contains("server_url"),
        "must offer the remote route:\n{text}"
    );
    assert_no_internal_names(&text);
}

// Regression: the built-in default range `HEAD~10..HEAD` names `HEAD~10`, a
// commit that does not exist in a repo with fewer than 11 commits, so an
// unclamped range makes `git log` abort with a raw `fatal: bad revision`. The
// range must clamp to the commits that actually exist, and the single commit in
// this one-commit fixture must still be harvested. Uses the DEFAULT range (no
// `--branch`), unlike the routing tests above, so the clamp is what is under
// test.
#[tokio::test]
async fn harvest_clamps_the_default_range_on_a_shallow_repo() {
    let loopback = server_mock(Some(harvest_payload())).await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_git_project(project.path()); // one commit
    let db = project.path().join("index.db");
    seed_index(home.path(), project.path(), &db);
    let state_dir = home.path().join("state");
    write_loopback_state(&state_dir, &loopback.uri());

    let mem = project.path().join("memory.db");
    let output = base_cmd(home.path(), project.path())
        .env("INKENTRY_STATE_DIR", &state_dir)
        .arg("memory")
        .arg("harvest")
        .arg("--db")
        .arg(&mem)
        .output()
        .expect("run harvest");
    let text = combined(&output);

    assert!(
        !text.contains("bad revision"),
        "the default range must clamp on a shallow repo, not hit `bad revision`:\n{text}"
    );
    assert!(
        output.status.success(),
        "harvest must succeed on a one-commit repo:\n{text}"
    );
    assert!(
        count_path(&loopback, "/llm/complete").await > 0,
        "the single available commit must be sent to the LLM for extraction:\n{text}"
    );
}

// Regression: with no LLM available, `harvest` on a shallow repo must surface
// the actionable no-LLM message, never a raw `git log` `bad revision` error.
// The LLM precheck runs before the git range is resolved, so how many commits
// the repo has cannot change which message the user sees.
#[tokio::test]
async fn harvest_reports_no_llm_before_the_git_range_on_a_shallow_repo() {
    let loopback = server_mock(None).await; // embedding only, no LLM

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_git_project(project.path()); // one commit
    let db = project.path().join("index.db");
    seed_index(home.path(), project.path(), &db);
    let state_dir = home.path().join("state");
    write_loopback_state(&state_dir, &loopback.uri());

    let mem = project.path().join("memory.db");
    let output = base_cmd(home.path(), project.path())
        .env("INKENTRY_STATE_DIR", &state_dir)
        .arg("memory")
        .arg("harvest")
        .arg("--db")
        .arg(&mem)
        .output()
        .expect("run harvest");
    let text = combined(&output);

    assert!(!output.status.success(), "{text}");
    assert!(
        !text.contains("bad revision"),
        "the LLM precheck must fire before the git range resolves, so a shallow \
         repo never surfaces a raw git error:\n{text}"
    );
    assert!(
        text.contains("llm_url") && text.contains("server_url"),
        "the actionable no-LLM message must name both routes to an LLM:\n{text}"
    );
    assert_no_internal_names(&text);
}

// ── top-level `inkentry harvest` promotion ────────────────────────────────
//
// The promoted top-level command shares the handler and the memory-store
// resolution with the deprecated `memory harvest` alias. These pin the two
// promotion-specific guarantees the shared handler alone cannot: results
// identical to the alias, and the alias-only stderr deprecation warning.

fn toplevel_harvest_cmd(
    home: &Path,
    project: &Path,
    db: &Path,
    state_dir: &Path,
) -> assert_cmd::Command {
    let mut cmd = base_cmd(home, project);
    cmd.env("INKENTRY_STATE_DIR", state_dir)
        .arg("harvest")
        .arg("--db")
        .arg(db)
        .arg("--branch")
        .arg("HEAD");
    cmd
}

#[tokio::test]
async fn toplevel_harvest_matches_the_alias_and_only_the_alias_warns() {
    let loopback = server_mock(Some(harvest_payload())).await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_git_project(project.path());
    let db = project.path().join("index.db");
    seed_index(home.path(), project.path(), &db);
    let state_dir = home.path().join("state");
    write_loopback_state(&state_dir, &loopback.uri());

    // Two fresh memory stores so neither run sees the other's entries; both
    // start empty and harvest the same single commit, so stdout must match.
    let mem_top = project.path().join("memory_top.db");
    let top = toplevel_harvest_cmd(home.path(), project.path(), &mem_top, &state_dir)
        .output()
        .expect("run top-level harvest");

    let mem_alias = project.path().join("memory_alias.db");
    let alias = harvest_cmd(home.path(), project.path(), &mem_alias, &state_dir)
        .output()
        .expect("run memory harvest alias");

    let top_out = String::from_utf8_lossy(&top.stdout).to_string();
    let alias_out = String::from_utf8_lossy(&alias.stdout).to_string();
    let top_err = String::from_utf8_lossy(&top.stderr).to_string();
    let alias_err = String::from_utf8_lossy(&alias.stderr).to_string();

    assert!(
        top.status.success(),
        "top-level harvest failed:\n{top_out}\n{top_err}"
    );
    assert!(
        alias.status.success(),
        "alias harvest failed:\n{alias_out}\n{alias_err}"
    );

    // Both spellings store the harvested decision.
    assert!(
        top_out.contains("Stored 1 memory entries"),
        "top-level harvest must store the extracted entry:\n{top_out}"
    );

    // Identical stdout: the promotion is a rename, not a behaviour change.
    assert_eq!(
        top_out, alias_out,
        "the two spellings must produce byte-identical stdout"
    );

    // Exactly one deprecation line, on the alias's stderr only, naming the
    // canonical command; the top-level command must never warn.
    let alias_warnings = alias_err
        .lines()
        .filter(|l| l.contains("deprecated"))
        .count();
    assert_eq!(
        alias_warnings, 1,
        "the alias must emit exactly one deprecation warning:\n{alias_err}"
    );
    assert!(
        alias_err.contains("inkentry harvest"),
        "the deprecation warning must name the replacement command:\n{alias_err}"
    );
    assert!(
        !top_err.contains("deprecated"),
        "the top-level command must not emit a deprecation warning:\n{top_err}"
    );
}

// ── memory harvest --source failures ──────────────────────────────────────
//
// The third harvest source. It builds its clients at its own call site, so the
// git source passing says nothing about it, exactly as with claude-code.

// The failures harvester only looks at commits whose subject reads as a
// failure signal, so the fixture's feature commit alone yields an empty run
// that never reaches client construction.
fn write_failure_commit(dir: &Path) {
    std::fs::write(
        dir.join("src").join("guard.rs"),
        "pub fn guard(v: &[u8]) -> u8 {\n    *v.first().unwrap_or(&0)\n}\n",
    )
    .expect("write guard.rs");
    let run = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    };
    run(&["add", "."]);
    run(&[
        "commit",
        "-q",
        "-m",
        "fix: stop the indexer panicking on empty input",
    ]);
}

// The failures schema carries no `kind`, unlike the git source's.
fn failures_payload() -> String {
    serde_json::json!({
        "entries": [{
            "sha": "HEAD",
            "title": "Empty input panicked the indexer",
            "body": "Guard the first-element read instead of indexing directly.",
            "tags": ["reliability"],
        }]
    })
    .to_string()
}

fn failures_harvest_cmd(
    home: &Path,
    project: &Path,
    mem: &Path,
    state_dir: &Path,
) -> assert_cmd::Command {
    let mut cmd = base_cmd(home, project);
    cmd.env("INKENTRY_STATE_DIR", state_dir)
        .arg("memory")
        .arg("harvest")
        .arg("--db")
        .arg(mem)
        .arg("--source")
        .arg("failures")
        .arg("--branch")
        .arg("HEAD");
    cmd
}

#[tokio::test]
async fn failures_harvest_splits_extraction_to_the_remote_and_embedding_to_the_loopback() {
    let loopback = server_mock(None).await;
    let remote = server_mock(Some(failures_payload())).await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_git_project(project.path());
    write_failure_commit(project.path());
    let db = project.path().join("index.db");
    seed_index(home.path(), project.path(), &db);
    write_server_config(project.path(), &remote.uri());
    let state_dir = home.path().join("state");
    write_loopback_state(&state_dir, &loopback.uri());

    let mem = project.path().join("memory.db");
    let output = failures_harvest_cmd(home.path(), project.path(), &mem, &state_dir)
        .output()
        .expect("run harvest");
    let text = combined(&output);

    assert!(
        count_path(&remote, "/llm/complete").await > 0,
        "the remote is the only LLM available and must serve extraction:\n{text}"
    );
    assert!(
        count_path(&loopback, "/index/embed").await > 0,
        "dedup vectors must still be embedded locally:\n{text}"
    );
    assert_eq!(
        count_path(&remote, "/index/embed").await,
        0,
        "routing extraction to the remote must not divert embedding there:\n{text}"
    );
}

#[tokio::test]
async fn failures_harvest_stops_with_the_restart_message_when_the_local_llm_is_not_served() {
    let loopback = server_mock(None).await;
    let remote = server_mock(Some(failures_payload())).await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_git_project(project.path());
    write_failure_commit(project.path());
    let db = project.path().join("index.db");
    seed_index(home.path(), project.path(), &db);
    write_server_config(project.path(), &remote.uri());
    let state_dir = home.path().join("state");
    write_loopback_state(&state_dir, &loopback.uri());

    let mem = project.path().join("memory.db");
    let output = failures_harvest_cmd(home.path(), project.path(), &mem, &state_dir)
        .env("INKENTRY_LLM_URL", "http://127.0.0.1:1234")
        .output()
        .expect("run harvest");
    let text = combined(&output);

    assert!(!output.status.success(), "{text}");
    assert!(
        text.contains("inkentry server stop") && text.contains("inkentry server start"),
        "the restart is the only useful instruction here:\n{text}"
    );
    // The privacy guard rendered as prose: never nudge a user who asked for a
    // local LLM toward the remote this run deliberately avoided.
    assert!(
        !text.contains("server_url"),
        "the message must not offer the remote as a way out:\n{text}"
    );
    assert_eq!(
        count_path(&remote, "/llm/complete").await,
        0,
        "commit content must never reach a remote LLM the user did not choose:\n{text}"
    );
    assert_no_internal_names(&text);
}

// ── memory harvest --source claude-code ───────────────────────────────────
//
// The Claude Code harvester builds its own clients, so the git source passing
// says nothing about it.

fn write_claude_history(path: &Path, project_root: &Path, session: &str) {
    // `project` is matched against the git workdir the command discovers, which
    // is canonical; on macOS the temp dir is a symlink, so an uncanonicalised
    // path here would silently filter every session out.
    let root = std::fs::canonicalize(project_root).expect("canonicalize project root");
    let entry = serde_json::json!({
        "display": "we chose sqlite over postgres for the local index",
        "pastedContents": {},
        "timestamp": 1_800_000_000_000i64,
        "project": root.to_string_lossy(),
        "sessionId": session,
    });
    std::fs::write(path, format!("{entry}\n")).expect("write history.jsonl");
}

fn claude_payload(session: &str) -> String {
    serde_json::json!({
        "entries": [{
            "session_id": session,
            "kind": "decision",
            "title": "Use sqlite for the local index",
            "body": "Chosen over postgres to keep the local setup dependency free.",
            "tags": ["storage"],
        }]
    })
    .to_string()
}

fn claude_harvest_cmd(
    home: &Path,
    project: &Path,
    mem: &Path,
    state_dir: &Path,
    history: &Path,
) -> assert_cmd::Command {
    let mut cmd = base_cmd(home, project);
    cmd.env("INKENTRY_STATE_DIR", state_dir)
        .arg("memory")
        .arg("harvest")
        .arg("--db")
        .arg(mem)
        .arg("--source")
        .arg("claude-code")
        .arg("--history-file")
        .arg(history)
        .arg("--confirm");
    cmd
}

#[tokio::test]
async fn claude_code_harvest_splits_extraction_to_the_remote_and_embedding_to_the_loopback() {
    const SESSION: &str = "session-abc";
    let loopback = server_mock(None).await;
    let remote = server_mock(Some(claude_payload(SESSION))).await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_git_project(project.path());
    let db = project.path().join("index.db");
    seed_index(home.path(), project.path(), &db);
    write_server_config(project.path(), &remote.uri());
    let state_dir = home.path().join("state");
    write_loopback_state(&state_dir, &loopback.uri());
    let history = home.path().join("history.jsonl");
    write_claude_history(&history, project.path(), SESSION);

    let mem = project.path().join("memory.db");
    let output = claude_harvest_cmd(home.path(), project.path(), &mem, &state_dir, &history)
        .output()
        .expect("run harvest");
    let text = combined(&output);

    assert!(
        count_path(&remote, "/llm/complete").await > 0,
        "the remote is the only LLM available and must serve extraction:\n{text}"
    );
    assert!(
        count_path(&loopback, "/index/embed").await > 0,
        "dedup vectors must still be embedded locally:\n{text}"
    );
    assert_eq!(
        count_path(&remote, "/index/embed").await,
        0,
        "routing extraction to the remote must not divert embedding there:\n{text}"
    );
}

#[tokio::test]
async fn claude_code_harvest_stops_with_the_restart_message_when_the_local_llm_is_not_served() {
    const SESSION: &str = "session-def";
    let loopback = server_mock(None).await;
    let remote = server_mock(Some(claude_payload(SESSION))).await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_git_project(project.path());
    let db = project.path().join("index.db");
    seed_index(home.path(), project.path(), &db);
    write_server_config(project.path(), &remote.uri());
    let state_dir = home.path().join("state");
    write_loopback_state(&state_dir, &loopback.uri());
    let history = home.path().join("history.jsonl");
    write_claude_history(&history, project.path(), SESSION);

    let mem = project.path().join("memory.db");
    let output = claude_harvest_cmd(home.path(), project.path(), &mem, &state_dir, &history)
        .env("INKENTRY_LLM_URL", "http://127.0.0.1:1234")
        .output()
        .expect("run harvest");
    let text = combined(&output);

    assert!(!output.status.success(), "{text}");
    assert!(
        text.contains("inkentry server stop") && text.contains("inkentry server start"),
        "the restart is the only useful instruction here:\n{text}"
    );
    // The privacy guard rendered as prose: never nudge a user who asked for a
    // local LLM toward the remote this run deliberately avoided.
    assert!(
        !text.contains("server_url"),
        "the message must not offer the remote as a way out:\n{text}"
    );
    assert_eq!(
        count_path(&remote, "/llm/complete").await,
        0,
        "session content must never reach a remote LLM the user did not choose:\n{text}"
    );
    assert_no_internal_names(&text);
}
