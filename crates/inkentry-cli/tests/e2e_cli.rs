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
            // On Windows clap includes the `.exe` extension: "inkentry.exe [OPTIONS]…"
            // Match only the stable prefix so the assertion holds on all platforms.
            "Usage: inkentry",
        ))
        .stdout(predicate::str::contains("Commands:"));
}

/// Guard the help-text corrections from PR fix(cli): correct stale and inaccurate --help text.
///
/// Checks that:
/// - `memory add --kind` lists `antipattern` (was missing before the fix)
/// - `memory harvest --source` lists `failures` (was missing before the fix)
/// - `memory harvest --help` does not contain an `ADR-` internal reference (removed)
/// - `sync --help` says "shorthand" not "alias" (was inaccurate before the fix)
///
/// These assertions are deliberately non-brittle: they check for the *presence* of
/// a corrected token or the *absence* of a stale one, not for exact prose alignment,
/// so ordinary copy edits won't break them.
#[test]
fn test_help_text_accuracy_guards() {
    // `memory add --help` must list `antipattern` as a valid kind.
    inkentry_bin()
        .args(["memory", "add", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("antipattern"));

    // `memory harvest --help` must list `failures` as a valid --source value.
    inkentry_bin()
        .args(["memory", "harvest", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("failures"))
        // Must not embed internal ADR references in user-facing help.
        .stdout(predicate::str::contains("ADR-").not());

    // Top-level `sync --help` must say "shorthand", not "alias"
    // (sync dispatches directly, it is not a clap alias).
    inkentry_bin()
        .args(["sync", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("shorthand"))
        .stdout(predicate::str::contains("alias").not());
}

// The harvest promotion surface: `harvest` is a first-class top-level command
// listed in `--help` with full flag parity, while the old `memory harvest`
// spelling is hidden from `memory --help` yet still fully documented and
// runnable via its own `--help` (the still-working deprecated alias).
#[test]
fn harvest_is_a_top_level_command_with_a_hidden_working_alias() {
    // `inkentry --help` lists the top-level `harvest` command. "backfill"
    // appears only in that command's about, so it is a faithful proxy for the
    // command being listed.
    inkentry_bin()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("backfill"));

    // `inkentry harvest --help` documents every source value and the store
    // overrides, with no internal references leaking into user-facing help.
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

    // The deprecated alias is hidden from `inkentry memory --help` …
    inkentry_bin()
        .args(["memory", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("harvest").not());

    // … but still fully documented and runnable via `memory harvest --help`,
    // still listing `failures` and still free of internal references.
    inkentry_bin()
        .args(["memory", "harvest", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("failures"))
        .stdout(predicate::str::contains("ADR-").not());
}

// The `explore` command was removed outright (ADR-079). It must no longer
// appear in `inkentry --help`, and invoking it must fall through to clap's
// unknown-subcommand error with a non-zero exit — no LLM plumbing, no server
// probe, just the standard "unrecognized subcommand" failure.
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

// The `check` command was removed outright. Its three jobs are served
// elsewhere: index freshness by running the idempotent `index` directly (or, for
// a non-mutating gate, `plumbing ls-files --stale`), server health by `server
// status`, and active intents/overlap by `context`. Invoking `check` must fall
// through to clap's unknown-subcommand error, and it must not appear in help.
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

// The old porcelain machine surface (`check --format porcelain`, `--files`) is
// gone with the command, not merely hidden: it too yields the unknown-subcommand
// error rather than parsing.
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
        // ADR-067: an un-init'd dir fails closed and reports no project rather
        // than describing the global store.
        .stdout(predicate::str::contains("No inkentry project here"));
}

use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn test_index_and_status() {
    let mock_server = MockServer::start().await;
    let project_id = FIXTURE_PROJECT_ID;

    // Health probe — Tier 1 capability set.
    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "capabilities": ["memory", "index.embed", "search.semantic", "plan"],
        })))
        .mount(&mock_server)
        .await;

    // Embedding endpoint — handles the index phase.
    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/projects/.+/index/embed$"))
        .respond_with(IndexEmbedResponder)
        .mount(&mock_server)
        .await;

    // Search endpoint (#322) — returns a fake query vector for CLI-side KNN.
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
    // `server_url`/`project_id` only take effect from project-level
    // `.inkentry/config.toml` (or env), never from the `--config` global file.
    write_project_server_config(&project_dir, &mock_server.uri(), project_id);

    // Under the default `local_first` mode a bare `server_url` never routes
    // embedding/search to it (that's a loopback-only inference path); this
    // test exists to exercise the mock server's `/index/embed` and `/search`
    // endpoints, so it opts into `cloud_first` explicitly on every command,
    // the same way a real user would to keep this behavior.
    const CLOUD_FIRST: (&str, &str) = ("INKENTRY_MODE", "cloud_first");

    // 1. Index the project
    let mut cmd = inkentry_bin();
    cmd.current_dir(&project_dir)
        .env(CLOUD_FIRST.0, CLOUD_FIRST.1)
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    // 2. Check status
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

    // 3. Search for the function (semantic search via server embedding)
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

/// Regression test for #349 / qa-v080-test-plan.md §Fix 1 (decision #106).
///
/// `derive_project_id` produces slugs containing `/`:
///   - `local/<blake3-hex>`        — repo with no git remote
///   - `github.com/owner/repo`     — repo with a GitHub remote
///
/// Inserted raw into `/v1/projects/{project_id}/index/embed`, the slashes
/// split the path into extra segments and axum's router 404s. PR #349 added
/// `encode_project_id` to percent-encode the whole slug as a single path
/// segment (`/` → `%2F`) before building the URL. This test locks that fix in
/// for both shapes of project_id by asserting on the *raw* request path the
/// mock server actually received — not just that the CLI exits 0 — so a
/// future change that silently reverts to naive `format!` interpolation would
/// fail here even though the mock still matches via `path_regex`.
#[tokio::test]
async fn test_index_encodes_project_id_with_slashes_as_single_segment() {
    for project_id in [
        // No-remote repo: derive_local_fallback() shape.
        "local/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcd",
        // Remote repo: normalise_git_url() shape.
        "github.com/owner/repo",
    ] {
        let mock_server = MockServer::start().await;

        // Health probe — Tier 1 capability set.
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ok",
                "capabilities": ["memory", "index.embed", "search.semantic", "plan"],
            })))
            .mount(&mock_server)
            .await;

        // Embedding endpoint — match on ANY `/v1/projects/.../index/embed`
        // shape (including one that's been split into extra segments by an
        // unencoded slash) so a regression produces a clear path-shape
        // assertion failure below rather than an opaque 404 from the CLI.
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

        // `server_url` only loads from project-level `.inkentry/config.toml` (or
        // env), never the personal global config: see `Config::load_with_store`.
        write_project_server_config(&project_dir, &mock_server.uri(), project_id);

        // Index the project — must reach the embedding phase without a 404.
        //
        // This test's purpose is the project_id slash-encoding in the embed
        // request path, not local-vs-remote embed routing, so it needs an
        // explicit `server_url` to legitimately serve embedding. Under the
        // default `local_first` mode that routing is now correctly refused
        // (see the `get_inference_tier` routing fix), so force `cloud_first`
        // here: `.inkentry/config.toml` doesn't recognize a `mode` key (see
        // `write_project_server_config`), so this must go through the env
        // var.
        inkentry_bin()
            .current_dir(&project_dir)
            .env("INKENTRY_MODE", "cloud_first")
            .arg("--config")
            .arg(&config_path)
            .arg("index")
            .arg(&project_dir)
            .assert()
            .success();

        // Inspect the *raw* request the mock server received: the project_id
        // must occupy exactly one path segment, percent-encoded, with no bare
        // `/` from the slug splitting it into extra segments.
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

            // `v1`, `projects`, `<encoded project_id>`, `index`, `embed` — five
            // segments. If the slug's `/` were left raw, `local/<hex>` would
            // add one extra segment (six total) and `github.com/owner/repo`
            // would add two (seven total).
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

            // Round-trip: percent-decoding the segment must recover the
            // original slug exactly (this is what axum does server-side, and
            // what `projects.slug` persistence relies on — decision #106).
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

// ── Capability tier E2E tests ────────────────────────────────────────────────

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
        // ADR-067 D3: the memory line reflects the resolved backend (sqlite by
        // default), not a tier-derived git-notes label.
        .stdout(predicate::str::contains("sqlite (local)"))
        .stdout(predicate::str::contains("set server_url to enable"));
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
        // ADR-067 D3: memory line reflects the resolved backend. With an explicit
        // team server_url the mode is local_first, so the store is local sqlite
        // (converged by `inkentry sync`), not a tier-inferred "server sync" label.
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
    // `plan` is a reserved protocol field (ADR-002) with no `inkentry plan`
    // command yet: even though this mock server advertises "plan", it must
    // never surface in user-facing status JSON.
    assert!(body["capabilities"]["plan"].is_null());
    // `explore` was removed (ADR-079); the capability field is gone, so it never
    // appears in status JSON.
    assert!(body["capabilities"]["explore"].is_null());
    // With an explicit server_url and no `mode` override, the default is
    // local_first even though the tier probe found the server
    // reachable: tier and sync mode are independent axes.
    assert_eq!(body["mode"], "local_first", "got: {body}");
}

/// Validate the *stable* JSON schema introduced by issue #269.
///
/// Asserted top-level keys must be present in every future release; their
/// types must remain stable (additive changes only).
#[tokio::test]
async fn test_status_json_stable_schema() {
    // Offline mode — no server URL configured; embed locally.
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

    // Index the project so there is data to query.
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

    // ── Stable schema assertions (issue #269) ────────────────────────────────
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
    // languages must be an array; Rust file should appear.
    assert!(body["languages"].is_array(), "languages must be an array");
    let langs = body["languages"].as_array().unwrap();
    assert!(!langs.is_empty(), "languages must not be empty");
    // Each language entry must have name (string) and file_count (integer).
    for lang in langs {
        assert!(lang["name"].is_string(), "language name must be string");
        assert!(
            lang["file_count"].as_i64().is_some(),
            "language file_count must be integer"
        );
    }
    // embedding_dim: must be an integer or null (768 when embeddings are stored,
    // null when the local embedding server is not available in CI/test mode).
    assert!(
        body["embedding_dim"].as_u64().is_some() || body["embedding_dim"].is_null(),
        "embedding_dim must be a positive integer or null, got: {}",
        body["embedding_dim"]
    );
    // has_semantic_search: false in offline mode (no server_url).
    assert_eq!(
        body["has_semantic_search"].as_bool(),
        Some(false),
        "has_semantic_search must be false in offline mode"
    );
    // last_indexed_at: ISO-8601 string when files are indexed.
    assert!(
        body["last_indexed_at"].is_string(),
        "last_indexed_at must be a string after indexing"
    );
    let ts = body["last_indexed_at"].as_str().unwrap();
    assert!(
        ts.contains('T') && ts.ends_with('Z'),
        "last_indexed_at must be ISO-8601 UTC, got: {ts}"
    );
    // memory_entries: integer (0 is valid when no entries exist yet).
    assert!(
        body["memory_entries"].as_i64().is_some(),
        "memory_entries must be an integer"
    );
    // mode: additive field (no server_url configured -> resolve_mode() is
    // offline, the same default as pre-existing behaviour).
    assert_eq!(body["mode"], "offline", "got: {body}");
}

/// Locks the top-level key set of `status --format json` so a future change
/// cannot silently rename, drop, or add a field outside the documented
/// "additive extensions only" contract (issue #269 doc comment above
/// `status()`). `mode` (this story) is the newest addition.
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
        // ADR-037 P2 item 35: additive-only pending-count/last-synced fields.
        "sync_pending",
        "sync_last_synced_at",
        "server_url",
        "capabilities",
        "embedder_state",
        "embedding_count",
        "embedding_pending",
        // Freshness signal (distinct from coverage) + composition-scheme
        // provenance: additive-only status fields.
        "embedding_refresh_pending",
        "summary_scheme",
        // Tells an index this build emptied from one nobody ever indexed; the
        // two are otherwise the same zeros.
        "index_rebuilt_from",
        "embed_worker_alive",
        "embed_tokens",
        "drift_candidates",
        "usage_7d",
    ];
    want.sort_unstable();
    assert_eq!(
        got, want,
        "status --format json top-level key set changed; if this is an \
         intentional additive field, add it to `want` here and to the doc \
         comment on `status()`"
    );
}

// Investigation found no shared server/port/filesystem state this test could
// race on (INKENTRY_NO_SERVER short-circuits before any is touched); flakes
// under the parallel runner are attributed to generic child-process
// spawn/stdio contention on a loaded runner, not CLI logic. Named group so
// this doesn't serialize against unrelated tests.
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
        // Structural summaries are offline and always run, so there is no
        // "skipping summaries" notice any more. Offline, the actionable note is
        // that semantic search needs a local server; the index still succeeds
        // (chunks are stored for full-text search).
        .stderr(predicate::str::contains("inkentry server start"));
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

// ── Issue #284: search falls back to structural matching when no index / no embedder ───

/// When there is no .inkentry/index.db, `inkentry search` in auto mode must
// With no index, `inkentry search` requires one: it funnels to `inkentry init`
// rather than a silent empty result or the old index-free ast-grep scan.
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

    // No `.inkentry/` project here: search must fail closed and point at `init`.
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

// When the index exists but there is no embedder (no reachable server),
// `inkentry search` degrades to full-text search and succeeds, not a hard error.
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
    // Point at an unreachable endpoint so there's no embedder.
    fs::write(
        &config_path,
        format!(
            "db_path = {:?}\napi_base_url = \"http://127.0.0.1:19999\"\nllm_model = \"test\"\n",
            db_path
        ),
    )
    .unwrap();

    // Build the index (offline — no embedder needed for parse phase).
    // INKENTRY_NO_SERVER=1 keeps the embed phase from auto-discovering a
    // loopback inkentry-server on 127.0.0.1:7777.
    inkentry_bin()
        .env("INKENTRY_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    // Now search with no reachable embedder: the full-text degrade kicks in.
    // INKENTRY_NO_SERVER pins "no embedder" so the result does not depend on
    // whatever may be listening on the default loopback port.
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

    // Must not print the old opaque error message.
    assert.stdout(predicate::str::contains("Make sure the index has embeddings").not());
}

// ── inkentry server error-path tests ──────────────────────────────────────────

/// `inkentry server status` prints "not started" when no pid file exists.
#[test]
fn test_server_status_not_running() {
    let tmp = tempdir().unwrap();
    // Point HOME to an empty tmpdir so no real state files interfere.
    inkentry_bin_in(tmp.path())
        .arg("server")
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("not started"));
}

/// `inkentry server logs` exits with an error when no log file exists.
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

/// `inkentry server stop` exits with an error when there is no pid file, and
/// says how to find a server that is running without one rather than implying
/// none is.
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

/// `inkentry server start --bin <missing-path>` exits with a clear error.
///
/// We use `--bin` with a nonexistent path rather than `PATH=""` because in CI
/// both `inkentry` and `inkentry-server` are built to the same `target/debug/`
/// directory, so the sibling-binary lookup would find the real binary even with
/// an empty PATH.
#[test]
fn test_server_start_binary_not_found() {
    let tmp = tempdir().unwrap();
    // Use a path that does not exist on any platform. On Windows, an absolute
    // Unix-style path like /tmp/... is interpreted as a relative path and will
    // also not exist, so any clearly non-existent path works here.
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

/// `inkentry init` in non-TTY mode (piped stdin) prints the server skip notice
/// when no server is reachable. This covers the CI/hook path from issue #318.
#[test]
fn test_init_non_tty_prints_skip_notice() {
    let tmp = tempdir().unwrap();
    // Initialise a git repo so inkentry init finds a project root.
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

    // stdin is piped (not a TTY) when launched via assert_cmd, so
    // is_terminal() returns false — the non-interactive branch runs.
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

/// Init a git repo at `dir` with a committer identity so `inkentry init` finds a
/// project root. (spelunk-cloud/spelunk#141 init tests only need the repo, not any commits.)
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

/// `inkentry init` must NOT create an uninvited `CLAUDE.md` in the user's repo,
/// and must not claim to have written one.
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
        // The uninvited-write log line must be gone.
        .stdout(predicate::str::contains("CLAUDE.md written").not());

    assert!(
        !tmp.path().join("CLAUDE.md").exists(),
        "init must not create a CLAUDE.md in the project root"
    );
}

/// A pre-existing `CLAUDE.md` must be left byte-for-byte untouched — init must
/// never overwrite a user's own file.
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

// ── memory commands against an auto-discovered (loopback) server ─────────────
//
// ADR-004 (unified memory storage): `.inkentry/memory.db` is the single
// canonical store for every CLI memory read and write. An auto-discovered
// loopback server is an INFERENCE backend only (embeddings + LLM); it is never
// a memory store. So `memory add`, `memory search`, and `memory timeline` all
// resolve to the same local `memory.db`, and the server is consulted only to
// embed the query — never to fetch memory rows.
//
// Historical context: IMP-3 / spelunk-cloud/spelunk#316 / PR spelunk-cloud/spelunk#349 first taught these commands
// to honour an auto-discovered server (so they no longer errored "requires
// inkentry-server"), but routed BOTH inference and memory storage to the server
// via a synthesised `server_url`. That produced the split-brain Johan flagged
// on PR #386: a note added (to local `memory.db`) was invisible to
// `memory search` (which read the server's `server.db`). ADR-004 fixes this by
// routing inference via `inference_url` while leaving `server_url` unset for
// auto-discovered servers, so `open_memory_backend` keeps memory local.
//
// These tests reproduce the auto-discovery path end-to-end: NO `server_url` in
// config, `INKENTRY_NO_SERVER` unset, and a mock server reachable on loopback —
// discovered via `~/.local/state/inkentry/server.port` (the same file
// `inkentry server start` writes; see `capability/probe.rs` step 3a). We redirect
// `HOME` to an isolated temp dir and pre-write that port file so the probe
// finds our `wiremock` instance deterministically, without depending on the
// real default port 7777 (which may be occupied — or unoccupied — on the test
// host) and without touching the developer's real `~/.local/state`.
//
// Coverage note: `memory harvest` routes through the same `effective_config`
// bridging code, but harvesting requires mocking `git log` plus a streaming
// `/llm/complete` SSE extraction round-trip — disproportionately heavy relative
// to what's under test (the auto-discovery → inference-vs-storage split). Left
// uncovered here; flagged honestly rather than thrashing on heavyweight SSE
// mocks.

/// Write `<home>/.local/state/inkentry/server.port` so `capability::get_tier`'s
/// loopback auto-discovery (step 3a) finds our mock server deterministically.
/// Mirrors the file `inkentry server start` writes (see `cli/cmd/server.rs`).
///
/// Returns the state dir path so callers can pass it as `INKENTRY_STATE_DIR`
/// to child processes. `dirs::home_dir()` 6.x on Windows calls the Win32
/// `SHGetKnownFolderPath` API (a Registry lookup) instead of reading
/// `USERPROFILE`, so setting `HOME`/`USERPROFILE` in the child env is not
/// enough — `INKENTRY_STATE_DIR` bypasses that entirely.
fn write_server_port_file(home: &std::path::Path, port: u16) -> std::path::PathBuf {
    let state_dir = home.join(".local").join("state").join("inkentry");
    fs::create_dir_all(&state_dir).expect("create state dir");
    fs::write(state_dir.join("server.port"), format!("{port}\n")).expect("write server.port");
    state_dir
}

/// Extract the TCP port `wiremock` bound to from its `uri()` (`http://127.0.0.1:<port>`).
fn port_from_uri(uri: &str) -> u16 {
    uri.rsplit(':')
        .next()
        .expect("uri has a port")
        .trim_end_matches('/')
        .parse()
        .expect("uri port is numeric")
}

/// Mount the endpoints an INFERENCE-ONLY auto-discovered server needs:
/// - `GET /v1/health` — capability probe (reports `memory` + `search.semantic`
///   so `effective_config` and the inference client build successfully)
/// - `POST /v1/projects/{id}/index/embed` — query/note embedding (`embed_query`
///   / `try_embed_via_server`); returns a constant 768-dim vector so KNN over
///   the LOCAL store is deterministic.
///
/// Deliberately does NOT mount `POST /v1/projects/{id}/memory/search`. Under
/// ADR-004 an auto-discovered server is never a memory backend, so the CLI must
/// not call it for memory rows. The `expect(0)` guard below turns any such call
/// into a test failure, locking in the inference-vs-storage split.
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

    // Guard: the server's memory endpoint must NEVER be hit by an auto-discovered
    // server. If it is, the split-brain has regressed. `expect(0)` fails the test
    // on any matching request when the `MockServer` is dropped.
    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/projects/.+/memory/search$"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(server)
        .await;
}

/// ADR-004 round-trip: with a loopback server auto-discovered (no `server_url`
/// in config), a note written by `memory add` is found by `memory search` — and
/// the note's content comes from the LOCAL `memory.db`, not the server. The
/// server is consulted ONLY to embed (it has no `/memory/search` mount, and the
/// `expect(0)` guard fails the test if memory rows are ever requested from it).
///
/// This is the exact split-brain the ADR removes: before ADR-004 the
/// auto-discovered server synthesised a `server_url`, so `memory add` wrote
/// `memory.db` while `memory search` read the server's `server.db` and could not
/// see the note.
#[tokio::test]
async fn test_memory_add_then_search_round_trip_on_local_store_with_auto_discovered_server() {
    let mock_server = MockServer::start().await;
    mount_auto_discovery_inference_endpoints(&mock_server).await;

    let temp = tempdir().unwrap();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let state_dir = write_server_port_file(&home, port_from_uri(&mock_server.uri()));

    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(project_dir.join("main.rs"), "fn main() {}").unwrap();

    // No `server_url` (and no `project_id`) in config — the defining trait of
    // the auto-discovered path. `api_base_url` is unrelated to capability tier
    // probing; it only configures the (offline) embedding/LLM endpoints.
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

    // Build a local index so memory commands have a DB to resolve `mem_path`
    // from (offline embedding — INKENTRY_NO_SERVER keeps `index` from probing).
    inkentry_bin()
        .env("HOME", &home)
        .env("INKENTRY_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    // Add a note via the auto-discovery path. No INKENTRY_NO_SERVER, so the
    // loopback server embeds the note (via /index/embed) while the note text +
    // metadata are written to the LOCAL memory.db.
    inkentry_bin()
        .env("HOME", &home)
        .env("INKENTRY_STATE_DIR", &state_dir)
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

    // Search for it via the same auto-discovery path. The result must be the
    // locally-stored note — proving add and search share one store. The server
    // only embedded the query; the `/memory/search` guard ensures no memory rows
    // were fetched from the server.
    inkentry_bin()
        .env("HOME", &home)
        .env("INKENTRY_STATE_DIR", &state_dir)
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

    // Cross-check: `memory list` (which has always read memory.db) sees the same
    // note. Before ADR-004 `search` and `list` could disagree; now they cannot.
    inkentry_bin()
        .env("HOME", &home)
        .env("INKENTRY_STATE_DIR", &state_dir)
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

/// Founder's own manual repro (2026-07-23): `local_first`
/// (no explicit `mode`, reached because `server_url` is set), an explicit
/// `server_url` pointed at an address nothing mounts anything on, and a
/// loopback server auto-discovered via the port file. `memory add` must embed
/// via the loopback server (never the unroutable `server_url`), and `memory
/// search` must return the local semantic result rather than erroring: before
/// the fix, `resolve_inference_url()` returned the explicit `server_url`
/// unconditionally, and the query embed 404'd against it (`{server_url}` has
/// no `/index/embed` route in the cloud case this reproduces).
#[tokio::test]
async fn test_memory_add_then_search_round_trip_local_first_with_explicit_server_url() {
    let mock_server = MockServer::start().await;
    mount_auto_discovery_inference_endpoints(&mock_server).await;

    let temp = tempdir().unwrap();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let state_dir = write_server_port_file(&home, port_from_uri(&mock_server.uri()));

    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(project_dir.join("main.rs"), "fn main() {}").unwrap();

    // `server_url` set, no `mode` key: resolves to `local_first`. Deliberately
    // an address nothing mounts anything on (mirrors the founder's
    // `https://api.inkentry.com`): an accidental fallback to it for
    // inference would surface as a connection error, never a silent pass.
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

/// `memory timeline` against an auto-discovered loopback server returns notes
/// from the LOCAL `memory.db` (the server only embeds the query). Companion to
/// the add→search round-trip above; guards that `timeline` does not regress to
/// reading the server's store.
#[tokio::test]
async fn test_memory_timeline_reads_local_store_with_auto_discovered_server() {
    let mock_server = MockServer::start().await;
    mount_auto_discovery_inference_endpoints(&mock_server).await;

    let temp = tempdir().unwrap();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let state_dir = write_server_port_file(&home, port_from_uri(&mock_server.uri()));

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

// ── init imports git-notes memory into memory.db ─────────────────────────────
//
// During `inkentry init`, after the project memory.db is created, every entry on
// the enclosing repo's `refs/notes/inkentry` that is not already present is
// imported into memory.db (no embeddings). The summary line
// `Memory:  imported N entries from git notes` prints only when N > 0, and a
// re-run imports nothing (dedup by the same content key as `memory reconcile`).

/// Init a git repo at `dir` with a committer identity AND one commit, so
/// `refs/notes/inkentry` can be attached - git notes hang off a commit object,
/// so the no-commit `git_init_repo` helper above is not enough here.
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

/// One JSON-Lines `NoteRecord` as the git-notes backend serializes it. Built as
/// a `serde_json::Value` rather than the (crate-private) `NoteRecord` type so
/// this test needs no library dependency on inkentry-cli.
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

/// Attach `jsonl` (one or more record lines) to HEAD's `refs/notes/inkentry`.
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

/// End-to-end: `inkentry init` over a real repo that already has git-notes
/// memory imports those entries, `memory list` surfaces them, the summary line
/// reports the right count, and a second init is a no-op (no re-import, no
/// duplicate rows). Covers the import-on-init and idempotency guarantees.
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

    // First init: both pre-existing git-notes entries import, and the summary
    // line reports the exact count.
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

    // `memory list` (default sqlite backend, reads memory.db) surfaces both.
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

    // Second init: everything dedups, so nothing imports and the Memory summary
    // line is suppressed (printed only when N > 0).
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

    // The key idempotency guarantee: row count is stable — no duplicate rows.
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

/// `inkentry init` outside any git repo skips the git-notes import entirely:
/// there is no enclosing repo to read notes from, so no import runs, the Memory
/// summary line is absent, and init still succeeds.
#[test]
fn test_init_without_git_repo_skips_notes_import() {
    let tmp = tempdir().unwrap();
    // Deliberately NOT a git repo — no `.git`, no notes ref.
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

// ── ADR-070 D3/D4: warmup contract + status honesty (adversarial pass) ────────

/// Build an offline-indexed project (chunks stored, zero embeddings, no
/// recorded worker) under `home`, returning `(project_dir, config_path)`.
/// The index DB lands at `<project_dir>/.inkentry/index.db` - the same path
/// `status`/`search` resolve via the project walk, and the one the embed
/// worker's state files are keyed on.
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

/// Path of the embed worker's pid state file for `db_path` under a given
/// state directory, replicating the worker's own keying (blake3 of the
/// canonicalised index path, first 16 hex chars). Deliberately duplicated
/// here: if the writer's keying ever drifts from this, the reader/writer
/// pair drifts too, and this test fails loudly.
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

/// Same as [`embed_worker_pid_file_in`], for the default (no
/// `INKENTRY_STATE_DIR`) state dir derived from `home`.
#[cfg(unix)]
fn embed_worker_pid_file(home: &std::path::Path, db_path: &std::path::Path) -> std::path::PathBuf {
    embed_worker_pid_file_in(&home.join(".local").join("state").join("inkentry"), db_path)
}

/// ADR-070 D4: the `status --format json` embed-state extensions are additive
/// and truthful. On an offline-built index (pending work, no worker) the new
/// fields must report pending counts, a non-alive worker, and token sums with
/// their own denominators - while the stable #269 schema keys survive intact.
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

    // Stable schema keys must survive the extension (additive-only contract).
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

/// ADR-070 D4: with pending work and no recorded worker, text `status` says
/// `Embedding incomplete` plus the resume command - never `in progress`, and
/// the deleted hedging parenthetical must not resurface.
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

/// ADR-070 D4: a worker that crashed without cleanup leaves a pid file behind;
/// the next `status` must classify the dead pid as not-running (never
/// `in progress`) and remove the stale record so it cannot be re-read later.
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

/// ADR-070 D4: a pid recycled by an unrelated live process (here: this test
/// process itself - alive, but its command line is not a inkentry index run)
/// must never be reported as a live embed worker, and the foreign record is
/// cleaned up like a dead one.
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

/// Regression: writer and reader of runtime state must agree on
/// `INKENTRY_STATE_DIR`. `HOME` and `INKENTRY_STATE_DIR` are pointed at two
/// *different* directories; the embed worker's pid file is written only into
/// the override directory (as the writer does once it honours the override),
/// never under `HOME`. `status` - the reader - must resolve the same
/// override to find and clean it up. Before the fix, `status`'s read path
/// (`cli/cmd/embed_worker.rs` -> `cli/cmd/server.rs::inkentry_state_dir()`)
/// ignored `INKENTRY_STATE_DIR` and only ever looked under `HOME`, so a file
/// written to the override would never be found.
#[cfg(unix)]
#[test]
fn test_status_honors_state_dir_override_for_embed_worker_pid() {
    let home = tempfile::TempDir::new().unwrap();
    let state_override = tempfile::TempDir::new().unwrap();
    let (project_dir, config_path) = offline_indexed_project(home.path());
    let db_path = project_dir.join(".inkentry").join("index.db");
    assert!(db_path.exists(), "offline index must exist");

    // A pid that was real and is now certainly dead.
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let dead_pid = child.id();
    child.wait().unwrap();

    // Write directly into the override dir - NOT `<home>/.local/state/inkentry`.
    let pid_file = embed_worker_pid_file_in(state_override.path(), &db_path);
    fs::create_dir_all(pid_file.parent().unwrap()).unwrap();
    fs::write(&pid_file, format!("{dead_pid}\n")).unwrap();
    fs::write(pid_file.with_extension("baseline"), "0 1000\n").unwrap();

    // Sanity: nothing was written under the HOME-derived default location.
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

// Re-stamp an index with a schema version this build does not accept, so the
// next open discards and recreates it. The rebuild branches on the stamp alone,
// so a re-stamped index takes exactly the path a genuinely older one does,
// without pinning the test to a shape no released binary writes any more.
fn downstamp_index(project_dir: &std::path::Path, to: i32) -> std::path::PathBuf {
    let db_path = project_dir.join(".inkentry").join("index.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch(&format!("PRAGMA user_version = {to};"))
        .unwrap();
    drop(conn);
    db_path
}

// A rebuilt index and a never-indexed project print the same zeros, and until
// this landed nothing at the CLI's default log level told them apart: `search`
// said `No results found.` and exited 0, so a successful upgrade read as an
// empty repository. The rebuild has to state itself on the run that performs
// it, and the emptiness it leaves has to stay attributable on every run after.
#[test]
fn a_rebuilt_index_states_itself_and_stays_attributable_until_reindexed() {
    let home = tempfile::TempDir::new().unwrap();
    let (project_dir, config_path) = offline_indexed_project(home.path());
    downstamp_index(&project_dir, 15);

    // The run that rebuilds says so, without RUST_LOG.
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

    // A later run rebuilds nothing, so it must not claim to, but the absence is
    // still explained: this is the run a user actually meets after upgrading.
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

    // `status` computes the same fact and now states it.
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

    // The rebuild is a statement, not a gate: the tool still works, and the
    // reindex it asked for clears the fact rather than leaving it stuck on.
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

// `status --format json` carries the same fact for the tooling that reads it
// there, and a never-rebuilt index reports null rather than an absent key.
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

// Zero-coverage cell, end to end: an offline-built index has chunks but no
// embeddings; `search` degrades to full-text search (which covers every chunk
// from parse time) with a stderr warmup notice, never a bare `No results found.`
// over a corpus the vector half never saw and never the removed ast-grep scan.
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

// Partial-coverage cell, end to end: embed everything, then add a
// file and re-index offline so coverage is partial. An auto search must emit
// the one-line stderr warmup notice carrying the coverage AND its
// front-loaded shape, while `--format json` stdout stays machine-clean.
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

    // Pass 1: embed everything via the mock server (full coverage).
    //
    // This test's purpose is the partial-vs-zero coverage warmup notice, not
    // local-vs-remote embed routing, so it needs an explicit `server_url` to
    // legitimately serve embedding here. Under the default `local_first`
    // mode that routing is now correctly refused (see the `get_inference_tier`
    // routing fix) in favor of the local loopback embedder, which this test
    // does not configure - so force `cloud_first` via env, which outranks
    // both config files.
    inkentry_bin_in(home.path())
        .env("INKENTRY_MODE", "cloud_first")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    // Pass 2: add a file and re-index offline - its chunks are stored but not
    // embedded, so coverage drops below 100%.
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

    // Auto search with no reachable embedder: the partial-coverage warmup
    // notice must land on stderr (percentage + shape + pointer at status),
    // and the JSON on stdout must stay parseable.
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

// A key the project config is not read for is named on stderr rather
// than dropped in silence, and the rest of the file still loads.
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
