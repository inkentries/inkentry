//! Integration tests for `inkentry metrics snapshot` (ADR-098) and the
//! additive `metrics` field it adds to `inkentry status --format json` and
//! the text summary in `inkentry status`.

mod plumbing_helpers;
use plumbing_helpers::inkentry_bin_in;

use std::fs;
use tempfile::TempDir;

/// An indexed, non-git project under `home`, returning `(project_dir,
/// config_path)`. Deliberately not a git repository: commit-based metrics
/// must be absent from this fixture's output.
fn indexed_project(home: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let project_dir = home.join("project");
    fs::create_dir(&project_dir).unwrap();
    fs::write(
        project_dir.join("lib.rs"),
        "pub fn compute(x: i32) -> i32 { x * 2 }\n",
    )
    .unwrap();
    let config_path = home.join("config.toml");
    fs::write(&config_path, "").unwrap();
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

#[test]
fn metrics_snapshot_json_is_valid_and_omits_commit_metrics_outside_a_git_repo() {
    let home = TempDir::new().unwrap();
    let (project_dir, config_path) = indexed_project(home.path());

    let output = inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .args(["metrics", "snapshot", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let body: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");

    assert_eq!(body["schema"], "inkentry.metrics/1");
    assert!(
        body["header"]["commit"].is_null(),
        "no git repo => no commit header"
    );
    assert!(
        body["state"].get("rec.commit_coverage").is_none(),
        "commit_coverage must be absent, not present-and-zero: {body}"
    );
    assert!(
        body["state"].get("cmp.lines_per_decision").is_none(),
        "lines_per_decision must be absent outside a git repo: {body}"
    );
    assert!(body.get("events").is_none(), "no events block: {body}");
    assert!(body.get("eval").is_none(), "no eval block, ever: {body}");
    assert!(body["state"].get("events").is_none());
    assert!(
        body["state"]["rec.entries"]["total"].is_object(),
        "rec.entries must be present: {body}"
    );
}

#[test]
fn metrics_snapshot_two_runs_are_byte_identical() {
    let home = TempDir::new().unwrap();
    let (project_dir, config_path) = indexed_project(home.path());

    let run = || {
        inkentry_bin_in(home.path())
            .env("INKENTRY_NO_SERVER", "1")
            .current_dir(&project_dir)
            .arg("--config")
            .arg(&config_path)
            .args(["metrics", "snapshot", "--json"])
            .output()
            .unwrap()
            .stdout
    };
    let a = run();
    let b = run();
    assert_eq!(
        a, b,
        "the same repository state must produce byte-identical output"
    );
}

#[test]
fn metrics_snapshot_text_summary_is_human_readable() {
    let home = TempDir::new().unwrap();
    let (project_dir, config_path) = indexed_project(home.path());

    inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .args(["metrics", "snapshot"])
        .assert()
        .success()
        .stdout(predicates::str::contains("rec.entries"))
        .stdout(predicates::str::contains("rec.commit_coverage"))
        .stdout(predicates::str::contains("cmp.tokens_context_estimate"));
}

#[test]
fn metrics_snapshot_fails_closed_without_an_inkentry_project() {
    let home = TempDir::new().unwrap();
    let bare_dir = home.path().join("bare");
    fs::create_dir(&bare_dir).unwrap();

    inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&bare_dir)
        .args(["metrics", "snapshot", "--json"])
        .assert()
        .failure();
}

#[test]
fn status_json_gains_the_additive_metrics_field_without_disturbing_stable_keys() {
    let home = TempDir::new().unwrap();
    let (project_dir, config_path) = indexed_project(home.path());

    // Seed one memory entry so the memory store exists.
    inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "Use X",
            "--body",
            "because Y",
        ])
        .assert()
        .success();

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

    // Stable schema keys (issue #269) survive the addition (additive-only contract).
    for key in [
        "version",
        "project",
        "db_path",
        "indexed_files",
        "total_chunks",
        "languages",
        "embedding_dim",
        "has_semantic_search",
        "last_indexed_at",
        "memory_entries",
        "memory_backend",
    ] {
        assert!(
            body.get(key).is_some(),
            "stable key `{key}` must survive the additive `metrics` field: {body}"
        );
    }

    let metrics = &body["metrics"];
    assert!(
        !metrics.is_null(),
        "metrics must be present once a memory store exists: {body}"
    );
    assert!(metrics["rec.entries_in_window"].is_object());
    assert!(
        metrics.get("rec.near_duplicate_rate").is_none(),
        "status must never carry the near-duplicate scan: {metrics}"
    );
    assert!(
        metrics.get("cmp.lines_per_decision").is_none(),
        "status must never carry the numstat-derived lines_per_decision: {metrics}"
    );
}

#[test]
fn status_text_shows_the_compact_metrics_section_once_a_memory_store_exists() {
    let home = TempDir::new().unwrap();
    let (project_dir, config_path) = indexed_project(home.path());

    inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&project_dir)
        .arg("--config")
        .arg(&config_path)
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "Use X",
            "--body",
            "because Y",
        ])
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
        .stdout(predicates::str::contains("Metrics ("))
        .stdout(predicates::str::contains("entries"))
        .stdout(predicates::str::contains("conflicts"));
}
