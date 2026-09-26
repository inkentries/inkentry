use crate::plumbing_helpers;
use plumbing_helpers::{init_git_repo, inkentry_bin};

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn write_config(dir: &Path, mem_db: &Path, git_notes: bool) -> PathBuf {
    let content = format!(
        concat!(
            "db_path = {:?}\n",
            "llm_model = \"x\"\n",
            "store_in_git_notes = {}\n",
        ),
        mem_db, git_notes,
    );
    let cfg = dir.join("config.toml");
    std::fs::write(&cfg, content).expect("write config.toml");
    cfg
}

fn memory_add_cmd(dir: &Path, cfg: &Path, mem_db: &Path) -> Command {
    let mut cmd = inkentry_bin();
    cmd.current_dir(dir)
        .env_remove("INKENTRY_SERVER_URL")
        .arg("--config")
        .arg(cfg)
        .arg("memory")
        .arg("--db")
        .arg(mem_db)
        .arg("add")
        .arg("--kind")
        .arg("note");
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

// Aborting is right here because one `add` is one entry; the git-commit harvest instead skips the matching
// commit and continues the walk (see `harvest_secret_scan.rs`).
#[test]
fn secret_in_body_exits_nonzero_and_writes_no_sqlite_row() {
    let tmp = TempDir::new().unwrap();
    let mem_db = tmp.path().join("memory.db");
    // Git notes off, so no repo is needed.
    let cfg = write_config(tmp.path(), &mem_db, false);

    let secret_body = format!("key = AKIA{}", "IOSFODNN7EXAMPLE");

    memory_add_cmd(tmp.path(), &cfg, &mem_db)
        .arg("--title")
        .arg("clean title")
        .arg("--body")
        .arg(&secret_body)
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "refusing to store entry — title or body matches a secret pattern",
        ));

    assert_eq!(
        row_count(&mem_db),
        0_i64,
        "no SQLite row should be written when body contains a secret"
    );
}

#[test]
fn secret_in_title_exits_nonzero_and_writes_no_sqlite_row() {
    let tmp = TempDir::new().unwrap();
    let mem_db = tmp.path().join("memory.db");
    let cfg = write_config(tmp.path(), &mem_db, false);

    let secret_title = format!("DB creds AKIA{} here", "IOSFODNN7EXAMPLE");

    memory_add_cmd(tmp.path(), &cfg, &mem_db)
        .arg("--title")
        .arg(&secret_title)
        .arg("--body")
        .arg("clean body text with no secrets")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "refusing to store entry — title or body matches a secret pattern",
        ));

    assert_eq!(
        row_count(&mem_db),
        0_i64,
        "no SQLite row should be written when title contains a secret"
    );
}

#[test]
fn clean_input_writes_sqlite_row_and_git_note() {
    let tmp = TempDir::new().unwrap();

    // A real repo with an initial commit, which git notes require.
    init_git_repo(tmp.path());

    let mem_db = tmp.path().join("memory.db");
    let cfg = write_config(tmp.path(), &mem_db, true);

    memory_add_cmd(tmp.path(), &cfg, &mem_db)
        .arg("--title")
        .arg("Clean entry — no secrets here")
        .arg("--body")
        .arg("This is a safe body with no credentials at all.")
        .assert()
        .success()
        .stdout(predicate::str::contains("Stored [note]"));

    assert_eq!(
        row_count(&mem_db),
        1_i64,
        "SQLite row should be written for clean input"
    );

    let notes_out = std::process::Command::new("git")
        .args(["notes", "--ref=inkentry", "list"])
        .current_dir(tmp.path())
        .output()
        .expect("git notes list");
    let notes_list = String::from_utf8_lossy(&notes_out.stdout);
    assert!(
        !notes_list.trim().is_empty(),
        "expected at least one inkentry git note after clean memory add"
    );
}

#[test]
fn clean_input_with_git_notes_disabled_writes_only_sqlite() {
    let tmp = TempDir::new().unwrap();
    let mem_db = tmp.path().join("memory.db");
    let cfg = write_config(tmp.path(), &mem_db, false);

    memory_add_cmd(tmp.path(), &cfg, &mem_db)
        .arg("--title")
        .arg("Note with git-notes disabled")
        .arg("--body")
        .arg("Body with no credentials whatsoever.")
        .assert()
        .success()
        .stdout(predicate::str::contains("Stored [note]"));

    assert_eq!(
        row_count(&mem_db),
        1_i64,
        "SQLite row should be written even when store_in_git_notes = false"
    );
}
