use crate::plumbing_helpers;
use plumbing_helpers::{index_fixture_project, inkentry_bin, inkentry_cmd, parse_jsonl};

use predicates::prelude::*;
use tempfile::TempDir;

#[test]
fn ls_files_emits_jsonl_for_indexed_project() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    let output = inkentry_cmd(&db_path, &config_path)
        .arg("ls-files")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows = parse_jsonl(&output);
    assert!(!rows.is_empty(), "expected at least one file entry");

    for row in &rows {
        assert!(row.get("path").is_some(), "missing 'path': {row}");
        assert!(
            row.get("chunk_count").is_some(),
            "missing 'chunk_count': {row}"
        );
        assert!(
            row.get("indexed_at").is_some(),
            "missing 'indexed_at': {row}"
        );
        assert!(row.get("stale").is_some(), "missing 'stale': {row}");
        assert!(
            row["chunk_count"].as_u64().unwrap_or(0) >= 1,
            "chunk_count should be >= 1, got: {row}"
        );
    }
}

#[test]
fn ls_files_prefix_filter_narrows_results() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    inkentry_cmd(&db_path, &config_path)
        .arg("ls-files")
        .arg("--prefix")
        .arg("/does/not/exist/")
        .assert()
        .code(1);
}

#[test]
fn ls_files_stale_flag_returns_subset_or_empty() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    let all_output = inkentry_cmd(&db_path, &config_path)
        .arg("ls-files")
        .output()
        .unwrap();
    let all_rows = parse_jsonl(&all_output.stdout);
    let all_count = all_rows.len();

    let stale_output = inkentry_cmd(&db_path, &config_path)
        .arg("ls-files")
        .arg("--stale")
        .output()
        .unwrap();
    let stale_rows = parse_jsonl(&stale_output.stdout);

    assert!(
        stale_rows.len() <= all_count,
        "--stale results ({}) should not exceed total ({})",
        stale_rows.len(),
        all_count
    );
    for row in &stale_rows {
        assert_eq!(
            row["stale"].as_bool(),
            Some(true),
            "--stale should only return stale entries: {row}"
        );
    }
}

#[test]
fn ls_files_stale_exits_1_when_no_stale_files() {
    // A freshly indexed project has no changed files, so --stale emits nothing and exits 1.
    let (_tmp, db_path, config_path) = index_fixture_project();

    inkentry_cmd(&db_path, &config_path)
        .arg("ls-files")
        .arg("--stale")
        .arg("--root")
        .arg(plumbing_helpers::fixture_path())
        .assert()
        .code(1);
}

#[test]
fn ls_files_exits_nonzero_when_db_missing() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("config.toml");
    let db_path = tmp.path().join("nonexistent.db");

    std::fs::write(
        &config_path,
        format!("db_path = {:?}\nllm_model = \"x\"\n", db_path),
    )
    .unwrap();

    inkentry_bin()
        .arg("--config")
        .arg(&config_path)
        .arg("plumbing")
        .arg("--db")
        .arg(&db_path)
        .arg("ls-files")
        .assert()
        .failure()
        .stderr(predicate::str::contains("No index found"));
}

#[test]
fn plumbing_exits_2_on_error() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("config.toml");
    let db_path = tmp.path().join("nonexistent.db");

    std::fs::write(
        &config_path,
        format!("db_path = {:?}\nllm_model = \"x\"\n", db_path),
    )
    .unwrap();

    inkentry_bin()
        .arg("--config")
        .arg(&config_path)
        .arg("plumbing")
        .arg("--db")
        .arg(&db_path)
        .arg("ls-files")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("No index found"));
}
