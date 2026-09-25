use crate::plumbing_helpers;
use plumbing_helpers::inkentry_bin;

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

// A literal rather than `NOTE_KINDS`, so this end-to-end test pins the user-visible contract independently.
const VALID_KINDS: [&str; 9] = [
    "decision",
    "context",
    "requirement",
    "note",
    "question",
    "answer",
    "handoff",
    "intent",
    "antipattern",
];

// No git repo needed: the only store is the SQLite memory.db that `--db` points at.
fn write_config(dir: &Path, mem_db: &Path) -> PathBuf {
    let content = format!(
        "db_path = {:?}\nllm_model = \"x\"\nstore_in_git_notes = false\n",
        mem_db,
    );
    let cfg = dir.join("config.toml");
    std::fs::write(&cfg, content).expect("write config.toml");
    cfg
}

// `INKENTRY_NO_SERVER` keeps the embed phase offline and deterministic (the note is stored without a vector).
fn memory_add_cmd(dir: &Path, cfg: &Path, mem_db: &Path) -> Command {
    let mut cmd = inkentry_bin();
    cmd.current_dir(dir)
        .env("INKENTRY_NO_SERVER", "1")
        .env_remove("INKENTRY_SERVER_URL")
        .arg("--config")
        .arg(cfg)
        .arg("memory")
        .arg("--db")
        .arg(mem_db)
        .arg("add");
    cmd
}

fn row_count(mem_db: &Path) -> i64 {
    if !mem_db.exists() {
        return 0;
    }
    let conn = rusqlite::Connection::open(mem_db).expect("open memory db");
    conn.query_row("SELECT COUNT(*) FROM notes", [], |r| r.get::<_, i64>(0))
        .unwrap_or(0)
}

#[test]
fn each_canonical_kind_is_accepted_and_stored() {
    for kind in VALID_KINDS {
        let tmp = TempDir::new().unwrap();
        let mem_db = tmp.path().join("memory.db");
        let cfg = write_config(tmp.path(), &mem_db);

        memory_add_cmd(tmp.path(), &cfg, &mem_db)
            .arg("--kind")
            .arg(kind)
            .arg("--title")
            .arg("a title")
            .arg("--body")
            .arg("a body")
            .assert()
            .success()
            .stdout(predicate::str::contains(format!("Stored [{kind}]")));

        assert_eq!(
            row_count(&mem_db),
            1,
            "kind {kind} should store exactly one row"
        );
    }
}

#[test]
fn omitting_kind_defaults_to_note() {
    let tmp = TempDir::new().unwrap();
    let mem_db = tmp.path().join("memory.db");
    let cfg = write_config(tmp.path(), &mem_db);

    memory_add_cmd(tmp.path(), &cfg, &mem_db)
        .arg("--title")
        .arg("a title")
        .arg("--body")
        .arg("a body")
        .assert()
        .success()
        .stdout(predicate::str::contains("Stored [note]"));

    assert_eq!(row_count(&mem_db), 1);
}

#[test]
fn unknown_kind_is_rejected_and_stores_nothing() {
    let tmp = TempDir::new().unwrap();
    let mem_db = tmp.path().join("memory.db");
    let cfg = write_config(tmp.path(), &mem_db);

    memory_add_cmd(tmp.path(), &cfg, &mem_db)
        .arg("--kind")
        .arg("bogus")
        .arg("--title")
        .arg("a title")
        .arg("--body")
        .arg("a body")
        .assert()
        .failure()
        .stderr(predicate::str::contains("bogus"))
        .stderr(predicate::str::contains("decision"))
        .stderr(predicate::str::contains("note"))
        .stderr(predicate::str::contains("antipattern"));

    assert_eq!(row_count(&mem_db), 0, "an unknown kind must store no row");
}

#[test]
fn realistic_typo_kinds_are_rejected_and_store_nothing() {
    // Typos that would silently drop a decision from every retrieval path.
    for typo in ["decisions", "desicion"] {
        let tmp = TempDir::new().unwrap();
        let mem_db = tmp.path().join("memory.db");
        let cfg = write_config(tmp.path(), &mem_db);

        memory_add_cmd(tmp.path(), &cfg, &mem_db)
            .arg("--kind")
            .arg(typo)
            .arg("--title")
            .arg("a title")
            .arg("--body")
            .arg("a body")
            .assert()
            .failure()
            .stderr(predicate::str::contains(typo))
            .stderr(predicate::str::contains("decision"));

        assert_eq!(row_count(&mem_db), 0, "typo kind {typo} must store no row");
    }
}
