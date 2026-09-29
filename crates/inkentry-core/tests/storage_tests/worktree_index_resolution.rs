// The `config.rs` fixtures build a `.git`-file by hand, which `gix::discover`
// fails on, exercising only the manual fallback branch of
// `resolve_main_worktree_root`. This file builds a real `git worktree`, so
// `gix::discover` succeeds and the primary gix path is covered end to end,
// including the real-path shape it returns (e.g. macOS's `/var` ->
// `/private/var` symlink). Every git spawn goes through `common::git_command`,
// which shadows ambient git config, so a developer's or CI's global
// `core.hooksPath`/`commit.gpgsign` can't reach it.

use crate::common;
use inkentry_core::config::find_project_db;
use inkentry_core::utils::resolve_main_worktree_root;

fn git(cwd: &std::path::Path, args: &[&str]) -> std::process::Output {
    let out = common::git_command(cwd)
        .args(args)
        .output()
        .expect("git command");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

// Canonicalise both paths first: a symlinked temp prefix (macOS `/var` vs
// `/private/var`) must not spuriously fail the comparison.
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

    // wt_root/.git becomes a gitdir file pointing at
    // <main>/.git/worktrees/feat-branch — the layout gix::discover resolves.
    git(
        &main_root,
        &["worktree", "add", wt_root.to_str().unwrap(), "-b", "feat"],
    );
    assert!(
        wt_root.join(".git").is_file(),
        "worktree .git must be a file"
    );
    assert!(
        !wt_root.join(".inkentry").exists(),
        "worktree must have no local .inkentry/"
    );

    // The shared index lives only in the main worktree.
    std::fs::create_dir_all(main_root.join(".inkentry")).unwrap();
    let index_db = main_root.join(".inkentry").join("index.db");
    std::fs::write(&index_db, b"").unwrap();

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

// Confirms hermeticity is real, not aspirational: re-execs as a fresh child
// process with a hostile ambient GIT_CONFIG_GLOBAL (its core.hooksPath hooks
// always fail), and the wrapped test still passes because
// `isolate_git_config` overwrites it before the child's first git spawn. A
// fresh process is required because `isolate_git_config` is a one-shot
// `Once`: an already-running test can't simulate an ambient value the
// isolation never saw.
#[test]
fn real_git_worktree_resolves_to_main_index_survives_a_hostile_ambient_hooks_path() {
    let hostile = tempfile::TempDir::new().expect("tempdir");
    let hooks_dir = hostile.path().join("hooks");
    std::fs::create_dir_all(&hooks_dir).unwrap();
    for hook in ["pre-commit", "post-checkout", "post-commit"] {
        let path = hooks_dir.join(hook);
        std::fs::write(&path, "#!/bin/sh\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    let global_config = hostile.path().join("gitconfig");
    std::fs::write(
        &global_config,
        format!("[core]\n\thooksPath = {}\n", hooks_dir.display()),
    )
    .unwrap();

    let exe = std::env::current_exe().expect("current test binary");
    let status = std::process::Command::new(exe)
        .arg("worktree_index_resolution::real_git_worktree_resolves_to_main_index")
        .arg("--exact")
        .env("GIT_CONFIG_GLOBAL", &global_config)
        .status()
        .expect("run self as a child process");
    assert!(
        status.success(),
        "the child test failed under a hostile ambient GIT_CONFIG_GLOBAL: isolation did not shadow it"
    );
}

// Git resolves identity as env-vars-before-config, so a local user.email
// alone proves nothing about isolation — the real assertion is that ambient
// GIT_AUTHOR_*/GIT_COMMITTER_* did not win. Runs standalone, and is also
// re-exec'd by the driver test below under a poisoned ambient environment.
#[test]
fn git_command_commit_identity_matches_local_config_not_ambient_env() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-b", "main"]);
    git(root, &["config", "user.email", "test@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("f.txt"), "hi").unwrap();
    git(root, &["add", "."]);
    git(
        root,
        &[
            "commit",
            "--no-gpg-sign",
            "-m",
            "init",
            "--allow-empty-message",
        ],
    );

    let out = git(root, &["log", "-1", "--format=%an <%ae>%n%cn <%ce>"]);
    let identity = String::from_utf8_lossy(&out.stdout);
    let mut lines = identity.lines();
    let author = lines.next().unwrap_or_default();
    let committer = lines.next().unwrap_or_default();
    assert_eq!(
        author, "Test <test@example.com>",
        "commit author must come from the repo's own config, not an ambient GIT_AUTHOR_* override"
    );
    assert_eq!(
        committer, "Test <test@example.com>",
        "commit committer must come from the repo's own config, not an ambient GIT_COMMITTER_* override"
    );
}

// Re-execs the test above as a fresh child process with
// GIT_AUTHOR_*/GIT_COMMITTER_* poisoned in the ambient environment (as a CI
// bot identity or shell profile might export them), confirming
// `isolate_git_config` clears them before the child's first git spawn rather
// than only redirecting config files. Needs a fresh process for the same
// `Once`-per-process reason as the hooks-path driver above.
#[test]
fn git_command_commit_identity_survives_a_hostile_ambient_author_committer_env() {
    let exe = std::env::current_exe().expect("current test binary");
    let status = std::process::Command::new(exe)
        .arg("worktree_index_resolution::git_command_commit_identity_matches_local_config_not_ambient_env")
        .arg("--exact")
        .env("GIT_AUTHOR_NAME", "Ambient Poison")
        .env("GIT_AUTHOR_EMAIL", "poison@evil.example")
        .env("GIT_COMMITTER_NAME", "Ambient Poison")
        .env("GIT_COMMITTER_EMAIL", "poison@evil.example")
        .status()
        .expect("run self as a child process");
    assert!(
        status.success(),
        "the child test failed under a hostile ambient GIT_AUTHOR_*/GIT_COMMITTER_* env: isolation did not shadow it"
    );
}
