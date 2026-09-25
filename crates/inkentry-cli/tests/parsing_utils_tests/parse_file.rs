use crate::plumbing_helpers;
use plumbing_helpers::{inkentry_bin, parse_jsonl};

use predicates::prelude::*;
use std::path::Path;
use tempfile::TempDir;

fn fixture_main() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/simple-project/src/main.rs")
}

fn fixture_lib() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/simple-project/src/lib.rs")
}

// parse-file does not use the DB, but the binary needs the global `--config` path to exist.
fn dummy_config(tmp: &TempDir) -> std::path::PathBuf {
    let cfg = tmp.path().join("config.toml");
    std::fs::write(&cfg, "llm_model = \"x\"\n").unwrap();
    cfg
}

#[test]
fn parse_file_emits_jsonl_for_rust_file() {
    let tmp = TempDir::new().unwrap();
    let config = dummy_config(&tmp);

    let output = inkentry_bin()
        .arg("--config")
        .arg(&config)
        .arg("plumbing")
        .arg("parse-file")
        .arg(fixture_main())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows = parse_jsonl(&output);
    assert!(!rows.is_empty(), "expected at least one parsed chunk");

    for row in &rows {
        assert!(row.get("kind").is_some(), "missing 'kind': {row}");
        assert!(
            row.get("start_line").is_some(),
            "missing 'start_line': {row}"
        );
        assert!(row.get("end_line").is_some(), "missing 'end_line': {row}");
        assert!(row.get("content").is_some(), "missing 'content': {row}");
        assert!(row.get("language").is_some(), "missing 'language': {row}");
        assert_eq!(
            row["language"].as_str().unwrap(),
            "rust",
            "language should be rust"
        );
    }
}

#[test]
fn parse_file_finds_function_and_struct_chunks() {
    let tmp = TempDir::new().unwrap();
    let config = dummy_config(&tmp);

    let output = inkentry_bin()
        .arg("--config")
        .arg(&config)
        .arg("plumbing")
        .arg("parse-file")
        .arg(fixture_lib())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows = parse_jsonl(&output);

    let kinds: Vec<&str> = rows.iter().filter_map(|r| r["kind"].as_str()).collect();
    assert!(
        kinds.contains(&"function"),
        "expected a 'function' chunk; got {kinds:?}"
    );
    assert!(
        kinds.contains(&"struct"),
        "expected a 'struct' chunk; got {kinds:?}"
    );
}

#[test]
fn parse_file_exits_1_for_unsupported_file_type() {
    let tmp = TempDir::new().unwrap();
    let config = dummy_config(&tmp);

    let unknown = tmp.path().join("file.xyz123");
    std::fs::write(&unknown, "some content").unwrap();

    inkentry_bin()
        .arg("--config")
        .arg(&config)
        .arg("plumbing")
        .arg("parse-file")
        .arg(&unknown)
        .assert()
        .code(1);
}

#[test]
fn parse_file_exits_nonzero_for_missing_file() {
    let tmp = TempDir::new().unwrap();
    let config = dummy_config(&tmp);

    inkentry_bin()
        .arg("--config")
        .arg(&config)
        .arg("plumbing")
        .arg("parse-file")
        .arg("/nonexistent/path/file.rs")
        .assert()
        .failure()
        .stderr(predicate::str::contains("reading"));
}

#[test]
fn parse_file_exits_nonzero_missing_argument() {
    let tmp = TempDir::new().unwrap();
    let config = dummy_config(&tmp);

    inkentry_bin()
        .arg("--config")
        .arg(&config)
        .arg("plumbing")
        .arg("parse-file")
        .assert()
        .failure()
        .stderr(predicate::str::contains("required"));
}
