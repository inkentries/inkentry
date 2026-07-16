//! Read commands under a configured team `server_url` (ADR-037).
//!
//! With `server_url` set, the default `local_first` mode serves reads from the
//! local store. That is by design (offline-resilient; converge via `spelunk
//! sync`), but it must be labeled: a read that silently serves local data can
//! be mistaken for team state. These tests pin the label and the `cloud_first`
//! counterpart: reads route to the server and an unreachable server is a hard
//! error, never a silent local read.

mod plumbing_helpers;
use plumbing_helpers::spelunk_bin;

use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Stable prefix of the stderr label (`local_read_notice` in
/// `cli/cmd/helpers.rs`).
const NOTICE_SNIPPET: &str = "showing local data (mode \"local_first\")";

/// Title of the locally seeded entry; must never appear on stdout when reads
/// route to the server.
const LOCAL_TITLE: &str = "local only entry";

fn write_cfg(dir: &Path, name: &str, db_path: &Path, extra: &str) -> PathBuf {
    let cfg = format!(
        "db_path = {:?}\napi_base_url = \"http://127.0.0.1:1\"\n\
         embedding_model = \"test-model\"\nllm_model = \"test-chat\"\n{extra}",
        db_path
    );
    let path = dir.join(name);
    std::fs::write(&path, cfg).expect("write config");
    path
}

/// Seed one local memory entry (no `server_url`: solo/local write path) and
/// return `(tmp, mem_path, id)`.
fn seeded_project() -> (TempDir, PathBuf, i64) {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("spelunk.db");
    let mem_path = db_path.with_file_name("memory.db");
    let cfg = write_cfg(tmp.path(), "config-seed.toml", &db_path, "");
    let out = spelunk_bin()
        // Not a git repo: the git-notes write-through is a no-op, so the entry
        // lands only in the local memory.db.
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&cfg)
        .args(["memory", "--db"])
        .arg(&mem_path)
        .args([
            "add",
            "--kind",
            "note",
            "--title",
            LOCAL_TITLE,
            "--body",
            "b",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    // "Stored [note] #<id>: <title>"
    let id: i64 = stdout
        .split('#')
        .nth(1)
        .and_then(|s| s.split(':').next())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or_else(|| panic!("could not parse stored id from: {stdout}"));
    (tmp, mem_path, id)
}

fn memory_list(tmp: &TempDir, mem_path: &Path, cfg: &Path) -> std::process::Output {
    spelunk_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(cfg)
        .args(["memory", "--db"])
        .arg(mem_path)
        .args(["list", "--format", "json"])
        .output()
        .unwrap()
}

// ── local_first: data served, labeled on stderr, stdout machine-clean ─────────

#[test]
fn local_first_read_serves_data_with_stderr_notice_and_clean_stdout() {
    let (tmp, mem_path, _id) = seeded_project();
    // Non-loopback https passes the transport guard; local_first never contacts
    // it, so the host being unresolvable is irrelevant (and proves no probe).
    let cfg = write_cfg(
        tmp.path(),
        "config-local-first.toml",
        &tmp.path().join("spelunk.db"),
        "server_url = \"https://team.invalid:7777\"\nproject_id = \"team/proj\"\n",
    );

    let out = memory_list(&tmp, &mem_path, &cfg);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(out.status.success(), "expected exit 0; stderr: {stderr}");
    assert!(
        stderr.contains(NOTICE_SNIPPET),
        "stderr must label the local read: {stderr}"
    );
    assert!(
        stderr.contains("spelunk sync"),
        "notice must say how to converge: {stderr}"
    );
    // Local data is still served.
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout must be pure JSON");
    assert!(
        parsed.as_array().is_some_and(|a| !a.is_empty()),
        "expected the seeded entry on stdout: {stdout}"
    );
    assert!(stdout.contains(LOCAL_TITLE), "got: {stdout}");
    // The notice must not pollute machine-readable stdout.
    assert!(
        !stdout.contains(NOTICE_SNIPPET),
        "notice leaked to stdout: {stdout}"
    );
}

// ── no server_url: solo path stays byte-identical (no notice) ─────────────────

#[test]
fn no_server_url_read_has_no_notice() {
    let (tmp, mem_path, _id) = seeded_project();
    let cfg = write_cfg(
        tmp.path(),
        "config-solo.toml",
        &tmp.path().join("spelunk.db"),
        "",
    );

    let out = memory_list(&tmp, &mem_path, &cfg);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(out.status.success(), "stderr: {stderr}");
    assert!(
        !stderr.contains(NOTICE_SNIPPET),
        "solo path must not emit the team-server notice: {stderr}"
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains(LOCAL_TITLE));
}

// ── explicit offline: user opted out of server contact, no notice ─────────────

#[test]
fn explicit_offline_read_has_no_notice() {
    let (tmp, mem_path, _id) = seeded_project();
    let cfg = write_cfg(
        tmp.path(),
        "config-offline.toml",
        &tmp.path().join("spelunk.db"),
        "server_url = \"https://team.invalid:7777\"\nproject_id = \"team/proj\"\nmode = \"offline\"\n",
    );

    let out = memory_list(&tmp, &mem_path, &cfg);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(out.status.success(), "stderr: {stderr}");
    assert!(
        !stderr.contains(NOTICE_SNIPPET),
        "explicit offline is an opt-out, not an unlabeled fallback: {stderr}"
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains(LOCAL_TITLE));
}

// ── cloud_first: unreachable server = hard error, local data never printed ────

#[test]
fn cloud_first_read_unreachable_server_errors_without_local_data() {
    let (tmp, mem_path, _id) = seeded_project();
    // Loopback http passes the transport guard; nothing listens on port 1, so
    // the read must fail. A raw-UUID project_id skips slug resolution, proving
    // the failure is the memory read itself.
    let cfg = write_cfg(
        tmp.path(),
        "config-cloud-first.toml",
        &tmp.path().join("spelunk.db"),
        "server_url = \"http://127.0.0.1:1\"\n\
         project_id = \"11111111-1111-1111-1111-111111111111\"\n\
         mode = \"cloud_first\"\n",
    );

    let out = memory_list(&tmp, &mem_path, &cfg);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        !out.status.success(),
        "cloud_first read against an unreachable server must exit non-zero; \
         stdout: {stdout}"
    );
    // The one unacceptable outcome: silently substituting local data.
    assert!(
        !stdout.contains(LOCAL_TITLE),
        "local data must never be printed when reads route to the server: {stdout}"
    );
    assert!(
        !stderr.contains(NOTICE_SNIPPET),
        "cloud_first must not claim a local read: {stderr}"
    );
    // The error names the failed operation and carries the source chain
    // (anyhow context + reqwest cause).
    assert!(
        stderr.contains("GET /memory"),
        "error must name the failed server read: {stderr}"
    );
    assert!(
        stderr.contains("Caused by"),
        "error must carry the cause chain: {stderr}"
    );
}

// ── spelunk status: mode line + scope-aware offline hints ─────────────────────

/// Minimal indexed project so `spelunk status` passes the ADR-067 project
/// gate. Indexed with SPELUNK_NO_SERVER=1 (no embed phase, no probes).
fn indexed_project() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("lib.rs"), "pub fn hello() {}").unwrap();
    let db_path = tmp.path().join("index.db");
    let cfg = write_cfg(tmp.path(), "config-index.toml", &db_path, "");
    spelunk_bin()
        .env("SPELUNK_NO_SERVER", "1")
        .arg("--config")
        .arg(&cfg)
        .arg("index")
        .arg(&project)
        .assert()
        .success();
    (tmp, project)
}

#[test]
fn status_shows_mode_line_and_truthful_hints_with_unreachable_server_url() {
    let (tmp, project) = indexed_project();
    // Loopback https passes the transport guard; nothing listens on port 1, so
    // the tier probe fails fast and the tier is Offline with server_url SET.
    let cfg = write_cfg(
        tmp.path(),
        "config-team.toml",
        &tmp.path().join("index.db"),
        "server_url = \"https://127.0.0.1:1\"\nproject_id = \"team/proj\"\n",
    );

    let out = spelunk_bin()
        .current_dir(&project)
        .arg("--config")
        .arg(&cfg)
        .arg("status")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout: {stdout}");

    // The mode line distinguishes "local by design" from "local because broken".
    assert!(stdout.contains("mode"), "got: {stdout}");
    assert!(stdout.contains("local_first"), "got: {stdout}");
    // Explore's hint must not tell the operator to set an already-set server_url.
    assert!(
        stdout.contains("configured server unreachable"),
        "got: {stdout}"
    );
    assert!(
        !stdout.contains("set server_url to enable]"),
        "explore hint must not suggest setting an already-set server_url: {stdout}"
    );
}

#[test]
fn status_has_no_mode_line_on_solo_default() {
    let (tmp, project) = indexed_project();
    let cfg = write_cfg(
        tmp.path(),
        "config-solo-status.toml",
        &tmp.path().join("index.db"),
        "",
    );

    let out = spelunk_bin()
        .env("SPELUNK_NO_SERVER", "1") // hermetic: no loopback auto-discovery
        .current_dir(&project)
        .arg("--config")
        .arg(&cfg)
        .arg("status")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout: {stdout}");

    // Solo default: no sync configuration, no mode line.
    assert!(!stdout.contains("\n  mode"), "got: {stdout}");
    assert!(!stdout.contains("local_first"), "got: {stdout}");
    // And the set-server_url hints ARE correct here.
    assert!(stdout.contains("set server_url to enable"), "got: {stdout}");
}

// ── every one of the five read commands routes through the labeled seam ──────
//
// `open_read_backend` (helpers.rs) is a single seam, but each command wires it
// in independently (see the `open_read_backend` vs `open_memory_backend`
// imports in list.rs/show.rs/search.rs/timeline.rs/context.rs). A copy-paste
// regression in any one of them (e.g. reverting to `open_memory_backend`)
// would silently drop the notice for that command only; these drive the real
// binary for the three not already covered by `memory_list` above.

#[test]
fn show_command_labels_local_read_and_keeps_json_stdout_clean() {
    let (tmp, mem_path, id) = seeded_project();
    let cfg = write_cfg(
        tmp.path(),
        "config-show-local-first.toml",
        &tmp.path().join("spelunk.db"),
        "server_url = \"https://team.invalid:7777\"\nproject_id = \"team/proj\"\n",
    );

    let out = spelunk_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&cfg)
        .args(["memory", "--db"])
        .arg(&mem_path)
        .args(["show", &id.to_string(), "--format", "json"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(out.status.success(), "stderr: {stderr}");
    assert!(
        stderr.contains(NOTICE_SNIPPET),
        "memory show must label the local read: {stderr}"
    );
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout must be pure JSON");
    assert_eq!(parsed["title"], LOCAL_TITLE, "got: {stdout}");
    assert!(
        !stdout.contains(NOTICE_SNIPPET),
        "notice leaked to stdout: {stdout}"
    );
}

#[test]
fn show_command_no_server_url_has_no_notice() {
    let (tmp, mem_path, id) = seeded_project();
    let cfg = write_cfg(
        tmp.path(),
        "config-show-solo.toml",
        &tmp.path().join("spelunk.db"),
        "",
    );

    let out = spelunk_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&cfg)
        .args(["memory", "--db"])
        .arg(&mem_path)
        .args(["show", &id.to_string()])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr: {stderr}");
    assert!(!stderr.contains(NOTICE_SNIPPET), "got: {stderr}");
}

#[test]
fn search_command_text_mode_labels_local_read_and_keeps_json_stdout_clean() {
    let (tmp, mem_path, _id) = seeded_project();
    let cfg = write_cfg(
        tmp.path(),
        "config-search-local-first.toml",
        &tmp.path().join("spelunk.db"),
        "server_url = \"https://team.invalid:7777\"\nproject_id = \"team/proj\"\n",
    );

    // `--mode text` never needs an inference server, isolating the notice from
    // the (separately-tested) capability-tier probe / embedding path.
    let out = spelunk_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&cfg)
        .args(["memory", "--db"])
        .arg(&mem_path)
        .args(["search", "local", "--mode", "text", "--format", "json"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(out.status.success(), "stderr: {stderr}");
    assert!(
        stderr.contains(NOTICE_SNIPPET),
        "memory search must label the local read: {stderr}"
    );
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout must be pure JSON");
    assert!(
        parsed.as_array().is_some_and(|a| !a.is_empty()),
        "expected the seeded entry: {stdout}"
    );
    assert!(
        !stdout.contains(NOTICE_SNIPPET),
        "notice leaked to stdout: {stdout}"
    );
}

#[test]
fn context_command_labels_local_read_and_keeps_json_stdout_clean() {
    let (tmp, mem_path, _id) = seeded_project();
    let cfg = write_cfg(
        tmp.path(),
        "config-context-local-first.toml",
        &tmp.path().join("spelunk.db"),
        "server_url = \"https://team.invalid:7777\"\nproject_id = \"team/proj\"\n",
    );

    let out = spelunk_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&cfg)
        .args(["context", "--db"])
        .arg(&mem_path)
        .args(["--format", "json"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(out.status.success(), "stderr: {stderr}");
    assert!(
        stderr.contains(NOTICE_SNIPPET),
        "context must label the local read: {stderr}"
    );
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout must be pure JSON");
    assert!(parsed.get("sections").is_some(), "got: {stdout}");
    assert!(
        !stdout.contains(NOTICE_SNIPPET),
        "notice leaked to stdout: {stdout}"
    );
}

#[test]
fn context_command_no_server_url_has_no_notice() {
    let (tmp, mem_path, _id) = seeded_project();
    let cfg = write_cfg(
        tmp.path(),
        "config-context-solo.toml",
        &tmp.path().join("spelunk.db"),
        "",
    );

    let out = spelunk_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&cfg)
        .args(["context", "--db"])
        .arg(&mem_path)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr: {stderr}");
    assert!(!stderr.contains(NOTICE_SNIPPET), "got: {stderr}");
}

// ── local_first labels even when the team server IS reachable ────────────────
//
// The prior local_first tests use an unresolvable host, so the capability
// tier happens to be Offline too, never proving the notice fires when the
// tier probe succeeds. `memory search` builds an effective config from the
// probed tier (`capability::get_tier` + `Tier::effective_config`) before
// calling `open_read_backend`; this exercises that interaction end to end and
// pins the ADR-037 contract: sync mode and capability tier are independent
// axes, so a reachable team server does not change local_first's local read.

#[tokio::test]
async fn local_first_labels_read_even_when_team_server_is_reachable() {
    let (tmp, mem_path, _id) = seeded_project();

    let mock_server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/v1/health"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ok",
                "version": "0.9.0",
                "capabilities": ["memory", "index.embed", "search.semantic"],
                "instance_id": "00000000-0000-0000-0000-000000000002",
                "started_by": null,
                "embedding_dim": spelunk_core::embeddings::EMBEDDING_DIM,
            })),
        )
        .mount(&mock_server)
        .await;

    let cfg = write_cfg(
        tmp.path(),
        "config-search-local-first-reachable.toml",
        &tmp.path().join("spelunk.db"),
        &format!(
            "server_url = {:?}\nproject_id = \"team/proj\"\n",
            mock_server.uri()
        ),
    );

    let out = spelunk_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&cfg)
        .args(["memory", "--db"])
        .arg(&mem_path)
        .args(["search", "local", "--mode", "text", "--format", "json"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(out.status.success(), "stderr: {stderr}");
    assert!(
        stderr.contains(NOTICE_SNIPPET),
        "a reachable team server must not silence the local_first notice: {stderr}"
    );
    assert!(stdout.contains(LOCAL_TITLE), "got: {stdout}");
}
