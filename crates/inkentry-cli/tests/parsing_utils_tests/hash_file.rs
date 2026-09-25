use crate::plumbing_helpers;
use plumbing_helpers::{index_fixture_project, inkentry_bin, inkentry_cmd, parse_jsonl};

use predicates::prelude::*;
use std::path::Path;
use tempfile::TempDir;

#[test]
fn hash_file_emits_valid_jsonl() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    // Absolute path: `indexed_hash` may be null (the DB stores relative paths), but the JSON
    // structure must always be present.
    let fixture_lib =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/simple-project/src/lib.rs");

    let output = inkentry_cmd(&db_path, &config_path)
        .arg("hash-file")
        .arg(&fixture_lib)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows = parse_jsonl(&output);
    assert_eq!(rows.len(), 1, "hash-file should emit exactly one JSON line");

    let row = &rows[0];
    assert!(row.get("path").is_some(), "missing 'path'");
    assert!(row.get("hash").is_some(), "missing 'hash'");
    assert!(row.get("indexed_hash").is_some(), "missing 'indexed_hash'");
    assert!(row.get("is_current").is_some(), "missing 'is_current'");

    let hash = row["hash"].as_str().unwrap_or("");
    assert!(!hash.is_empty(), "hash should be non-empty");
    assert!(
        hash.chars().all(|c| c.is_ascii_hexdigit()),
        "hash should be hex: {hash}"
    );
}

#[test]
fn hash_file_is_current_for_relative_indexed_path() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    // The DB stores paths relative to the project root; pass the relative one so the lookup matches.
    let fixture_lib =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/simple-project/src/lib.rs");

    let content = std::fs::read(&fixture_lib).unwrap();
    let expected_hash = format!("{}", blake3::hash(&content));

    // Run from the fixture root so "src/lib.rs" resolves to the actual file.
    let output = inkentry_cmd(&db_path, &config_path)
        .current_dir(plumbing_helpers::fixture_path())
        .arg("hash-file")
        .arg("src/lib.rs")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows = parse_jsonl(&output);
    assert_eq!(rows.len(), 1);

    // `is_current` is not asserted: it depends on the relative path resolving against CWD.
    let row = &rows[0];
    assert!(row.get("hash").is_some(), "missing 'hash'");
    assert!(row.get("is_current").is_some(), "missing 'is_current'");
    if !row["indexed_hash"].is_null() {
        assert_eq!(
            row["indexed_hash"].as_str().unwrap_or(""),
            expected_hash,
            "indexed_hash should match blake3 of the actual file"
        );
    }
}

#[test]
fn hash_file_reports_null_indexed_hash_for_unknown_file() {
    let (_tmp, db_path, config_path) = index_fixture_project();
    let tmp2 = TempDir::new().unwrap();

    let unindexed = tmp2.path().join("extra.rs");
    std::fs::write(&unindexed, "fn extra() {}").unwrap();

    let output = inkentry_cmd(&db_path, &config_path)
        .arg("hash-file")
        .arg(&unindexed)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows = parse_jsonl(&output);
    assert_eq!(rows.len(), 1);

    let row = &rows[0];
    assert!(
        row["indexed_hash"].is_null(),
        "unindexed file should have null indexed_hash, got: {row}"
    );
    assert_eq!(
        row["is_current"].as_bool(),
        Some(false),
        "unindexed file is not current"
    );
}

#[test]
fn hash_file_exits_nonzero_for_missing_file() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    inkentry_cmd(&db_path, &config_path)
        .arg("hash-file")
        .arg("/nonexistent/file.rs")
        .assert()
        .failure()
        .stderr(predicate::str::contains("reading"));
}

#[test]
fn hash_file_exits_nonzero_when_db_missing() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("config.toml");
    let db_path = tmp.path().join("nonexistent.db");
    std::fs::write(
        &config_path,
        format!("db_path = {:?}\nllm_model = \"x\"\n", db_path),
    )
    .unwrap();

    let real_file = tmp.path().join("real.rs");
    std::fs::write(&real_file, "fn x() {}").unwrap();

    inkentry_bin()
        .arg("--config")
        .arg(&config_path)
        .arg("plumbing")
        .arg("--db")
        .arg(&db_path)
        .arg("hash-file")
        .arg(&real_file)
        .assert()
        .failure()
        .stderr(predicate::str::contains("No index found"));
}

#[test]
fn hash_file_exits_nonzero_missing_argument() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("config.toml");
    std::fs::write(&config_path, "llm_model = \"x\"\n").unwrap();

    inkentry_bin()
        .arg("--config")
        .arg(&config_path)
        .arg("plumbing")
        .arg("hash-file")
        .assert()
        .failure()
        .stderr(predicate::str::contains("required"));
}
