// `--relates-to` must record a `relates_to` edge visible from both endpoints while
// archiving neither entry (unlike `--supersedes`). `store_in_git_notes = false`, so no git
// repo is needed.

use crate::plumbing_helpers;
use plumbing_helpers::inkentry_bin;

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn write_config(dir: &Path, mem_db: &Path) -> PathBuf {
    let content = format!(
        "db_path = {:?}\nllm_model = \"x\"\nstore_in_git_notes = false\n",
        mem_db,
    );
    let cfg = dir.join("config.toml");
    std::fs::write(&cfg, content).expect("write config.toml");
    cfg
}

fn memory_cmd(dir: &Path, cfg: &Path, mem_db: &Path) -> Command {
    let mut cmd = inkentry_bin();
    cmd.current_dir(dir)
        .env_remove("INKENTRY_SERVER_URL")
        .arg("--config")
        .arg(cfg)
        .arg("memory")
        .arg("--db")
        .arg(mem_db);
    cmd
}

fn add_note(dir: &Path, cfg: &Path, mem_db: &Path, title: &str, extra: &[&str]) -> String {
    let mut cmd = memory_cmd(dir, cfg, mem_db);
    cmd.arg("add")
        .arg("--kind")
        .arg("note")
        .arg("--title")
        .arg(title)
        .arg("--body")
        .arg("body text with no secrets");
    for a in extra {
        cmd.arg(a);
    }
    let out = cmd.output().expect("run memory add");
    assert!(
        out.status.success(),
        "memory add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    parse_stored_id(&stdout)
}

// The lead line shows the portable handle; the row id is on its own `id:` line.
fn parse_stored_id(stdout: &str) -> String {
    let id = stdout
        .lines()
        .find_map(|l| l.trim_start().strip_prefix("id:"))
        .map(str::trim)
        .unwrap_or_else(|| panic!("no id line in stored output: {stdout:?}"));
    assert!(
        uuid::Uuid::parse_str(id).is_ok(),
        "stored id must be a UUID, got {id:?} in: {stdout:?}"
    );
    id.to_string()
}

fn note_row_count(mem_db: &Path) -> i64 {
    if !mem_db.exists() {
        return 0;
    }
    let conn = rusqlite::Connection::open(mem_db).expect("open memory db");
    conn.query_row("SELECT COUNT(*) FROM notes", [], |r| r.get::<_, i64>(0))
        .unwrap_or(0)
}

fn note_status(mem_db: &Path, id: &str) -> String {
    let conn = rusqlite::Connection::open(mem_db).expect("open memory db");
    conn.query_row("SELECT status FROM notes WHERE uuid = ?1", [id], |r| {
        r.get::<_, String>(0)
    })
    .expect("note status")
}

fn superseded_by(mem_db: &Path, id: &str) -> Option<String> {
    let conn = rusqlite::Connection::open(mem_db).expect("open memory db");
    conn.query_row(
        "SELECT superseded_by FROM notes WHERE uuid = ?1",
        [id],
        |r| r.get::<_, Option<String>>(0),
    )
    .expect("superseded_by")
}

fn edge_count(mem_db: &Path, from_id: &str, to_id: &str, kind: &str) -> i64 {
    let conn = rusqlite::Connection::open(mem_db).expect("open memory db");
    conn.query_row(
        "SELECT COUNT(*) FROM memory_edges WHERE from_id = ?1 AND to_id = ?2 AND kind = ?3",
        rusqlite::params![from_id, to_id, kind],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0)
}

fn total_edges(mem_db: &Path) -> i64 {
    if !mem_db.exists() {
        return 0;
    }
    let conn = rusqlite::Connection::open(mem_db).expect("open memory db");
    conn.query_row("SELECT COUNT(*) FROM memory_edges", [], |r| {
        r.get::<_, i64>(0)
    })
    .unwrap_or(0)
}

fn graph_json(dir: &Path, cfg: &Path, mem_db: &Path, id: &str) -> Value {
    let out = memory_cmd(dir, cfg, mem_db)
        .arg("graph")
        .arg(id)
        .arg("--format")
        .arg("json")
        .output()
        .expect("run memory graph");
    assert!(
        out.status.success(),
        "memory graph failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("parse memory graph json")
}

fn has_edge(edges: &Value, endpoint_field: &str, other: &str, kind: &str) -> bool {
    edges
        .as_array()
        .map(|arr| {
            arr.iter()
                .any(|e| e[endpoint_field].as_str() == Some(other) && e["kind"] == kind)
        })
        .unwrap_or(false)
}

#[test]
fn relates_to_writes_a_bidirectional_edge_and_archives_neither_entry() {
    let tmp = TempDir::new().unwrap();
    let mem_db = tmp.path().join("memory.db");
    let cfg = write_config(tmp.path(), &mem_db);

    let target = add_note(tmp.path(), &cfg, &mem_db, "Original observation", &[]);
    let linker = add_note(
        tmp.path(),
        &cfg,
        &mem_db,
        "Contradicting observation",
        &["--relates-to", &target],
    );

    assert_eq!(
        total_edges(&mem_db),
        1,
        "expected exactly one edge after --relates-to"
    );
    assert_eq!(
        edge_count(&mem_db, &linker, &target, "relates_to"),
        1,
        "expected a relates_to edge #{linker} -> #{target}"
    );

    assert_eq!(
        note_status(&mem_db, &target),
        "active",
        "target must stay active"
    );
    assert_eq!(
        note_status(&mem_db, &linker),
        "active",
        "linker must stay active"
    );
    assert_eq!(superseded_by(&mem_db, &target), None);
    assert_eq!(superseded_by(&mem_db, &linker), None);

    let from_linker = graph_json(tmp.path(), &cfg, &mem_db, &linker);
    assert!(
        has_edge(&from_linker["outgoing"], "to_id", &target, "relates_to"),
        "graph from #{linker} must show outgoing relates_to -> #{target}: {from_linker}"
    );

    let from_target = graph_json(tmp.path(), &cfg, &mem_db, &target);
    assert!(
        has_edge(&from_target["incoming"], "from_id", &linker, "relates_to"),
        "graph from #{target} must show incoming relates_to from #{linker}: {from_target}"
    );
}

#[test]
fn relates_to_a_missing_target_is_rejected_and_stores_nothing() {
    let tmp = TempDir::new().unwrap();
    let mem_db = tmp.path().join("memory.db");
    let cfg = write_config(tmp.path(), &mem_db);

    memory_cmd(tmp.path(), &cfg, &mem_db)
        .arg("add")
        .arg("--kind")
        .arg("note")
        .arg("--title")
        .arg("Dangling link")
        .arg("--body")
        .arg("body text with no secrets")
        .arg("--relates-to")
        .arg("999")
        .assert()
        .failure()
        .stderr(predicate::str::contains("'999' is not a memory entry id"));

    assert_eq!(
        note_row_count(&mem_db),
        0,
        "a rejected --relates-to must not leave an orphaned entry"
    );
    assert_eq!(
        total_edges(&mem_db),
        0,
        "no edge on a rejected --relates-to"
    );
}

#[test]
fn supersedes_still_archives_while_relates_to_does_not() {
    let tmp = TempDir::new().unwrap();
    let mem_db = tmp.path().join("memory.db");
    let cfg = write_config(tmp.path(), &mem_db);

    let old = add_note(tmp.path(), &cfg, &mem_db, "Old decision", &[]);
    let new = add_note(
        tmp.path(),
        &cfg,
        &mem_db,
        "New decision",
        &["--supersedes", &old],
    );
    assert_eq!(
        note_status(&mem_db, &old),
        "archived",
        "--supersedes must archive OLD"
    );
    assert_eq!(superseded_by(&mem_db, &old), Some(new.clone()));
    assert_eq!(edge_count(&mem_db, &new, &old, "supersedes"), 1);

    let a = add_note(tmp.path(), &cfg, &mem_db, "Note A", &[]);
    let b = add_note(tmp.path(), &cfg, &mem_db, "Note B", &["--relates-to", &a]);
    assert_eq!(
        note_status(&mem_db, &a),
        "active",
        "--relates-to must NOT archive its target"
    );
    assert_eq!(note_status(&mem_db, &b), "active");
    assert_eq!(edge_count(&mem_db, &b, &a, "relates_to"), 1);
    assert_eq!(
        edge_count(&mem_db, &b, &a, "supersedes"),
        0,
        "--relates-to must not write a supersedes edge"
    );
}
