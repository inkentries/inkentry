//! Fail-closed behaviour when there is no local `.inkentry/` project (ADR-067).
//!
//! In a directory that was never `inkentry init`'d, memory/context/index-backed
//! search must refuse rather than silently read or write the machine-global
//! `~/.config/inkentry/` store. `--db` and `inkentry index` stay exempt. `status`
//! reports "no project" instead of describing the global store.

mod plumbing_helpers;
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
            .join("inkentry")
            .join("index.db")
            .exists(),
        "ast-grep search must not create the global index"
    );
    assert!(!global_memory_db(home.path()).exists());
}

// ── zero-setup plain-string substring search ───────────────────────────────────

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

    let global_index = home.path().join(".config").join("inkentry").join("index.db");
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

// ── display commands: graph / chunks / explore / check ─────────────────────────
//
// These read-only commands previously resolved their DB via the legacy
// `open_project_db`/`resolve_db` path, which fell back to the machine-global
// `index.db` in an un-init'd dir and displayed cross-project data. They now share
// ADR-067's fail-closed resolver: refuse (or run live) instead of reading global.

#[test]
fn graph_runs_live_without_local_project() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    std::fs::write(
        proj.path().join("lib.rs"),
        "pub fn greet(name: &str) -> String { format!(\"hi {name}\") }\n\
         fn caller() { greet(\"x\"); }\n",
    )
    .unwrap();

    // A symbol query in an un-init'd dir must degrade to the live ast-grep graph
    // (mirroring search), matching the `greet(...)` call site in `caller`, not
    // read the machine-global index.
    bin(home.path(), proj.path())
        .args(["graph", "greet", "--format", "json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("lib.rs"))
        .stdout(predicate::str::contains("calls"));

    assert!(
        !global_index_db(home.path()).exists(),
        "live graph must not create the global index"
    );
}

#[test]
fn graph_does_not_read_preexisting_global_index() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    std::fs::write(
        proj.path().join("lib.rs"),
        "pub fn greet(name: &str) -> String { format!(\"hi {name}\") }\n\
         fn caller() { greet(\"x\"); }\n",
    )
    .unwrap();

    let global = global_index_db(home.path());
    std::fs::create_dir_all(global.parent().unwrap()).unwrap();
    let sentinel = b"pre-existing global index sentinel";
    std::fs::write(&global, sentinel).unwrap();

    // The symbol query runs live and must leave the stray global index untouched
    // rather than opening it (the pre-fix bug displayed its cross-project edges).
    bin(home.path(), proj.path())
        .args(["graph", "greet", "--format", "json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("lib.rs"));

    assert_eq!(
        std::fs::read(&global).unwrap(),
        sentinel,
        "live graph must not open or mutate the pre-existing global index"
    );
}

#[test]
fn graph_file_query_refuses_without_local_project() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();

    let global = global_index_db(home.path());
    std::fs::create_dir_all(global.parent().unwrap()).unwrap();
    let sentinel = b"pre-existing global index sentinel";
    std::fs::write(&global, sentinel).unwrap();

    // A file-path query needs the index and has no live mode, so it must refuse
    // rather than fall back to the global store.
    bin(home.path(), proj.path())
        .args(["graph", "src/lib.rs"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(NO_PROJECT_ERR));

    assert_eq!(
        std::fs::read(&global).unwrap(),
        sentinel,
        "refused graph file-query must not open or mutate the pre-existing global index"
    );
}

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

#[test]
fn explore_refuses_without_local_project() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();

    let global = global_index_db(home.path());
    std::fs::create_dir_all(global.parent().unwrap()).unwrap();
    let sentinel = b"pre-existing global index sentinel";
    std::fs::write(&global, sentinel).unwrap();

    // The project gate fires before any server probe, so an un-init'd dir refuses
    // with the ADR-067 message rather than reading the global index.
    bin(home.path(), proj.path())
        .args(["explore", "how does auth work"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(NO_PROJECT_ERR));

    assert_eq!(
        std::fs::read(&global).unwrap(),
        sentinel,
        "refused explore must not open or mutate the pre-existing global index"
    );
}

#[test]
fn check_refuses_without_local_project() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();

    let global = global_index_db(home.path());
    std::fs::create_dir_all(global.parent().unwrap()).unwrap();
    let sentinel = b"pre-existing global index sentinel";
    std::fs::write(&global, sentinel).unwrap();

    bin(home.path(), proj.path())
        .args(["check"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(NO_PROJECT_ERR));

    assert_eq!(
        std::fs::read(&global).unwrap(),
        sentinel,
        "refused check must not open or mutate the pre-existing global index"
    );
}

// ── happy path: an init'd project still resolves graph/chunks/check locally ────
//
// The fail-closed rework must not break the normal case: with a real local
// `.inkentry/index.db`, the display commands resolve LOCAL (not global) and work.
// A stray global index is left in place to prove they read local, not global.

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

    // graph: index-backed symbol query resolves the LOCAL index and shows the edge.
    bin(home.path(), proj.path())
        .args(["graph", "local_target", "--format", "json"])
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

    // check: resolves the LOCAL index and reports on it rather than refusing.
    bin(home.path(), proj.path())
        .args(["check"])
        .assert()
        .success();

    assert_eq!(
        std::fs::read(&global).unwrap(),
        sentinel,
        "init'd-project display commands must resolve local and never touch the global store"
    );
}

// ── strongest isolation assertion: a REAL populated global is never surfaced ───
//
// The refuse tests above use garbage-byte sentinels (which prove the file is not
// opened). graph is the one display command with a live fallback, so it is the
// only place a resolver regression could actually *print* cross-project data.
// This uses a genuine populated global index and asserts its data never appears.

#[test]
fn graph_does_not_surface_real_populated_global_index() {
    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();

    // Build a real index elsewhere, then copy it into the machine-global location
    // so the global store holds genuine graph edges for `global_only_symbol`.
    let global_src = tmp.path().join("global_src");
    std::fs::create_dir_all(&global_src).unwrap();
    std::fs::write(
        global_src.join("secret_global_file.rs"),
        "pub fn global_only_symbol() -> u32 { 1 }\n\
         fn global_only_caller() { let _ = global_only_symbol(); }\n",
    )
    .unwrap();
    bin(home.path(), &global_src)
        .args(["index", "."])
        .assert()
        .success();
    let real_global_index = global_src.join(".inkentry").join("index.db");
    assert!(
        real_global_index.exists(),
        "precondition: real global index built"
    );

    let global = global_index_db(home.path());
    std::fs::create_dir_all(global.parent().unwrap()).unwrap();
    std::fs::copy(&real_global_index, &global).unwrap();
    let before = std::fs::read(&global).unwrap();

    // A sibling (not ancestor) un-init'd dir with no source defining the symbol.
    // If graph regressed to reading the global store it would print
    // `global_only_caller` / `secret_global_file`; the fail-closed live scan over
    // this empty dir must not.
    let proj = tmp.path().join("uninit");
    std::fs::create_dir_all(&proj).unwrap();

    bin(home.path(), &proj)
        .args(["graph", "global_only_symbol", "--format", "json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("global_only_caller").not())
        .stdout(predicate::str::contains("secret_global_file").not());

    assert_eq!(
        std::fs::read(&global).unwrap(),
        before,
        "graph in an un-init'd dir must not open the real populated global index"
    );
}

// ── zero-result affordance: empty tree vs no-match hint ───────────────────────
//
// When the live graph scan finds no call sites, the message disambiguates a true
// leaf/typo (scannable source present) from an empty tree (e.g. an umbrella repo
// with uninitialized submodules). Neither message suggests `inkentry init`. Text
// output only — the branch is in the text path; JSON stays a bare edge array.

#[test]
fn graph_empty_dir_reports_no_scannable_source_hint() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();

    // No source at all: the live scan reports an empty tree and steers to a
    // populated subdir / submodule init, never to `inkentry init`.
    bin(home.path(), proj.path())
        .args(["graph", "anything"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No scannable source files"))
        .stdout(predicate::str::contains("submodules are initialized"))
        .stdout(predicate::str::contains("inkentry init").not())
        // The source-present call-scan wording must not leak into the empty-tree branch.
        .stdout(predicate::str::contains("No call-site invocations").not());
}

#[test]
fn graph_populated_dir_no_match_reports_live_scan() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    // Scannable source exists, but `BillingEntity` is only referenced as a class,
    // never invoked as a bare `BillingEntity(...)` call. The live structural scan
    // is call-syntax-only, so it finds nothing even though the symbol is heavily
    // used, not unused. The message must say so and point at the full index.
    std::fs::write(
        proj.path().join("model.rb"),
        "class BillingEntity\nend\nclass Invoice < BillingEntity\nend\n",
    )
    .unwrap();

    let assert = bin(home.path(), proj.path())
        .args(["graph", "BillingEntity"])
        .assert()
        .success()
        // New wording: scope the empty result to call-site syntax, not "unused".
        .stdout(predicate::str::contains(
            "No call-site invocations of 'BillingEntity' found",
        ))
        .stdout(predicate::str::contains("calls only"))
        // Source is present, so the full-index hint is appended.
        .stdout(predicate::str::contains("inkentry init"))
        .stdout(predicate::str::contains("imports/extends/implements"))
        // The old misleading "No callers found ... (live scan)" wording is gone.
        .stdout(predicate::str::contains("No callers found").not())
        .stdout(predicate::str::contains("No scannable source files").not());

    // No em-dash in any user-facing line (reads as an AI-tell in public copy).
    let out = assert.get_output();
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains('\u{2014}'),
        "graph live empty message must not contain an em-dash"
    );
}

/// Deferral anchor: the live scan matches only bare `symbol($$$)` call syntax, so
/// a symbol reached solely through a receiver (`X.new(...)`, `obj.method(...)`)
/// currently falls into the same "no call-site invocations" branch as a class or
/// constant reference. Broadening the matcher to receiver-method
/// `$_.<symbol>($$$)` calls was intentionally left out of this change; this test
/// pins the present behaviour so that future broadening has a failing anchor to
/// flip rather than silently changing an untested path.
#[test]
fn graph_receiver_method_only_symbol_reports_no_call_site_match() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    // `Widget` appears only as the receiver of `Widget.new(1)` and never as a bare
    // `Widget(...)` call. The receiver-method form is not yet scanned, so the live
    // result is empty and takes the source-present no-match branch.
    std::fs::write(
        proj.path().join("factory.rb"),
        "def make\n  Widget.new(1)\nend\n",
    )
    .unwrap();

    let assert = bin(home.path(), proj.path())
        .args(["graph", "Widget"])
        .assert()
        .success()
        // Receiver-method usage is not matched, so it reads as the call-site
        // no-match line (source is present), not as an edge and not as empty-tree.
        .stdout(predicate::str::contains(
            "No call-site invocations of 'Widget' found",
        ))
        .stdout(predicate::str::contains("receiver-method references"))
        .stdout(predicate::str::contains("Incoming to 'Widget'").not())
        .stdout(predicate::str::contains("No scannable source files").not());

    // Same no-em-dash guard as the sibling no-match test.
    let out = assert.get_output();
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains('\u{2014}'),
        "graph live empty message must not contain an em-dash"
    );
}

#[test]
fn graph_populated_dir_with_call_site_still_prints_edges() {
    // Regression guard: the zero-result branching must not swallow a real hit.
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    std::fs::write(
        proj.path().join("lib.rs"),
        "pub fn greet(name: &str) -> String { format!(\"hi {name}\") }\n\
         fn caller() { greet(\"x\"); }\n",
    )
    .unwrap();

    // Text output: the call site is listed and neither zero-result hint fires.
    bin(home.path(), proj.path())
        .args(["graph", "greet"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Incoming to 'greet'"))
        .stdout(predicate::str::contains("lib.rs"))
        .stdout(predicate::str::contains("No graph edges found").not());
}

// ── zero-result affordance: INDEXED (init'd) project branches ─────────────────
//
// The tests above exercise the un-init'd auto-live path. These cover the
// index-backed branches of the graph zero-result rework, all inside a real
// `.inkentry/`-init'd project:
//   * empty graph table         → auto-fall-back to the live scan
//   * populated graph, no symbol → a distinct, index-specific hint
//   * file-path query, no edges  → the unchanged "No graph edges found" message
// The project is already initialized, so no zero-result path here may ever
// suggest `inkentry init` (asserted on both stdout and stderr).

/// An init'd project whose `graph_edges` table is EMPTY (a graph-less or
/// freshly-created index) must AUTO-FALL-BACK to the live ast-grep scan for a
/// symbol query, never the ambiguous "no calls in the indexed graph" hint. Proven
/// by clearing every edge from a real local index, then confirming the query
/// still surfaces the working-tree call site through the live scan.
#[test]
fn graph_empty_index_auto_falls_back_to_live_scan() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    std::fs::write(
        proj.path().join("lib.rs"),
        "pub fn greet(name: &str) -> String { format!(\"hi {name}\") }\n\
         fn caller() { greet(\"x\"); }\n",
    )
    .unwrap();

    // Build a real local index; the parse phase populates graph_edges.
    bin(home.path(), proj.path())
        .args(["index", "."])
        .assert()
        .success();
    let index_db = proj.path().join(".inkentry").join("index.db");
    assert!(index_db.exists(), "precondition: local index built");

    // Test-only DB surgery: drop every graph edge so the index is a valid project
    // with an EMPTY graph table. Regular-table access needs no sqlite-vec
    // registration — the vec0 vtabs are never touched. A scoped connection so it
    // is fully closed before the CLI reopens the same DB.
    {
        let conn = rusqlite::Connection::open(&index_db).expect("open index db");
        let cleared = conn
            .execute("DELETE FROM graph_edges", [])
            .expect("clear graph_edges");
        assert!(cleared > 0, "precondition: index had edges to clear");
    }

    // `--no-stale-check` isolates the empty-graph fallback from the pre-existing
    // stale-index fallback, so this proves the has_any_graph_edges()==false path.
    // The live scan runs over the working tree, which still holds the greet call.
    bin(home.path(), proj.path())
        .args(["graph", "greet", "--no-stale-check"])
        .assert()
        .success()
        .stdout(predicate::str::contains("live scan"))
        .stdout(predicate::str::contains("Incoming to 'greet'"))
        .stdout(predicate::str::contains("lib.rs"))
        .stdout(predicate::str::contains("No calls to 'greet' found in the indexed graph").not())
        .stdout(predicate::str::contains("inkentry init").not())
        .stderr(predicate::str::contains("inkentry init").not());
}

/// A populated graph that simply lacks the queried symbol gets a distinct,
/// index-specific hint steering to `--live` — NOT the live-scan no-match line and
/// NOT the empty-graph live fallback. `--no-stale-check` keeps the fresh index off
/// the stale-fallback path so the has_any_graph_edges()==true branch is what fires.
#[test]
fn graph_populated_index_missing_symbol_reports_distinct_hint() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    std::fs::write(
        proj.path().join("lib.rs"),
        "pub fn greet(name: &str) -> String { format!(\"hi {name}\") }\n\
         fn caller() { greet(\"x\"); }\n",
    )
    .unwrap();

    bin(home.path(), proj.path())
        .args(["index", "."])
        .assert()
        .success();

    // `no_such_symbol_xyz` has no edge, but the graph is populated (greet/caller),
    // so has_any_graph_edges() is true → the index-specific hint. It must be
    // distinct from the live path's "(live scan)" wording.
    bin(home.path(), proj.path())
        .args(["graph", "no_such_symbol_xyz", "--no-stale-check"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "No calls to 'no_such_symbol_xyz' found in the indexed graph",
        ))
        .stdout(predicate::str::contains(
            "inkentry graph no_such_symbol_xyz --live",
        ))
        .stdout(predicate::str::contains("(live scan)").not())
        .stdout(predicate::str::contains("inkentry init").not())
        .stderr(predicate::str::contains("inkentry init").not());
}

/// A file-path query with no edges keeps the unchanged "No graph edges found"
/// message and, like every zero-result path, never suggests `inkentry init`.
#[test]
fn graph_file_query_no_edges_reports_no_edges_message() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    std::fs::write(
        proj.path().join("lib.rs"),
        "pub fn greet(name: &str) -> String { format!(\"hi {name}\") }\n\
         fn caller() { greet(\"x\"); }\n",
    )
    .unwrap();

    bin(home.path(), proj.path())
        .args(["index", "."])
        .assert()
        .success();

    // A path-shaped query (contains '/', ends in .rs) is a file query; a file
    // absent from the index has no edges, so the file-query branch prints the
    // unchanged message rather than any live-scan or `init` hint.
    bin(home.path(), proj.path())
        .args(["graph", "src/does_not_exist.rs"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "No graph edges found for 'src/does_not_exist.rs'.",
        ))
        .stdout(predicate::str::contains("inkentry init").not())
        .stderr(predicate::str::contains("inkentry init").not());
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
