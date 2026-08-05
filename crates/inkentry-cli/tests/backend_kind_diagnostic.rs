//! Integration tests for issue #308: `memory_backend` field in
//! `spelunk status --format json` and `spelunk check --format json`.

mod plumbing_helpers;
use plumbing_helpers::spelunk_bin;

use predicates::prelude::*;
use std::fs;
use tempfile::tempdir;

// ── helpers ──────────────────────────────────────────────────────────────────

/// Spin up a minimal indexed project in a temp directory and return
/// `(TempDir, project_dir, config_path)`.  The index is built with no server
/// URL and no explicit backend override, so the resolved backend is the
/// default local SQLite store.
fn setup_offline_project() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let temp = tempdir().unwrap();
    let project_dir = temp.path().join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(
        project_dir.join("lib.rs"),
        "pub fn hello() -> &'static str { \"hello\" }",
    )
    .unwrap();

    let db_path = temp.path().join("index.db");
    let config_path = temp.path().join("config.toml");
    // No server_url → offline, default backend is sqlite.
    fs::write(
        &config_path,
        format!("db_path = {:?}\n", db_path.display().to_string()),
    )
    .unwrap();

    // Build index. `INKENTRY_NO_SERVER=1` forces offline so the index skips the
    // embed phase entirely (no embedding server needed); without it, loopback
    // auto-discovery can pick up a `inkentry-server` running on 127.0.0.1:7777
    // and route the embed call there, which fails the build with a dimension
    // mismatch. We only care about the SQLite memory-backend path here.
    spelunk_bin()
        .env("INKENTRY_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    (temp, project_dir, config_path)
}

// ── `spelunk status --format json` ───────────────────────────────────────────

/// `spelunk status --format json` must include a top-level `memory_backend`
/// field whose value is one of the known backend identifiers (issue #308).
#[test]
fn status_json_includes_memory_backend_field() {
    let (_temp, project_dir, config_path) = setup_offline_project();

    let output = spelunk_bin()
        .current_dir(&project_dir)
        .env_remove("INKENTRY_SERVER_URL")
        .env("INKENTRY_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "spelunk status --format json exited non-zero: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let body: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("output must be valid JSON");

    // Field must be present.
    assert!(
        body.get("memory_backend").is_some(),
        "expected a `memory_backend` key in status JSON, got: {}",
        serde_json::to_string_pretty(&body).unwrap_or_default()
    );

    // Value must be a non-empty string.
    let kind = body["memory_backend"]
        .as_str()
        .expect("`memory_backend` must be a string");
    assert!(!kind.is_empty(), "`memory_backend` must not be empty");

    // Value must be one of the known backend identifiers.
    const KNOWN: &[&str] = &["sqlite", "git-meta", "git-notes", "remote"];
    assert!(
        KNOWN.contains(&kind),
        "`memory_backend` must be one of {KNOWN:?}, got: {kind:?}"
    );
}

// ── `spelunk check --format json` ────────────────────────────────────────────

/// `spelunk check --format json` must include a top-level `memory_backend`
/// field with the same semantics as in `spelunk status` (issue #308).
#[test]
fn check_json_includes_memory_backend_field() {
    let (_temp, project_dir, config_path) = setup_offline_project();

    let output = spelunk_bin()
        .current_dir(&project_dir)
        .env_remove("INKENTRY_SERVER_URL")
        .env("INKENTRY_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .arg("check")
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();

    // `spelunk check` exits 1 when stale files exist; that is fine here because
    // we only care about the JSON shape on stdout.
    let body: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("output must be valid JSON");

    // Field must be present.
    assert!(
        body.get("memory_backend").is_some(),
        "expected a `memory_backend` key in check JSON, got: {}",
        serde_json::to_string_pretty(&body).unwrap_or_default()
    );

    // Value must be a non-empty string in the known set.
    let kind = body["memory_backend"]
        .as_str()
        .expect("`memory_backend` must be a string");
    assert!(!kind.is_empty(), "`memory_backend` must not be empty");

    const KNOWN: &[&str] = &["sqlite", "git-meta", "git-notes", "remote"];
    assert!(
        KNOWN.contains(&kind),
        "`memory_backend` must be one of {KNOWN:?}, got: {kind:?}"
    );
}

// ── `spelunk status` text output ─────────────────────────────────────────────

/// `spelunk status` text output (no --format json) must mention the active
/// memory backend so humans can see which store is in use (issue #308). Since
/// ADR-067 D3 the line reflects the resolved backend (sqlite by default), sourced
/// from `backend_kind()` rather than the capability tier.
#[test]
fn status_text_mentions_memory_backend() {
    let (_temp, project_dir, config_path) = setup_offline_project();

    spelunk_bin()
        .current_dir(&project_dir)
        .env_remove("INKENTRY_SERVER_URL")
        .env("INKENTRY_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("sqlite (local)"));
}
