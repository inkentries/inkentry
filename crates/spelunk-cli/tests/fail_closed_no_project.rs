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

/// Exact fail-closed error text (ADR-067; em dash restructured out per the
/// no-em-dash house rule for user-facing copy).
const NO_PROJECT_ERR: &str = "no spelunk project here. Run 'spelunk init' first";

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

// ── exempt: index-free ast-grep search (ADR-067 D1) ────────────────────────────

#[test]
fn ast_grep_search_works_without_local_project() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    std::fs::write(
        proj.path().join("lib.rs"),
        "pub fn greet(name: &str) -> String { format!(\"hi {name}\") }\n\
         fn caller() { greet(\"x\"); }\n",
    )
    .unwrap();

    // `--mode ast-grep` touches no index and no global store, so it must run live
    // over the working tree in an un-init'd dir rather than fail closed. The
    // `greet($$$ARGS)` call pattern matches the call site in `caller`.
    bin(home.path(), proj.path())
        .args([
            "search",
            "greet($$$ARGS)",
            "--mode",
            "ast-grep",
            "--format",
            "json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"file_path\""))
        .stdout(predicate::str::contains("lib.rs"));

    // Index-free search must never create or read the machine-global store.
    assert!(
        !home
            .path()
            .join(".config")
            .join("spelunk")
            .join("index.db")
            .exists(),
        "ast-grep search must not create the global index"
    );
    assert!(!global_memory_db(home.path()).exists());
}

// ── zero-setup plain-string substring search (spelunk-oss^130) ─────────────────

/// The reported bug: an exact identifier matched but a *substring* of it (and
/// case variants) returned "No results found." in the index-free path, both in
/// `auto` mode and explicit `--mode ast-grep`.
#[test]
fn zero_setup_search_matches_identifier_substring() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    std::fs::write(
        proj.path().join("model.rs"),
        "pub struct BillingEntity { pub id: u64 }\n\
         fn use_it() { let _ = BillingEntity { id: 1 }; }\n",
    )
    .unwrap();

    // Exact identifier still works.
    bin(home.path(), proj.path())
        .args([
            "search",
            "BillingEntity",
            "--mode",
            "ast-grep",
            "--format",
            "json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("model.rs"));

    // Substring (auto mode, no index): the un-init'd dir degrades to the live
    // path, which must now find "Billing" inside "BillingEntity".
    bin(home.path(), proj.path())
        .args(["search", "Billing", "--format", "json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("model.rs"))
        .stdout(predicate::str::contains("No results found.").not());

    // Substring, explicit ast-grep mode.
    bin(home.path(), proj.path())
        .args([
            "search", "Billing", "--mode", "ast-grep", "--format", "json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("model.rs"));

    // Case-insensitive.
    bin(home.path(), proj.path())
        .args([
            "search", "billing", "--mode", "ast-grep", "--format", "json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("model.rs"));

    // A genuinely-absent string still reports no results.
    bin(home.path(), proj.path())
        .args(["search", "Zzznotpresent", "--mode", "ast-grep"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No results found."));
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

// ── gap: the top-level `Sync` arm (main.rs), distinct from memory-dispatch ─────

#[test]
fn sync_arm_refuses_without_local_project() {
    // `spelunk sync` is a top-level command whose guard lives in `main.rs`, not in
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
    // Every `memory` subcommand resolves its store through the same
    // `require_project_db` line before dispatch; `timeline` (needs no server)
    // confirms the guard is not specific to add/list/search.
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
        .stderr(predicate::str::contains(NO_PROJECT_ERR));

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

    let global_index = home.path().join(".config").join("spelunk").join("index.db");
    std::fs::create_dir_all(global_index.parent().unwrap()).unwrap();
    let sentinel = b"pre-existing global index sentinel";
    std::fs::write(&global_index, sentinel).unwrap();

    // Index-backed search fails closed before opening any DB, so even a stray
    // global index.db is never read or written.
    bin(home.path(), proj.path())
        .args(["search", "anything", "--mode", "text"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(NO_PROJECT_ERR));

    assert_eq!(
        std::fs::read(&global_index).unwrap(),
        sentinel,
        "refused index-backed search must not touch the pre-existing global index"
    );
}

// ── walk-up: memory resolves the ancestor project from a deep subdir ───────────

#[test]
fn memory_add_works_from_deep_nested_subdir() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    std::fs::create_dir_all(proj.path().join(".spelunk")).unwrap();
    let deep = proj.path().join("a").join("b").join("c");
    std::fs::create_dir_all(&deep).unwrap();

    // Run several levels below the `.spelunk/` project root; the guard walks up
    // and resolves the ancestor's store, not the global one.
    bin(home.path(), &deep)
        .args([
            "memory", "add", "--kind", "note", "--title", "t", "--body", "b",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Stored [note]"));

    assert!(
        proj.path().join(".spelunk").join("memory.db").exists(),
        "note must land in the ancestor project's .spelunk/memory.db"
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
fn memory_resolves_main_worktree_dot_spelunk_from_linked_worktree() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let main_root = tmp.path().join("main");
    std::fs::create_dir_all(&main_root).unwrap();

    git(&main_root, &["init", "-q", "-b", "main"]);
    std::fs::write(main_root.join("f.txt"), "x\n").unwrap();
    git(&main_root, &["add", "."]);
    git(&main_root, &["commit", "-q", "-m", "init"]);

    // Only the main worktree is a real project (has `.spelunk/`).
    std::fs::create_dir_all(main_root.join(".spelunk")).unwrap();

    // Add a linked worktree with no `.spelunk/` of its own.
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
        !linked.join(".spelunk").exists(),
        "precondition: linked worktree has no .spelunk/"
    );

    // ADR-067 worktree-awareness: memory run from the linked worktree must resolve
    // to the MAIN worktree's `.spelunk/` store, not fail closed and not go global.
    bin(home.path(), &linked)
        .args([
            "memory", "add", "--kind", "note", "--title", "t", "--body", "b",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Stored [note]"));

    assert!(
        main_root.join(".spelunk").join("memory.db").exists(),
        "note must land in the main worktree's .spelunk/memory.db"
    );
    assert!(
        !linked.join(".spelunk").exists(),
        "the linked worktree must not get its own .spelunk/"
    );
    assert!(!global_memory_db(home.path()).exists());
}
