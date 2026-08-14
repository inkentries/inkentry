//! Fail-closed behaviour when there is no local `.inkentry/` project (ADR-067).
//!
//! In a directory that was never `inkentry init`'d, memory/context/index-backed
//! search must refuse rather than silently read or write the machine-global
//! `~/.config/inkentry/` store. `--db` and `inkentry index` stay exempt. `status`
//! reports "no project" instead of describing the global store.

use crate::plumbing_helpers;
use plumbing_helpers::inkentry_bin_in;

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::Path;
use tempfile::TempDir;

/// Exact fail-closed error text (ADR-067; em dash restructured out per the
/// no-em-dash house rule for user-facing copy).
const NO_PROJECT_ERR: &str = "no inkentry project here. Run 'inkentry init' first";

/// ADR-068 D3 dual-escape-hatch error for `memory add`/`list` when there is
/// neither a project DB nor a usable git repo (case 5).
const NO_PROJECT_NO_REPO_ERR: &str = "no inkentry project here, and not inside a git repo. Run 'inkentry init' first, \
     or run inside a git repository.";

/// A `inkentry` command with an isolated HOME (so the "global" store lives under
/// `<home>/.config/inkentry`) and no server contact, run in `cwd`.
fn bin(home: &Path, cwd: &Path) -> Command {
    let mut cmd = inkentry_bin_in(home);
    cmd.current_dir(cwd)
        .env("INKENTRY_NO_SERVER", "1")
        .env_remove("INKENTRY_SERVER_URL");
    cmd
}

/// The global memory store path under the isolated HOME. Must never be created
/// by a fail-closed command.
fn global_memory_db(home: &Path) -> std::path::PathBuf {
    home.join(".config").join("inkentry").join("memory.db")
}

/// The global index store path under the isolated HOME. Display commands must
/// never read or create it from an un-init'd dir (ADR-067).
fn global_index_db(home: &Path) -> std::path::PathBuf {
    home.join(".config").join("inkentry").join("index.db")
}

// ── refuse-guard: un-init'd dir, no --db ───────────────────────────────────────

// A bare `TempDir` is not inside a git repo (it lives under the system temp
// dir, not a checkout), so `memory add`/`list` hit ADR-068 D3 case 5 (neither
// a project DB nor a usable git repo) and refuse with the dual-hatch message
// rather than falling back to git-notes.
#[test]
fn memory_add_refuses_without_project_or_git_repo() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();

    bin(home.path(), proj.path())
        .args([
            "memory", "add", "--kind", "note", "--title", "t", "--body", "b",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(NO_PROJECT_NO_REPO_ERR));

    assert!(
        !global_memory_db(home.path()).exists(),
        "refused memory add must not create the global store"
    );
}

#[test]
fn memory_list_refuses_without_project_or_git_repo() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();

    bin(home.path(), proj.path())
        .args(["memory", "list"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(NO_PROJECT_NO_REPO_ERR));

    assert!(!global_memory_db(home.path()).exists());
}

#[test]
fn search_only_memory_refuses_without_local_project() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();

    // Unified search resolves the index project before touching any corpus, so a
    // memory-only search still fails closed rather than reading the global store.
    bin(home.path(), proj.path())
        .args(["search", "anything", "--only-memory"])
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

    // Index-backed search must refuse rather than fall back to global.
    bin(home.path(), proj.path())
        .args(["search", "anything", "--only-text"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(NO_PROJECT_ERR));
}

// ── exempt: a real local project, an explicit --db, and `inkentry index` ────────

#[test]
fn memory_add_works_with_local_dot_inkentry() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    std::fs::create_dir_all(proj.path().join(".inkentry")).unwrap();

    bin(home.path(), proj.path())
        .args([
            "memory", "add", "--kind", "note", "--title", "t", "--body", "b",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Stored [note]"));

    assert!(
        proj.path().join(".inkentry").join("memory.db").exists(),
        "memory add must write into the local project's .inkentry/memory.db"
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
        "--db override must be honored even with no .inkentry/"
    );
    assert!(!global_memory_db(home.path()).exists());
}

#[test]
fn index_creates_project_in_uninit_dir() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    std::fs::write(proj.path().join("main.rs"), "fn main() {}\n").unwrap();

    // `inkentry index <path>` is the project-creation command; never gated.
    bin(home.path(), proj.path())
        .args(["index", "."])
        .assert()
        .success();

    assert!(
        proj.path().join(".inkentry").join("index.db").exists(),
        "index must create the local .inkentry/index.db"
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
        .stdout(predicate::str::contains("No inkentry project here"));
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

// ── gap: the top-level `Sync` arm (main.rs), distinct from memory-dispatch ─────

#[test]
fn sync_arm_refuses_without_local_project() {
    // `inkentry sync` is a top-level command whose guard lives in `main.rs`, not in
    // the `memory` dispatch. With no `server_url` configured, `validate_with_project`
    // passes, so the fail-closed guard is what must fire (not a config error).
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();

    bin(home.path(), proj.path())
        .args(["sync"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(NO_PROJECT_ERR));

    assert!(
        !global_memory_db(home.path()).exists(),
        "refused sync must not create the global store"
    );
}

// ── gap: a memory subcommand past add/list/search proves the shared funnel ─────

#[test]
fn memory_timeline_refuses_without_local_project() {
    // Every `memory` subcommand *except* the ADR-068 D3 add/list fallback
    // resolves its store through the same `require_project_db` line before
    // dispatch; `timeline` (needs no server) confirms the fail-closed guard
    // still holds for the non-add/list subcommands.
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();

    bin(home.path(), proj.path())
        .args(["memory", "timeline", "anything"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(NO_PROJECT_ERR));

    assert!(!global_memory_db(home.path()).exists());
}

// ── security invariant: a refused command must not MUTATE a pre-existing global ──
//
// The other refuse tests assert the global store is not *created*. These assert
// the other half of ADR-067's "not created or mutated": a global store left over
// from the pre-fix silent-fallback era is left byte-for-byte untouched.

#[test]
fn refused_memory_add_does_not_mutate_preexisting_global_store() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();

    let global = global_memory_db(home.path());
    std::fs::create_dir_all(global.parent().unwrap()).unwrap();
    let sentinel = b"pre-existing global memory store sentinel";
    std::fs::write(&global, sentinel).unwrap();

    bin(home.path(), proj.path())
        .args([
            "memory", "add", "--kind", "note", "--title", "t", "--body", "b",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(NO_PROJECT_NO_REPO_ERR));

    assert_eq!(
        std::fs::read(&global).unwrap(),
        sentinel,
        "refused memory add must not open or mutate the pre-existing global store"
    );
}

#[test]
fn refused_index_search_does_not_touch_preexisting_global_index() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();

    let global_index = home
        .path()
        .join(".config")
        .join("inkentry")
        .join("index.db");
    std::fs::create_dir_all(global_index.parent().unwrap()).unwrap();
    let sentinel = b"pre-existing global index sentinel";
    std::fs::write(&global_index, sentinel).unwrap();

    // Index-backed search fails closed before opening any DB, so even a stray
    // global index.db is never read or written.
    bin(home.path(), proj.path())
        .args(["search", "anything", "--only-text"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(NO_PROJECT_ERR));

    assert_eq!(
        std::fs::read(&global_index).unwrap(),
        sentinel,
        "refused index-backed search must not touch the pre-existing global index"
    );
}

// ── display commands: chunks ─────────────────────────
//
// This read-only command previously resolved its DB via the legacy
// `open_project_db`/`resolve_db` path, which fell back to the machine-global
// `index.db` in an un-init'd dir and displayed cross-project data. It now shares
// ADR-067's fail-closed resolver: refuse instead of reading global.

#[test]
fn chunks_refuses_without_local_project() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();

    let global = global_index_db(home.path());
    std::fs::create_dir_all(global.parent().unwrap()).unwrap();
    let sentinel = b"pre-existing global index sentinel";
    std::fs::write(&global, sentinel).unwrap();

    bin(home.path(), proj.path())
        .args(["chunks", "src/lib.rs"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(NO_PROJECT_ERR));

    assert_eq!(
        std::fs::read(&global).unwrap(),
        sentinel,
        "refused chunks must not open or mutate the pre-existing global index"
    );
}

// ── happy path: an init'd project still resolves graph-edges/chunks locally ────
//
// The fail-closed rework must not break the normal case: with a real local
// `.inkentry/index.db`, the index-backed commands resolve LOCAL (not global) and
// work. A stray global index is left in place to prove they read local.

#[test]
fn display_commands_resolve_local_index_in_initd_project() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    std::fs::write(
        proj.path().join("lib.rs"),
        "pub fn local_target() -> u32 { 7 }\n\
         fn local_caller() { let _ = local_target(); }\n",
    )
    .unwrap();

    // A stray machine-global index that must never be consulted by the local
    // happy path (garbage bytes: if any command opened it as SQLite it would err).
    let global = global_index_db(home.path());
    std::fs::create_dir_all(global.parent().unwrap()).unwrap();
    let sentinel = b"stray global index sentinel";
    std::fs::write(&global, sentinel).unwrap();

    // Create the local project.
    bin(home.path(), proj.path())
        .args(["index", "."])
        .assert()
        .success();
    assert!(proj.path().join(".inkentry").join("index.db").exists());

    // graph-edges: index-backed symbol query resolves the LOCAL index (the graph
    // capability's machine surface after the top-level `graph` porcelain was removed).
    bin(home.path(), proj.path())
        .args(["plumbing", "graph-edges", "--symbol", "local_target"])
        .assert()
        .success()
        .stdout(predicate::str::contains("lib.rs"))
        .stdout(predicate::str::contains("local_target"));

    // chunks: resolves the LOCAL index and returns this file's chunks.
    bin(home.path(), proj.path())
        .args(["chunks", "lib.rs", "--format", "json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("local_target"));

    assert_eq!(
        std::fs::read(&global).unwrap(),
        sentinel,
        "init'd-project display commands must resolve local and never touch the global store"
    );
}

// ── walk-up: memory resolves the ancestor project from a deep subdir ───────────

#[test]
fn memory_add_works_from_deep_nested_subdir() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    std::fs::create_dir_all(proj.path().join(".inkentry")).unwrap();
    let deep = proj.path().join("a").join("b").join("c");
    std::fs::create_dir_all(&deep).unwrap();

    // Run several levels below the `.inkentry/` project root; the guard walks up
    // and resolves the ancestor's store, not the global one.
    bin(home.path(), &deep)
        .args([
            "memory", "add", "--kind", "note", "--title", "t", "--body", "b",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Stored [note]"));

    assert!(
        proj.path().join(".inkentry").join("memory.db").exists(),
        "note must land in the ancestor project's .inkentry/memory.db"
    );
    assert!(!global_memory_db(home.path()).exists());
}

// ── worktree-awareness: a linked worktree resolves to the main worktree's store ──

/// Run `git args` in `dir`, asserting success. Isolated identity so it works on a
/// machine with no global git config.
fn git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git");
    assert!(status.status.success(), "git {args:?} failed");
}

#[test]
fn memory_resolves_main_worktree_dot_inkentry_from_linked_worktree() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let main_root = tmp.path().join("main");
    std::fs::create_dir_all(&main_root).unwrap();

    git(&main_root, &["init", "-q", "-b", "main"]);
    std::fs::write(main_root.join("f.txt"), "x\n").unwrap();
    git(&main_root, &["add", "."]);
    git(&main_root, &["commit", "-q", "-m", "init"]);

    // Only the main worktree is a real project (has `.inkentry/`).
    std::fs::create_dir_all(main_root.join(".inkentry")).unwrap();

    // Add a linked worktree with no `.inkentry/` of its own.
    let linked = tmp.path().join("linked");
    git(
        &main_root,
        &[
            "worktree",
            "add",
            "-q",
            linked.to_str().unwrap(),
            "-b",
            "feat",
        ],
    );
    assert!(
        !linked.join(".inkentry").exists(),
        "precondition: linked worktree has no .inkentry/"
    );

    // ADR-067 worktree-awareness: memory run from the linked worktree must resolve
    // to the MAIN worktree's `.inkentry/` store, not fail closed and not go global.
    bin(home.path(), &linked)
        .args([
            "memory", "add", "--kind", "note", "--title", "t", "--body", "b",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Stored [note]"));

    assert!(
        main_root.join(".inkentry").join("memory.db").exists(),
        "note must land in the main worktree's .inkentry/memory.db"
    );
    assert!(
        !linked.join(".inkentry").exists(),
        "the linked worktree must not get its own .inkentry/"
    );
    assert!(!global_memory_db(home.path()).exists());
}
