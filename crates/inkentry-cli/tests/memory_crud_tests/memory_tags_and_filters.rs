use crate::plumbing_helpers;
use plumbing_helpers::{inkentry_bin, write_config};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

// memory.db must sit at <root>/.inkentry/memory.db: linked-file resolution
// derives the project root from its parent's parent.
fn project() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = TempDir::new().unwrap();
    let inkentry_dir = tmp.path().join(".inkentry");
    std::fs::create_dir_all(&inkentry_dir).unwrap();
    let db_path = inkentry_dir.join("inkentry.db");
    let mem_path = db_path.with_file_name("memory.db");
    let config_path = write_config(tmp.path(), &db_path, "http://127.0.0.1:1");
    (tmp, mem_path, config_path)
}

fn memory_cmd(mem_path: &std::path::Path, config_path: &std::path::Path) -> Command {
    let mut cmd = inkentry_bin();
    cmd.arg("--config")
        .arg(config_path)
        .arg("memory")
        .arg("--db")
        .arg(mem_path);
    cmd
}

fn add_note(
    tmp: &TempDir,
    mem_path: &std::path::Path,
    config_path: &std::path::Path,
    title: &str,
    tags: &str,
    files: &str,
) {
    let mut cmd = memory_cmd(mem_path, config_path);
    cmd.current_dir(tmp.path())
        .arg("add")
        .arg("--kind")
        .arg("note")
        .arg("--title")
        .arg(title)
        .arg("--body")
        .arg("body");
    if !tags.is_empty() {
        cmd.arg("--tags").arg(tags);
    }
    if !files.is_empty() {
        cmd.arg("--files").arg(files);
    }
    cmd.assert().success();
}

#[test]
fn memory_tags_lists_normalised_vocabulary_with_counts() {
    let (tmp, mem_path, config_path) = project();
    add_note(&tmp, &mem_path, &config_path, "one", "Auth,billing", "");
    add_note(&tmp, &mem_path, &config_path, "two", "auth_service", "");

    let output = memory_cmd(&mem_path, &config_path)
        .arg("tags")
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = std::str::from_utf8(&output).unwrap();
    let rows: Vec<serde_json::Value> = serde_json::from_str(text).expect("valid json array");

    let by_tag: std::collections::HashMap<String, i64> = rows
        .iter()
        .map(|r| {
            (
                r["tag"].as_str().unwrap().to_string(),
                r["count"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(by_tag.get("auth"), Some(&1), "Auth normalises to auth");
    assert_eq!(by_tag.get("billing"), Some(&1));
    assert_eq!(
        by_tag.get("auth-service"),
        Some(&1),
        "auth_service normalises to auth-service"
    );
    assert!(
        !by_tag.contains_key("Auth"),
        "the original spelling is not kept: {by_tag:?}"
    );
}

#[test]
fn memory_list_tag_filter_is_exact_after_normalisation() {
    let (tmp, mem_path, config_path) = project();
    add_note(&tmp, &mem_path, &config_path, "tagged", "Auth-Service", "");
    add_note(&tmp, &mem_path, &config_path, "untagged", "billing", "");

    let output = memory_cmd(&mem_path, &config_path)
        .arg("list")
        .arg("--tag")
        .arg("AUTH_SERVICE")
        .arg("--format")
        .arg("jsonl")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = std::str::from_utf8(&output).unwrap();
    let rows: Vec<serde_json::Value> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    assert_eq!(rows.len(), 1, "only the tagged note matches: {rows:?}");
    assert_eq!(rows[0]["title"], "tagged");
}

#[test]
fn memory_list_file_filter_is_an_exact_path_match() {
    let (tmp, mem_path, config_path) = project();
    add_note(
        &tmp,
        &mem_path,
        &config_path,
        "linked",
        "",
        "./src/does_not_exist.rs",
    );
    add_note(&tmp, &mem_path, &config_path, "unlinked", "", "");

    let output = memory_cmd(&mem_path, &config_path)
        .arg("list")
        .arg("--file")
        .arg("src/does_not_exist.rs")
        .arg("--format")
        .arg("jsonl")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = std::str::from_utf8(&output).unwrap();
    let rows: Vec<serde_json::Value> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(rows.len(), 1, "only the linked note matches: {rows:?}");
    assert_eq!(rows[0]["title"], "linked");

    // Exact match, unlike `context --path`: a prefix or directory substring must not match.
    let output = memory_cmd(&mem_path, &config_path)
        .arg("list")
        .arg("--file")
        .arg("src")
        .arg("--format")
        .arg("jsonl")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(
        std::str::from_utf8(&output).unwrap().trim().is_empty()
            || !std::str::from_utf8(&output).unwrap().contains("linked"),
        "a directory substring must not match the exact-path filter"
    );
}

#[test]
fn memory_add_reports_missing_linked_file_state_without_refusing() {
    let (tmp, mem_path, config_path) = project();

    let assert = memory_cmd(&mem_path, &config_path)
        .current_dir(tmp.path())
        .arg("add")
        .arg("--kind")
        .arg("note")
        .arg("--title")
        .arg("missing file link")
        .arg("--body")
        .arg("body")
        .arg("--files")
        .arg("does/not/exist.rs")
        .arg("--format")
        .arg("json")
        .assert()
        .success();

    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("missing"),
        "expected a missing-file warning on stderr: {stderr}"
    );

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let obj: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    let linked = obj["linked_files"]
        .as_array()
        .expect("linked_files array in json output");
    assert_eq!(linked.len(), 1);
    assert_eq!(linked[0]["path"], "does/not/exist.rs");
    assert_eq!(linked[0]["state"], "missing");
}

#[test]
fn memory_add_refuses_a_linked_file_that_escapes_the_project_root() {
    let (tmp, mem_path, config_path) = project();

    memory_cmd(&mem_path, &config_path)
        .current_dir(tmp.path())
        .arg("add")
        .arg("--kind")
        .arg("note")
        .arg("--title")
        .arg("escaping link")
        .arg("--body")
        .arg("body")
        .arg("--files")
        .arg("../../etc/passwd")
        .assert()
        .failure()
        .stderr(predicate::str::contains("escapes the project root"));
}
