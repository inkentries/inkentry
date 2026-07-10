//! Fail-closed behaviour when there is no local `.spelunk/` project (ADR-067,
//! spelunk-oss^131).
//!
//! In a directory that was never `spelunk init`'d, memory/context/index-backed
//! search must refuse rather than silently read or write the machine-global
//! `~/.config/spelunk/` store. `--db` and `spelunk index` stay exempt. `status`
//! reports "no project" instead of describing the global store.

mod plumbing_helpers;
use plumbing_helpers::spelunk_bin_in;

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::Path;
use tempfile::TempDir;

/// Exact fail-closed error text mandated by ADR-067 (em dash, single quotes).
const NO_PROJECT_ERR: &str = "no spelunk project here \u{2014} run 'spelunk init' first";

/// A `spelunk` command with an isolated HOME (so the "global" store lives under
/// `<home>/.config/spelunk`) and no server contact, run in `cwd`.
fn bin(home: &Path, cwd: &Path) -> Command {
    let mut cmd = spelunk_bin_in(home);
    cmd.current_dir(cwd)
        .env("SPELUNK_NO_SERVER", "1")
        .env_remove("SPELUNK_SERVER_URL")
        .env_remove("SPELUNK_MEMORY_SERVER_URL");
    cmd
}

/// The global memory store path under the isolated HOME. Must never be created
/// by a fail-closed command.
fn global_memory_db(home: &Path) -> std::path::PathBuf {
    home.join(".config").join("spelunk").join("memory.db")
}

// ── refuse-guard: un-init'd dir, no --db ───────────────────────────────────────

#[test]
fn memory_add_refuses_without_local_project() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();

    bin(home.path(), proj.path())
        .args([
            "memory", "add", "--kind", "note", "--title", "t", "--body", "b",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(NO_PROJECT_ERR));

    assert!(
        !global_memory_db(home.path()).exists(),
        "refused memory add must not create the global store"
    );
}

#[test]
fn memory_list_refuses_without_local_project() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();

    bin(home.path(), proj.path())
        .args(["memory", "list"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(NO_PROJECT_ERR));

    assert!(!global_memory_db(home.path()).exists());
}

#[test]
fn memory_search_refuses_without_local_project() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();

    bin(home.path(), proj.path())
        .args(["memory", "search", "anything"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(NO_PROJECT_ERR));

    assert!(!global_memory_db(home.path()).exists());
}

#[test]
fn context_refuses_without_local_project() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();

    bin(home.path(), proj.path())
        .args(["context"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(NO_PROJECT_ERR));

    assert!(!global_memory_db(home.path()).exists());
}

#[test]
fn index_backed_search_refuses_without_local_project() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();

    // Explicit index-backed mode: must refuse rather than fall back to global.
    bin(home.path(), proj.path())
        .args(["search", "anything", "--mode", "text"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(NO_PROJECT_ERR));
}

// ── exempt: a real local project, an explicit --db, and `spelunk index` ────────

#[test]
fn memory_add_works_with_local_dot_spelunk() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    std::fs::create_dir_all(proj.path().join(".spelunk")).unwrap();

    bin(home.path(), proj.path())
        .args([
            "memory", "add", "--kind", "note", "--title", "t", "--body", "b",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Stored [note]"));

    assert!(
        proj.path().join(".spelunk").join("memory.db").exists(),
        "memory add must write into the local project's .spelunk/memory.db"
    );
    assert!(
        !global_memory_db(home.path()).exists(),
        "the global store must stay untouched"
    );
}

#[test]
fn memory_add_works_with_explicit_db_in_uninit_dir() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    let db = proj.path().join("custom.db");

    bin(home.path(), proj.path())
        .args(["memory", "--db"])
        .arg(&db)
        .args(["add", "--kind", "note", "--title", "t", "--body", "b"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Stored [note]"));

    assert!(
        db.exists(),
        "--db override must be honored even with no .spelunk/"
    );
    assert!(!global_memory_db(home.path()).exists());
}

#[test]
fn index_creates_project_in_uninit_dir() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    std::fs::write(proj.path().join("main.rs"), "fn main() {}\n").unwrap();

    // `spelunk index <path>` is the project-creation command; never gated.
    bin(home.path(), proj.path())
        .args(["index", "."])
        .assert()
        .success();

    assert!(
        proj.path().join(".spelunk").join("index.db").exists(),
        "index must create the local .spelunk/index.db"
    );
}

// ── status: report no-project, and label the resolved backend ──────────────────

#[test]
fn status_text_reports_no_project_when_uninit() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();

    bin(home.path(), proj.path())
        .args(["status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No spelunk project here"));
}

#[test]
fn status_json_fails_closed_when_uninit() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();

    bin(home.path(), proj.path())
        .args(["status", "--format", "json"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(NO_PROJECT_ERR));
}

#[test]
fn status_labels_resolved_backend_as_sqlite_not_git_notes() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    std::fs::write(proj.path().join("main.rs"), "fn main() {}\n").unwrap();

    bin(home.path(), proj.path())
        .args(["index", "."])
        .assert()
        .success();

    // The memory line must reflect the resolved backend (sqlite by default), not
    // a tier-derived "git-notes" label (ADR-067 D3).
    bin(home.path(), proj.path())
        .args(["status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("sqlite (local)"))
        .stdout(predicate::str::contains("git-notes").not());
}
