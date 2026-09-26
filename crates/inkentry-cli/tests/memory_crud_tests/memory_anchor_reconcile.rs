// ADR-099 D3a: hook-free rewrite reconciliation, run by `inkentry index` (the
// pass that already runs after local history moves — see the post-commit
// hook and the "bring the index up to date" step every session starts with).
//
// These tests simulate a rebase with `cherry-pick`: the cheapest way to
// produce a commit with the same diff (so the same `git patch-id --stable`)
// under a different sha, without driving an actual `git rebase` sequence.

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
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("spawn git")
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_out(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_repo(dir: &Path) {
    isolate_git_config();
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    // `.inkentry/` (memory.db, index.db) must never be part of the commits
    // these tests manufacture: a `git reset --hard`/cherry-pick would then
    // rewrite or delete the live stores as a side effect of moving history.
    std::fs::write(dir.join(".gitignore"), ".inkentry/\n").unwrap();
    std::fs::write(dir.join("README.md"), "hello\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "initial commit"]);
}

fn head_sha(dir: &Path) -> String {
    git(dir, &["rev-parse", "HEAD"])
}

fn run_index(home: &Path, repo: &Path) {
    bin(home, repo).arg("index").arg(".").assert().success();
}

fn add_note(home: &Path, repo: &Path, title: &str) -> String {
    bin(home, repo)
        .arg("memory")
        .arg("add")
        .arg("--kind")
        .arg("note")
        .arg("--title")
        .arg(title)
        .arg("--body")
        .arg("body")
        .assert()
        .success();
    conn(repo)
        .query_row(
            "SELECT entity_id FROM notes WHERE title = ?1",
            [title],
            |r| r.get(0),
        )
        .unwrap()
}

fn anchor_head(home: &Path, repo: &Path) {
    bin(home, repo)
        .arg("memory")
        .arg("anchor")
        .arg("--commit")
        .arg("HEAD")
        .assert()
        .success();
}

fn conn(repo: &Path) -> rusqlite::Connection {
    register_sqlite_vec();
    rusqlite::Connection::open(repo.join(".inkentry").join("memory.db")).expect("open memory.db")
}

fn source_ref_of(repo: &Path, entity_id: &str) -> Option<String> {
    conn(repo)
        .query_row(
            "SELECT source_ref FROM notes WHERE entity_id = ?1",
            [entity_id],
            |r| r.get(0),
        )
        .unwrap()
}

fn pending_count(repo: &Path) -> i64 {
    conn(repo)
        .query_row("SELECT COUNT(*) FROM pending_anchors", [], |r| r.get(0))
        .unwrap()
}

fn head_at_write_of(repo: &Path, entity_id: &str) -> Option<String> {
    conn(repo)
        .query_row(
            "SELECT head_at_write FROM pending_anchors WHERE entity_id = ?1",
            [entity_id],
            |r| r.get(0),
        )
        .ok()
}

// ── an anchored commit, rebased ─────────────────────────────────────────────

#[test]
fn a_rebased_anchored_commit_gets_a_second_anchor_and_source_ref_follows() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    run_index(home.path(), &repo);

    let entity_id = add_note(home.path(), &repo, "rebased decision");
    std::fs::write(repo.join("work.txt"), "the actual change\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "the work"]);
    anchor_head(home.path(), &repo);
    let original_sha = head_sha(&repo);
    assert_eq!(source_ref_of(&repo, &entity_id), Some(original_sha.clone()));

    // Rewrite history so the original commit is no longer reachable from any
    // ref, and cherry-pick the same diff onto a new tip (same patch-id, new sha).
    git(&repo, &["reset", "-q", "--hard", "HEAD~1"]);
    std::fs::write(repo.join("unrelated.txt"), "unrelated\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "unrelated commit"]);
    git(&repo, &["cherry-pick", &original_sha]);
    let rebased_sha = head_sha(&repo);
    assert_ne!(original_sha, rebased_sha, "cherry-pick must mint a new sha");

    run_index(home.path(), &repo);

    assert_eq!(
        source_ref_of(&repo, &entity_id),
        Some(rebased_sha),
        "source_ref must follow the rebased commit once the original is unreachable"
    );
}

// ── a conflict-resolved rebase changes the diff: left alone ────────────────

#[test]
fn a_rebase_that_changes_the_diff_is_left_alone() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    run_index(home.path(), &repo);

    let entity_id = add_note(home.path(), &repo, "conflict-resolved decision");
    std::fs::write(repo.join("work.txt"), "the original change\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "the work"]);
    anchor_head(home.path(), &repo);
    let original_sha = head_sha(&repo);

    // Move the original commit out of reach, but land a DIFFERENT diff at the
    // new tip rather than cherry-picking the same one (as if a rebase needed
    // conflict resolution and the result differs).
    git(&repo, &["reset", "-q", "--hard", "HEAD~1"]);
    std::fs::write(repo.join("work.txt"), "a differently resolved change\n").unwrap();
    git(&repo, &["add", "."]);
    git(
        &repo,
        &["commit", "-q", "-m", "the work, resolved differently"],
    );

    run_index(home.path(), &repo);

    assert_eq!(
        source_ref_of(&repo, &entity_id),
        Some(original_sha),
        "no reachable commit shares the original's patch-id, so the entry \
         keeps the anchor it has rather than guessing"
    );
}

// ── two identical diffs: ambiguous, left alone ──────────────────────────────

#[test]
fn two_reachable_commits_with_the_same_patch_id_are_ambiguous_and_left_alone() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    run_index(home.path(), &repo);

    let entity_id = add_note(home.path(), &repo, "ambiguous decision");
    std::fs::write(repo.join("work.txt"), "the same change\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "the work"]);
    anchor_head(home.path(), &repo);
    let original_sha = head_sha(&repo);

    // Move the original out of reach, then land the SAME diff twice more
    // further down `main`'s own line — two distinct, reachable shas sharing
    // one patch-id (`git cherry-pick` refuses to no-op a tree already
    // present, so an unrelated commit separates the two picks).
    git(&repo, &["reset", "-q", "--hard", "HEAD~1"]);
    std::fs::write(repo.join("unrelated.txt"), "u\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "unrelated"]);
    git(&repo, &["cherry-pick", &original_sha]);
    git(&repo, &["rm", "-q", "work.txt"]);
    git(
        &repo,
        &[
            "commit",
            "-q",
            "-m",
            "undo, so the second pick has something to redo",
        ],
    );
    git(&repo, &["cherry-pick", &original_sha]);

    run_index(home.path(), &repo);

    assert_eq!(
        source_ref_of(&repo, &entity_id),
        Some(original_sha),
        "two reachable commits sharing the patch-id are ambiguous; \
         the entry must keep the anchor it has, not guess between them"
    );
}

// ── a pending row's base is rebased, then claimed through the replacement ──

#[test]
fn a_pending_rows_rebased_base_is_claimed_through_the_replacement() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    run_index(home.path(), &repo);

    let entity_id = add_note(home.path(), &repo, "written before a rebase of its base");
    assert_eq!(pending_count(&repo), 1);
    let original_head = head_sha(&repo);
    assert_eq!(
        head_at_write_of(&repo, &entity_id),
        Some(original_head.clone())
    );

    // Amend the commit the entry was written on: same diff, new sha, and the
    // original sha drops out of `main`'s reachable history.
    git(
        &repo,
        &["commit", "-q", "--amend", "-m", "initial commit, amended"],
    );
    let amended_head = head_sha(&repo);
    assert_ne!(original_head, amended_head);

    run_index(home.path(), &repo);

    assert_eq!(
        head_at_write_of(&repo, &entity_id),
        Some(amended_head),
        "the pending row must be repointed at the replacement commit"
    );
    assert_eq!(
        pending_count(&repo),
        1,
        "reconciliation repoints, it does not claim"
    );

    // The next ordinary commit in this worktree now claims it via the
    // ordinary D2 ancestry test, through the replacement.
    std::fs::write(repo.join("next.txt"), "next\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "the next commit"]);
    bin(home.path(), &repo)
        .arg("memory")
        .arg("anchor")
        .arg("--commit")
        .arg("HEAD")
        .assert()
        .success();

    assert_eq!(pending_count(&repo), 0, "the next commit must claim it");
    assert!(source_ref_of(&repo, &entity_id).is_some());
}
