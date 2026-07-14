//! Integration tests for `spelunk init` configuring the `origin` git-notes
//! fetch refspec so `refs/notes/spelunk` (spelunk's memory) travels on
//! clone/fetch (ADR-068).
//!
//! Covered:
//! - origin present: `remote.origin.fetch` gains `+refs/notes/spelunk:…` and
//!   init announces the configured line.
//! - origin absent: init still exits 0 and prints the exact manual hint.
//! - idempotent: two inits leave exactly ONE notes refspec + "already
//!   configured" announce on the second run.
//! - push preserved: `remote.origin.push` stays unset (branch-push default).
//! - round-trip: notes pushed to a bare origin are fetchable back via the
//!   configured refspec into a fresh clone.
//! - non-TTY: piped-stdin init completes without prompting/hanging.
//!
//! Every spawned `spelunk` uses `spelunk_bin` (pins `SPELUNK_SECRET_STORE=file`),
//! `SPELUNK_NO_SERVER=1`, and `init --no-index` for an offline, fast run.

mod plumbing_helpers;
use plumbing_helpers::spelunk_bin;

use predicates::prelude::*;
use std::path::Path;
use std::process::Output;
use tempfile::tempdir;

const NOTES_REFSPEC: &str = "+refs/notes/spelunk:refs/notes/spelunk";

/// Run `git args` in `dir`, asserting success. Isolated identity + config so it
/// works hermetically on a machine with (or without) a global git config.
fn git(dir: &Path, args: &[&str]) {
    let out = git_out(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Like [`git`] but returns the captured `Output` without asserting success.
fn git_out(dir: &Path, args: &[&str]) -> Output {
    std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git")
}

/// `stdout` of `git args` as a trimmed `String`.
fn git_stdout(dir: &Path, args: &[&str]) -> String {
    String::from_utf8_lossy(&git_out(dir, args).stdout)
        .trim()
        .to_string()
}

/// A git repo with a real identity + one commit. Returns nothing; caller owns
/// the dir. Local identity is set so spawned `git` (and spelunk's inner git)
/// can commit without inheriting the test-runner's global config.
fn init_repo_with_commit(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    std::fs::write(dir.join("README.md"), "# test\n").unwrap();
    git(dir, &["add", "README.md"]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

/// Write an empty spelunk config (init needs `--config` but no values here).
fn empty_config(dir: &Path) -> std::path::PathBuf {
    let cfg = dir.join("config.toml");
    std::fs::write(&cfg, "").unwrap();
    cfg
}

/// Run `spelunk init --no-index` in `dir` (offline, non-TTY) and return stdout.
fn run_init(dir: &Path) -> String {
    let cfg = empty_config(dir);
    let out = spelunk_bin()
        .current_dir(dir)
        .env("HOME", dir)
        .env("SPELUNK_NO_SERVER", "1")
        .arg("--config")
        .arg(&cfg)
        .args(["init", "--no-index"])
        .output()
        .expect("spawn spelunk init");
    assert!(
        out.status.success(),
        "spelunk init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// (1) With an `origin` remote: init adds the notes fetch refspec and announces it.
#[test]
fn init_configures_notes_refspec_when_origin_present() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let origin = tmp.path().join("origin.git");
    std::fs::create_dir_all(&repo).unwrap();

    git(
        tmp.path(),
        &["init", "--bare", "-q", origin.to_str().unwrap()],
    );
    init_repo_with_commit(&repo);
    git(
        &repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );

    let stdout = run_init(&repo);

    let fetch = git_stdout(&repo, &["config", "--get-all", "remote.origin.fetch"]);
    assert!(
        fetch.lines().any(|l| l.trim() == NOTES_REFSPEC),
        "remote.origin.fetch should contain the notes refspec, got:\n{fetch}"
    );
    assert!(
        stdout.contains("Memory:") && stdout.contains("configured notes fetch refspec on 'origin'"),
        "init stdout should announce the configured refspec, got:\n{stdout}"
    );
}

/// (2) No `origin` remote: init still succeeds and prints the exact manual hint.
#[test]
fn init_no_origin_prints_hint_and_succeeds() {
    let tmp = tempdir().unwrap();
    init_repo_with_commit(tmp.path());

    let stdout = run_init(tmp.path()); // asserts exit 0 internally

    assert!(
        stdout.contains(&format!(
            "git config --add remote.origin.fetch '{NOTES_REFSPEC}'"
        )),
        "no-origin init should print the exact refspec hint, got:\n{stdout}"
    );
    // The push hint frames the notes push as per-change, not one-time: each
    // memory add/remove makes a new notes commit that must be pushed to travel.
    assert!(
        stdout.contains("push notes after each memory change: git push origin refs/notes/spelunk"),
        "no-origin init should print the per-change notes push hint, got:\n{stdout}"
    );
    // And it must not have invented an `origin` remote.
    assert!(
        !git_out(tmp.path(), &["remote", "get-url", "origin"])
            .status
            .success(),
        "init must not create an origin remote when none exists"
    );
}

/// (3) Idempotent: two inits leave exactly one notes refspec + "already
/// configured" announce on the second run.
#[test]
fn init_notes_refspec_is_idempotent() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let origin = tmp.path().join("origin.git");
    std::fs::create_dir_all(&repo).unwrap();

    git(
        tmp.path(),
        &["init", "--bare", "-q", origin.to_str().unwrap()],
    );
    init_repo_with_commit(&repo);
    git(
        &repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );

    run_init(&repo);
    let second = run_init(&repo);

    let count = git_stdout(&repo, &["config", "--get-all", "remote.origin.fetch"])
        .lines()
        .filter(|l| l.trim() == NOTES_REFSPEC)
        .count();
    assert_eq!(
        count, 1,
        "notes refspec must appear exactly once after two inits"
    );
    assert!(
        second.contains("already configured"),
        "second init should report the refspec is already configured, got:\n{second}"
    );
}

/// (4) Push default preserved: `remote.origin.push` stays unset so a normal
/// `git push` keeps pushing the current branch (the engineer set no push refspec).
#[test]
fn init_does_not_set_origin_push_refspec() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let origin = tmp.path().join("origin.git");
    std::fs::create_dir_all(&repo).unwrap();

    git(
        tmp.path(),
        &["init", "--bare", "-q", origin.to_str().unwrap()],
    );
    init_repo_with_commit(&repo);
    git(
        &repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );

    run_init(&repo);

    let push = git_out(&repo, &["config", "--get", "remote.origin.push"]);
    assert!(
        !push.status.success() && String::from_utf8_lossy(&push.stdout).trim().is_empty(),
        "remote.origin.push must remain unset, got: {:?}",
        String::from_utf8_lossy(&push.stdout)
    );
}

/// (5) Round-trip (the promise): a note pushed to the bare origin is fetchable
/// back into a fresh clone via the configured notes refspec.
///
/// A. init in repo (configures the refspec) → add a decision (git note on
///    refs/notes/spelunk) → push the branch + notes ref to the bare origin.
/// B. clone origin → run init in the clone (adds the same fetch refspec) →
///    plain `git fetch origin` pulls the notes → `git notes --ref=spelunk`
///    surfaces the decision. This proves the ref is publishable AND that the
///    init-configured refspec is what fetches it.
#[test]
fn notes_round_trip_through_bare_origin() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let origin = tmp.path().join("origin.git");
    let clone = tmp.path().join("clone");
    std::fs::create_dir_all(&repo).unwrap();

    git(
        tmp.path(),
        &["init", "--bare", "-q", origin.to_str().unwrap()],
    );
    init_repo_with_commit(&repo);
    git(
        &repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );

    // A. Configure the refspec, then add a decision via `spelunk memory add`
    //    (store_in_git_notes = true → writes refs/notes/spelunk).
    run_init(&repo);

    let mem_db = repo.join(".spelunk").join("memory.db");
    let cfg = repo.join("mem-config.toml");
    std::fs::write(
        &cfg,
        format!(
            "db_path = {:?}\nllm_model = \"x\"\nstore_in_git_notes = true\n",
            mem_db
        ),
    )
    .unwrap();

    let unique = "notes travel via the origin refspec";
    spelunk_bin()
        .current_dir(&repo)
        .env("HOME", &repo)
        .env("SPELUNK_NO_SERVER", "1")
        .env_remove("SPELUNK_SERVER_URL")
        .arg("--config")
        .arg(&cfg)
        .arg("memory")
        .arg("--db")
        .arg(&mem_db)
        .arg("add")
        .arg("--kind")
        .arg("decision")
        .arg("--title")
        .arg(unique)
        .arg("--body")
        .arg("Chosen so refs/notes/spelunk clone/fetch behaviour is observable.")
        .assert()
        .success()
        .stdout(predicate::str::contains("Stored [decision]"));

    // Sanity: the note exists locally on refs/notes/spelunk.
    assert!(
        !git_stdout(&repo, &["notes", "--ref=spelunk", "list"]).is_empty(),
        "expected a local spelunk note after memory add"
    );

    // Publish branch + notes to the bare origin.
    git(&repo, &["push", "-q", "origin", "main"]);
    git(&repo, &["push", "-q", "origin", "refs/notes/spelunk"]);

    // B. Fresh clone gets the branch but NOT notes by default…
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    // clone identity for its own inner git (init announces, no commit needed).
    git(&clone, &["config", "user.email", "clone@example.com"]);
    git(&clone, &["config", "user.name", "Clone"]);
    assert!(
        git_stdout(&clone, &["notes", "--ref=spelunk", "list"]).is_empty(),
        "a fresh clone should not have spelunk notes before fetch"
    );

    // …init in the clone configures the notes fetch refspec, and a plain fetch
    // then pulls the notes ref — the end-to-end promise.
    run_init(&clone);
    git(&clone, &["fetch", "-q", "origin"]);

    let notes = git_stdout(&clone, &["notes", "--ref=spelunk", "list"]);
    assert!(
        !notes.is_empty(),
        "clone should have the spelunk note after init-configured fetch"
    );

    // The decision content travelled, not just an empty ref. `list` lines are
    // `<note-obj> <annotated-obj>`; show the annotated object explicitly rather
    // than relying on the clone's HEAD matching it.
    let annotated = notes
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .expect("note list line has an annotated object")
        .to_string();
    let shown = git_stdout(&clone, &["notes", "--ref=spelunk", "show", &annotated]);
    assert!(
        shown.contains(unique),
        "fetched note should contain the decision title, got:\n{shown}"
    );
}

/// (6) Non-TTY: init run with piped stdin (as assert_cmd/Output does) must not
/// prompt or hang — it returns and exits 0. Explicit guard for the hook/CI path.
#[test]
fn init_non_tty_does_not_prompt_or_hang() {
    let tmp = tempdir().unwrap();
    init_repo_with_commit(tmp.path());

    // run_init spawns with piped (non-TTY) stdin and asserts exit 0; reaching
    // this line at all means init completed without blocking on input.
    let stdout = run_init(tmp.path());
    assert!(
        stdout.contains("spelunk initialised for"),
        "init should print its success summary in non-TTY mode, got:\n{stdout}"
    );
}
