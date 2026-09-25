use crate::plumbing_helpers;
use plumbing_helpers::{index_fixture_project, inkentry_bin, inkentry_cmd, parse_jsonl};

use predicates::prelude::*;
use tempfile::TempDir;

#[test]
fn cat_chunks_emits_jsonl_for_indexed_file() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    // The DB stores absolute paths, so match by suffix.
    let output = inkentry_cmd(&db_path, &config_path)
        .arg("cat-chunks")
        .arg("src/lib.rs")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows = parse_jsonl(&output);
    assert!(!rows.is_empty(), "expected at least one chunk for lib.rs");

    for row in &rows {
        assert!(row.get("chunk_id").is_some(), "missing 'chunk_id': {row}");
        assert!(row.get("file_path").is_some(), "missing 'file_path': {row}");
        assert!(row.get("content").is_some(), "missing 'content': {row}");
        assert!(
            row.get("start_line").is_some(),
            "missing 'start_line': {row}"
        );
        assert!(row.get("end_line").is_some(), "missing 'end_line': {row}");
        assert!(row.get("language").is_some(), "missing 'language': {row}");
        assert_eq!(
            row["language"].as_str().unwrap_or(""),
            "rust",
            "language should be rust"
        );
    }
}

#[test]
fn cat_chunks_output_includes_function_name() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    let output = inkentry_cmd(&db_path, &config_path)
        .arg("cat-chunks")
        .arg("src/lib.rs")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows = parse_jsonl(&output);
    let names: Vec<&str> = rows.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(
        names.contains(&"greet"),
        "expected a chunk named 'greet', got: {names:?}"
    );
}

// Runs on all OSes: `\` normalises to `/` regardless of host.
#[test]
fn cat_chunks_matches_backslash_query_path() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    let output = inkentry_cmd(&db_path, &config_path)
        .arg("cat-chunks")
        .arg("src\\lib.rs")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows = parse_jsonl(&output);
    assert!(
        !rows.is_empty(),
        "expected chunks for a backslash-style query path 'src\\\\lib.rs'"
    );
}

#[test]
fn cat_chunks_exits_1_for_unknown_file() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    inkentry_cmd(&db_path, &config_path)
        .arg("cat-chunks")
        .arg("does/not/exist.rs")
        .assert()
        .code(1)
        .stderr(predicate::str::contains("No indexed chunks"));
}

#[test]
fn cat_chunks_exits_nonzero_when_db_missing() {
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
        .arg("cat-chunks")
        .arg("src/lib.rs")
        .assert()
        .failure()
        .stderr(predicate::str::contains("No index found"));
}

#[test]
fn cat_chunks_exits_nonzero_missing_argument() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("config.toml");
    std::fs::write(&config_path, "llm_model = \"x\"\n").unwrap();

    inkentry_bin()
        .arg("--config")
        .arg(&config_path)
        .arg("plumbing")
        .arg("cat-chunks")
        .assert()
        .failure()
        .stderr(predicate::str::contains("required"));
}
