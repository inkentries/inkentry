// Component tests for `inkentry plumbing graph-edges`.
//
// Paths here are the paths the index stores, which are relative to the indexed
// project root (`src/main.rs`), not to the fixture directory
// (`simple-project/src/main.rs`). Filtering on the latter matches nothing and
// exits 2, so a test written as "exit 0 or non-zero" over such a path never
// runs its assertions at all.

use crate::plumbing_helpers;
use plumbing_helpers::{
    index_fixture_project, index_project_dir, inkentry_bin, inkentry_cmd, parse_jsonl,
};

use predicates::prelude::*;
use serde_json::Value;
use tempfile::TempDir;

fn has_edge(rows: &[Value], source: &str, target: &str, kind: &str) -> bool {
    rows.iter().any(|row| {
        row["source_name"] == *source && row["target_name"] == *target && row["kind"] == *kind
    })
}

fn assert_edge_fields(rows: &[Value]) {
    assert!(!rows.is_empty(), "expected at least one edge");
    for row in rows {
        for field in ["source_file", "source_name", "target_name", "kind", "line"] {
            assert!(row.get(field).is_some(), "missing {field:?}: {row}");
        }
    }
}

// ── happy path: file filter ───────────────────────────────────────────────────

#[test]
fn graph_edges_file_filter_emits_the_files_call_edges() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    let result = inkentry_cmd(&db_path, &config_path)
        .arg("graph-edges")
        .arg("--file")
        .arg("src/utils.rs")
        .output()
        .unwrap();

    assert_eq!(
        result.status.code(),
        Some(0),
        "utils.rs has call edges, so an empty result is a regression\nstderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let rows = parse_jsonl(&result.stdout);
    assert_edge_fields(&rows);
    assert!(
        has_edge(&rows, "sum_slice", "sum", "calls"),
        "expected the `sum_slice -> sum` call edge: {rows:?}"
    );
    assert!(
        rows.iter().all(|row| row["source_file"] == "src/utils.rs"),
        "a --file filter must not leak edges from other files: {rows:?}"
    );
}

#[test]
fn graph_edges_main_file_emits_both_call_and_import_edges() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    let result = inkentry_cmd(&db_path, &config_path)
        .arg("graph-edges")
        .arg("--file")
        .arg("src/main.rs")
        .output()
        .unwrap();

    assert_eq!(
        result.status.code(),
        Some(0),
        "main.rs calls `greet` and imports it, so it has edges\nstderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let rows = parse_jsonl(&result.stdout);
    assert_edge_fields(&rows);
    assert!(
        has_edge(&rows, "main", "greet", "calls"),
        "expected the `main -> greet` call edge: {rows:?}"
    );
    // An import edge has no enclosing symbol, which is why `source_name` is
    // nullable in the JSONL contract rather than always a string.
    let import = rows
        .iter()
        .find(|row| row["kind"] == "imports")
        .unwrap_or_else(|| panic!("expected an import edge: {rows:?}"));
    assert!(
        import["source_name"].is_null(),
        "an import edge is not attributed to a symbol: {import}"
    );
}

// ── resolved target file ──────────────────────────────────────────────────────

fn edge_row<'a>(rows: &'a [Value], source: &str, target: &str) -> &'a Value {
    rows.iter()
        .find(|row| {
            row["source_name"] == *source && row["target_name"] == *target && row["kind"] == "calls"
        })
        .unwrap_or_else(|| panic!("expected a `{source} -> {target}` call edge: {rows:?}"))
}

#[test]
fn graph_edges_carries_target_file_only_for_a_resolved_edge() {
    let project = TempDir::new().unwrap();
    std::fs::create_dir_all(project.path().join("src")).unwrap();
    std::fs::write(
        project.path().join("src/a.rs"),
        "fn helper() {}\npub fn go() {\n    helper();\n    other();\n}\n",
    )
    .unwrap();
    std::fs::write(project.path().join("src/b.rs"), "pub fn other() {}\n").unwrap();
    let (_tmp, db_path, config_path) = index_project_dir(project.path());

    let out = inkentry_cmd(&db_path, &config_path)
        .args(["graph-edges", "--file", "src/a.rs"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows = parse_jsonl(&out);

    assert_eq!(
        edge_row(&rows, "go", "helper")["target_file"],
        "src/a.rs",
        "`helper` is defined in the calling file"
    );
    let unresolved = edge_row(&rows, "go", "other");
    assert!(
        unresolved.get("target_file").is_none(),
        "a callee defined only in another file is not placed: {unresolved}"
    );
}

#[test]
fn graph_edges_keeps_rows_that_differ_only_in_target_file_when_filters_merge() {
    // One line calls `helper` twice: the bare call binds to the import and
    // stays unresolved, the receiver call reaches this file's own method.
    let project = TempDir::new().unwrap();
    std::fs::create_dir_all(project.path().join("src")).unwrap();
    std::fs::write(
        project.path().join("src/a.ts"),
        "import { helper } from './x';\n\
         class A {\n  helper() { return 1; }\n  run() { helper(); this.helper(); }\n}\n",
    )
    .unwrap();
    std::fs::write(
        project.path().join("src/b.ts"),
        "export function helper() { return 2; }\n",
    )
    .unwrap();
    let (_tmp, db_path, config_path) = index_project_dir(project.path());

    let out = inkentry_cmd(&db_path, &config_path)
        // The two rows arrive through `--symbol`, so it is the merge's own
        // de-duplication that has to keep them apart.
        .args(["graph-edges", "--file", "src/b.ts", "--symbol", "helper"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows = parse_jsonl(&out);
    let mut targets: Vec<Option<&str>> = rows
        .iter()
        .filter(|row| row["target_name"] == "helper" && row["kind"] == "calls")
        .map(|row| row.get("target_file").and_then(Value::as_str))
        .collect();
    targets.sort();
    assert_eq!(targets, vec![None, Some("src/a.ts")], "{rows:?}");
}

// ── symbol filter ─────────────────────────────────────────────────────────────

#[test]
fn graph_edges_symbol_filter_finds_edges_across_files() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    let result = inkentry_cmd(&db_path, &config_path)
        .arg("graph-edges")
        .arg("--symbol")
        .arg("greet")
        .output()
        .unwrap();

    assert_eq!(
        result.status.code(),
        Some(0),
        "`greet` is defined in lib.rs and called from main.rs\nstderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let rows = parse_jsonl(&result.stdout);
    assert_edge_fields(&rows);
    assert!(
        has_edge(&rows, "main", "greet", "calls"),
        "the symbol filter must reach the caller in another file: {rows:?}"
    );
}

#[test]
fn graph_edges_symbol_filter_finds_edges_out_of_the_definition() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    // `greet` only calls `format!`, a builtin the graph skips, so the edge out
    // of a definition is exercised on `sum_slice`, which calls `sum`.
    let result = inkentry_cmd(&db_path, &config_path)
        .arg("graph-edges")
        .arg("--symbol")
        .arg("sum_slice")
        .output()
        .unwrap();

    assert_eq!(
        result.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let rows = parse_jsonl(&result.stdout);
    assert_edge_fields(&rows);
    assert!(
        has_edge(&rows, "sum_slice", "sum", "calls"),
        "the symbol filter must reach edges out of the definition: {rows:?}"
    );
}

// ── a path the index does not store ───────────────────────────────────────────

#[test]
fn graph_edges_exits_2_for_a_path_the_index_does_not_store() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    // Stored paths are relative to the indexed root, so a fixture-relative path
    // matches nothing. That is a hard error naming the path, not an empty set:
    // a silent exit here is what made the earlier file-filter tests
    // unfalsifiable, and what let a mistyped path pass for a file with no edges.
    inkentry_cmd(&db_path, &config_path)
        .arg("graph-edges")
        .arg("--file")
        .arg("simple-project/src/main.rs")
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("simple-project/src/main.rs"));
}

// ── no results (exit 1) ───────────────────────────────────────────────────────

#[test]
fn graph_edges_exits_1_for_nonexistent_symbol() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    inkentry_cmd(&db_path, &config_path)
        .arg("graph-edges")
        .arg("--symbol")
        .arg("symbol_that_does_not_exist_xyz")
        .assert()
        .code(1);
}

// ── error path: no flags ──────────────────────────────────────────────────────

#[test]
fn graph_edges_exits_nonzero_when_no_flags_given() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    inkentry_cmd(&db_path, &config_path)
        .arg("graph-edges")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "at least one of --file or --symbol is required",
        ));
}

// ── error path: missing DB ────────────────────────────────────────────────────

#[test]
fn graph_edges_exits_nonzero_when_db_missing() {
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
        .arg("graph-edges")
        .arg("--symbol")
        .arg("foo")
        .assert()
        .failure()
        .stderr(predicate::str::contains("No index found"));
}
