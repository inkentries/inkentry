// ADR-099: `memory add` records a pending anchor, and the post-commit hook's
// `memory anchor --commit HEAD` claims it per D2's rule — same worktree, and
// `head_at_write` is the claimed commit's first parent or an ancestor of it,
// or the commit an amend replaced. Never by recency, branch, or session.

use crate::plumbing_helpers;
use plumbing_helpers::{inkentry_bin_in, register_sqlite_vec};

use assert_cmd::Command;
use std::path::Path;
use tempfile::TempDir;

pub use inkentry_core::test_support::isolate_git_config;

fn bin(home: &Path, cwd: &Path) -> Command {
    let mut cmd = inkentry_bin_in(home);
    cmd.current_dir(cwd)
        .env("INKENTRY_NO_SERVER", "1")
        .env_remove("INKENTRY_SERVER_URL");
    cmd
}

fn git_out(dir: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git")
}

fn git(dir: &Path, args: &[&str]) {
    let out = git_out(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn init_repo(dir: &Path) {
    isolate_git_config();
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    std::fs::write(dir.join("README.md"), "hello\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "initial commit"]);
}

fn head_sha(dir: &Path) -> String {
    let out = git_out(dir, &["rev-parse", "HEAD"]);
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn add_note(home: &Path, cwd: &Path, db: &Path, title: &str) -> String {
    bin(home, cwd)
        .arg("memory")
        .arg("--db")
        .arg(db)
        .arg("add")
        .arg("--kind")
        .arg("note")
        .arg("--title")
        .arg(title)
        .arg("--body")
        .arg("body")
        .assert()
        .success();
    entity_id_for(db, title)
}

fn add_note_with_commit(home: &Path, cwd: &Path, db: &Path, title: &str, commit: &str) {
    bin(home, cwd)
        .arg("memory")
        .arg("--db")
        .arg(db)
        .arg("add")
        .arg("--kind")
        .arg("note")
        .arg("--title")
        .arg(title)
        .arg("--body")
        .arg("body")
        .arg("--commit")
        .arg(commit)
        .assert()
        .success();
}

fn anchor(home: &Path, cwd: &Path, db: &Path, commit: &str) {
    bin(home, cwd)
        .arg("memory")
        .arg("--db")
        .arg(db)
        .arg("anchor")
        .arg("--commit")
        .arg(commit)
        .assert()
        .success()
        .stdout("")
        .stderr("");
}

fn conn(db: &Path) -> rusqlite::Connection {
    register_sqlite_vec();
    rusqlite::Connection::open(db).expect("open memory.db")
}

fn entity_id_for(db: &Path, title: &str) -> String {
    conn(db)
        .query_row(
            "SELECT entity_id FROM notes WHERE title = ?1",
            [title],
            |r| r.get(0),
        )
        .unwrap_or_else(|e| panic!("no note titled {title:?}: {e}"))
}

fn pending_count(db: &Path) -> i64 {
    conn(db)
        .query_row("SELECT COUNT(*) FROM pending_anchors", [], |r| r.get(0))
        .unwrap()
}

fn source_ref_of(db: &Path, entity_id: &str) -> Option<String> {
    conn(db)
        .query_row(
            "SELECT source_ref FROM notes WHERE entity_id = ?1",
            [entity_id],
            |r| r.get(0),
        )
        .unwrap()
}

// ── D1: pending anchors are recorded, and only inside a git repo ───────────

#[test]
fn a_write_with_no_commit_stays_pending() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let db = repo.join("memory.db");

    add_note(home.path(), &repo, &db, "never committed");

    assert_eq!(
        pending_count(&db),
        1,
        "an entry with no claiming commit stays pending"
    );
}

#[test]
fn non_git_project_records_no_pending_row_and_does_not_error() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let dir = tmp.path().join("not-a-repo");
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("memory.db");

    bin(home.path(), &dir)
        .arg("memory")
        .arg("--db")
        .arg(&db)
        .arg("add")
        .arg("--kind")
        .arg("note")
        .arg("--title")
        .arg("outside any repo")
        .arg("--body")
        .arg("body")
        .assert()
        .success();

    assert_eq!(
        pending_count(&db),
        0,
        "a project that is not a git repository gets no pending anchor at all"
    );
}

// ── D2: ancestry claims, never recency, branch, or session ─────────────────

#[test]
fn ancestry_claims_a_pending_entry_once_a_commit_grows_out_of_it() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let db = repo.join("memory.db");

    let entity_id = add_note(home.path(), &repo, &db, "decision written before commit");
    assert_eq!(pending_count(&db), 1);

    std::fs::write(repo.join("f.txt"), "x").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "the commit that carries it"]);
    let sha = head_sha(&repo);

    anchor(home.path(), &repo, &db, "HEAD");

    assert_eq!(pending_count(&db), 0, "the row must be claimed");
    assert_eq!(source_ref_of(&db, &entity_id), Some(sha));
}

#[test]
fn ancestry_claims_across_a_branch_created_at_commit_time() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let db = repo.join("memory.db");

    // Written "on main", committed "on feature" — the entry belongs to that
    // commit, and ancestry must see it (ADR-099 D2 rationale).
    let entity_id = add_note(
        home.path(),
        &repo,
        &db,
        "written on main, committed on feature",
    );
    git(&repo, &["switch", "-q", "-c", "feature"]);
    std::fs::write(repo.join("f.txt"), "x").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "on feature"]);

    anchor(home.path(), &repo, &db, "HEAD");

    assert_eq!(
        pending_count(&db),
        0,
        "the entry must be claimed via ancestry"
    );
    assert!(source_ref_of(&db, &entity_id).is_some());
}

#[test]
fn switching_to_an_unrelated_branch_does_not_claim_the_pending_entry() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let db = repo.join("memory.db");

    add_note(home.path(), &repo, &db, "entry on main");

    // An unrelated branch that does NOT build on main's tip.
    git(&repo, &["switch", "-q", "--orphan", "unrelated"]);
    std::fs::write(repo.join("g.txt"), "y").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "unrelated history"]);

    anchor(home.path(), &repo, &db, "HEAD");

    assert_eq!(
        pending_count(&db),
        1,
        "a commit on an unrelated branch must not claim the entry"
    );
}

#[test]
fn amend_is_claimed_via_the_commit_it_replaced() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let db = repo.join("memory.db");

    std::fs::write(repo.join("f.txt"), "x").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "first"]);

    let entity_id = add_note(home.path(), &repo, &db, "written just before the amend");

    // Amend replaces HEAD with a new sha whose parent is unchanged, so
    // ordinary ancestry (against the new commit's first parent) does not see
    // the pre-amend commit; `HEAD@{1}` names it instead.
    std::fs::write(repo.join("f.txt"), "x2").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "--amend", "-m", "first (amended)"]);

    anchor(home.path(), &repo, &db, "HEAD");

    assert_eq!(pending_count(&db), 0, "the amended commit must claim it");
    assert!(source_ref_of(&db, &entity_id).is_some());
}

// ── D2 condition 1: same worktree only ──────────────────────────────────────

#[test]
fn a_commit_in_one_linked_worktree_claims_only_that_worktrees_pending_entries() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let main_repo = tmp.path().join("main");
    init_repo(&main_repo);

    let linked = tmp.path().join("linked");
    git(
        &main_repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "linked-branch",
            linked.to_str().unwrap(),
        ],
    );

    // One shared memory.db, outside either worktree, exactly as linked
    // worktrees share one `.inkentry/memory.db` in real use.
    let db = tmp.path().join("shared").join("memory.db");

    let main_entity = add_note(home.path(), &main_repo, &db, "written in the main worktree");
    let linked_entity = add_note(home.path(), &linked, &db, "written in the linked worktree");
    assert_eq!(pending_count(&db), 2);

    // Commit only in the main worktree.
    std::fs::write(main_repo.join("f.txt"), "x").unwrap();
    git(&main_repo, &["add", "."]);
    git(&main_repo, &["commit", "-q", "-m", "main worktree commit"]);
    anchor(home.path(), &main_repo, &db, "HEAD");

    assert_eq!(
        pending_count(&db),
        1,
        "only the main worktree's entry must be claimed"
    );
    assert!(source_ref_of(&db, &main_entity).is_some());
    assert_eq!(source_ref_of(&db, &linked_entity), None);
}

// ── D4: the explicit escape hatches ─────────────────────────────────────────

#[test]
fn memory_add_with_commit_anchors_immediately_and_skips_the_pending_row() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let db = repo.join("memory.db");
    let sha = head_sha(&repo);

    add_note_with_commit(home.path(), &repo, &db, "anchored on write", &sha);

    assert_eq!(
        pending_count(&db),
        0,
        "--commit anchors immediately instead of recording a pending row"
    );
    let entity_id = entity_id_for(&db, "anchored on write");
    assert_eq!(source_ref_of(&db, &entity_id), Some(sha));
}

#[test]
fn memory_anchor_with_explicit_ids_anchors_by_hand() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let db = repo.join("memory.db");

    let entity_id = add_note(home.path(), &repo, &db, "anchored by hand");
    let sha = head_sha(&repo);

    bin(home.path(), &repo)
        .arg("memory")
        .arg("--db")
        .arg(&db)
        .arg("anchor")
        .arg("--commit")
        .arg(&sha)
        .arg(&entity_id)
        .assert()
        .success()
        .stdout("")
        .stderr("");

    assert_eq!(pending_count(&db), 0);
    assert_eq!(source_ref_of(&db, &entity_id), Some(sha));
}

// ── Hook plumbing: never fails, never prints ────────────────────────────────

#[test]
fn memory_anchor_exits_zero_and_silent_even_with_no_store_at_all() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let db = repo.join("does-not-exist").join("memory.db");

    bin(home.path(), &repo)
        .arg("memory")
        .arg("--db")
        .arg(&db)
        .arg("anchor")
        .arg("--commit")
        .arg("HEAD")
        .assert()
        .success()
        .stdout("")
        .stderr("");
}
