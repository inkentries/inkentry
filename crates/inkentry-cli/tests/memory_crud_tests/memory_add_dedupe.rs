use crate::plumbing_helpers;
use plumbing_helpers::{init_git_repo, inkentry_bin};

use assert_cmd::Command;
use predicates::prelude::*;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn write_config(dir: &Path, mem_db: &Path) -> PathBuf {
    let cfg = dir.join("config.toml");
    std::fs::write(
        &cfg,
        format!(
            "db_path = {:?}\nllm_model = \"test-model\"\nstore_in_git_notes = true\n",
            mem_db.display().to_string()
        ),
    )
    .expect("write config.toml");
    cfg
}

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
        .arg("add")
        .arg("--kind")
        .arg("decision");
    cmd
}

fn row_count(mem_db: &Path) -> i64 {
    let conn = Connection::open(mem_db).expect("open memory.db");
    conn.query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))
        .unwrap_or(0)
}

// Reads by `note_uuid` and rejoins the tag rows into the comma-joined form callers compare against.
fn note_tags(mem_db: &Path, uuid: &str) -> String {
    let conn = Connection::open(mem_db).expect("open memory.db");
    let mut stmt = conn
        .prepare("SELECT tag FROM note_tags WHERE note_uuid = ?1 ORDER BY tag")
        .expect("prepare");
    let tags: Vec<String> = stmt
        .query_map(rusqlite::params![uuid], |r| r.get(0))
        .expect("query")
        .collect::<rusqlite::Result<_>>()
        .expect("collect");
    tags.join(",")
}

fn git_note_record_entity_ids(dir: &Path) -> Vec<String> {
    let out = std::process::Command::new("git")
        .args(["notes", "--ref=inkentry", "show", "HEAD"])
        .current_dir(dir)
        .output()
        .expect("git notes show HEAD");
    assert!(
        out.status.success(),
        "git notes show HEAD failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l)
                .unwrap_or_else(|e| panic!("git note line is not valid JSON: {l:?}: {e}"));
            v["entity_id"]
                .as_str()
                .expect("record has an entity_id field")
                .to_string()
        })
        .collect()
}

#[test]
fn second_identical_add_reuses_the_row_and_prints_already_recorded() {
    let tmp = TempDir::new().unwrap();
    let mem_db = tmp.path().join("memory.db");
    let cfg = write_config(tmp.path(), &mem_db);

    // The first add promotes `idx_notes_entity_id` to UNIQUE on this `open()`, so the second call hits the promoted index.
    memory_add_cmd(tmp.path(), &cfg, &mem_db)
        .arg("--title")
        .arg("dup entry")
        .arg("--body")
        .arg("same content")
        .assert()
        .success()
        .stdout(predicate::str::contains("Stored [decision]"));

    assert_eq!(row_count(&mem_db), 1, "first add creates one row");

    memory_add_cmd(tmp.path(), &cfg, &mem_db)
        .arg("--title")
        .arg("dup entry")
        .arg("--body")
        .arg("same content")
        .assert()
        .success()
        .stdout(predicate::str::contains("Already recorded as [decision]"))
        .stdout(predicate::str::contains("Stored [decision]").not());

    assert_eq!(
        row_count(&mem_db),
        1,
        "criterion 26/30: a collision must reuse the existing row, not create a second one"
    );
}

#[test]
fn second_identical_add_merges_tags_into_the_existing_row() {
    let tmp = TempDir::new().unwrap();
    let mem_db = tmp.path().join("memory.db");
    let cfg = write_config(tmp.path(), &mem_db);

    memory_add_cmd(tmp.path(), &cfg, &mem_db)
        .arg("--title")
        .arg("dup entry")
        .arg("--body")
        .arg("same content")
        .arg("--tags")
        .arg("alpha")
        .assert()
        .success();

    memory_add_cmd(tmp.path(), &cfg, &mem_db)
        .arg("--title")
        .arg("dup entry")
        .arg("--body")
        .arg("same content")
        .arg("--tags")
        .arg("beta")
        .assert()
        .success()
        .stdout(predicate::str::contains("Already recorded as"));

    let conn = Connection::open(&mem_db).unwrap();
    let uuid: String = conn
        .query_row("SELECT uuid FROM notes LIMIT 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        note_tags(&mem_db, &uuid),
        "alpha,beta",
        "criterion 26: tags must union add-wins, neither dropped"
    );
}

// The carrier's own `id` is a per-write stamp, not identity; a reader resolves on `entity_id`.
#[test]
fn second_identical_add_still_writes_through_to_git_notes_with_the_same_entity_id() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let mem_db = tmp.path().join("memory.db");
    let cfg = write_config(tmp.path(), &mem_db);

    memory_add_cmd(tmp.path(), &cfg, &mem_db)
        .arg("--title")
        .arg("dup entry")
        .arg("--body")
        .arg("same content")
        .assert()
        .success();

    memory_add_cmd(tmp.path(), &cfg, &mem_db)
        .arg("--title")
        .arg("dup entry")
        .arg("--body")
        .arg("same content")
        .assert()
        .success()
        .stdout(predicate::str::contains("Already recorded as"));

    let ids = git_note_record_entity_ids(tmp.path());
    assert_eq!(
        ids.len(),
        2,
        "criterion 34: the carrier must write on BOTH calls, reuse or not, \
         got records: {ids:?}"
    );
    assert_eq!(
        ids[0], ids[1],
        "criterion 34: both records must carry the SAME entity_id, the \
         reused row's, so a later reader can't see two different identities \
         for what SQLite considers a single entry"
    );

    assert_eq!(row_count(&mem_db), 1);
}
