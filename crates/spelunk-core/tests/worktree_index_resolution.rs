//! Integration test for worktree → main-index resolution over a REAL
//! `git worktree` (spelunk-oss^138).
//!
//! The `config.rs` unit fixtures build a `.git`-*file* by hand, on which
//! `gix::discover` fails — so they exercise only the manual fallback branch of
//! `resolve_main_worktree_root`. This test creates a genuine linked worktree via
//! `git worktree add`, so `gix::discover` succeeds and the primary gix branch is
//! covered end to end. It also guards the real-path shape gix returns (e.g. the
//! macOS `/var` → `/private/var` symlink), which string-only fixtures cannot.
//!
//! Requires `git` on PATH; fully hermetic (everything lives under a TempDir that
//! is removed on drop, taking the linked worktree with it — no orphans).

use spelunk_core::config::find_project_db;
use spelunk_core::utils::resolve_main_worktree_root;

fn git(cwd: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git command")
}

/// Compare two paths after canonicalising both, so a symlinked temp prefix
/// (macOS `/var` vs `/private/var`) does not spuriously fail the assertion.
fn same_path(a: &std::path::Path, b: &std::path::Path) {
    let ca = std::fs::canonicalize(a).unwrap();
    let cb = std::fs::canonicalize(b).unwrap();
    assert_eq!(ca, cb, "{} != {}", a.display(), b.display());
}

#[test]
fn real_git_worktree_resolves_to_main_index() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let main_root = tmp.path().join("main");
    let wt_root = tmp.path().join("feat-branch");
    std::fs::create_dir_all(&main_root).unwrap();

    // Real main repo with one commit so a worktree can be added.
    git(&main_root, &["init", "-b", "main"]);
    git(&main_root, &["config", "user.email", "test@example.com"]);
    git(&main_root, &["config", "user.name", "Test"]);
    std::fs::write(main_root.join("README.md"), "test").unwrap();
    git(&main_root, &["add", "."]);
    git(
        &main_root,
        &[
            "commit",
            "--no-gpg-sign",
            "-m",
            "init",
            "--allow-empty-message",
        ],
    );

    // Add a REAL linked worktree; wt_root/.git becomes a gitdir file pointing at
    // <main>/.git/worktrees/feat-branch — the layout gix::discover resolves.
    let out = git(
        &main_root,
        &["worktree", "add", wt_root.to_str().unwrap(), "-b", "feat"],
    );
    assert!(
        out.status.success(),
        "git worktree add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wt_root.join(".git").is_file(),
        "worktree .git must be a file"
    );
    assert!(
        !wt_root.join(".spelunk").exists(),
        "worktree must have no local .spelunk/"
    );

    // The shared index lives only in the main worktree.
    std::fs::create_dir_all(main_root.join(".spelunk")).unwrap();
    let index_db = main_root.join(".spelunk").join("index.db");
    std::fs::write(&index_db, b"").unwrap();

    // Primary gix branch: discovery from inside the linked worktree resolves to
    // the main worktree root, and a read from the worktree finds the main index.
    same_path(&resolve_main_worktree_root(&wt_root), &main_root);
    same_path(
        &find_project_db(&wt_root).expect("worktree resolves to main index"),
        &index_db,
    );

    // A subdirectory inside the worktree resolves the same way (gix walks up).
    let sub = wt_root.join("nested").join("dir");
    std::fs::create_dir_all(&sub).unwrap();
    same_path(&resolve_main_worktree_root(&sub), &main_root);
}
