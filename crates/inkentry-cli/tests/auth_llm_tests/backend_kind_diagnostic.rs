use crate::plumbing_helpers;
use plumbing_helpers::inkentry_bin;

use predicates::prelude::*;
use std::fs;
use tempfile::tempdir;

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
    fs::write(
        &config_path,
        format!("db_path = {:?}\n", db_path.display().to_string()),
    )
    .unwrap();

    // INKENTRY_NO_SERVER=1: loopback auto-discovery could route the embed call
    // to a running inkentry-server and fail the build with a dimension mismatch.
    inkentry_bin()
        .env("INKENTRY_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .arg("index")
        .arg(&project_dir)
        .assert()
        .success();

    (temp, project_dir, config_path)
}

#[test]
fn status_json_includes_memory_backend_field() {
    let (_temp, project_dir, config_path) = setup_offline_project();

    let output = inkentry_bin()
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
        "inkentry status --format json exited non-zero: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let body: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("output must be valid JSON");

    assert!(
        body.get("memory_backend").is_some(),
        "expected a `memory_backend` key in status JSON, got: {}",
        serde_json::to_string_pretty(&body).unwrap_or_default()
    );

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

#[test]
fn status_text_mentions_memory_backend() {
    let (_temp, project_dir, config_path) = setup_offline_project();

    inkentry_bin()
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
