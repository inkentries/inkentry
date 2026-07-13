//! ADR-068 D3: git-notes memory carrier for `memory add`/`list` before `init`.
//!
//! Store priority for `memory add`/`list` (ADR-004, unchanged) resolves in order:
//!   1. `--backend git-notes` → git notes as the *primary* store
//!   2. explicit team `server_url` (CloudFirst → remote)
//!   3. a resolvable local `.spelunk/` DB (sqlite)
//!   4. no DB but inside a git repo → the universal git-notes write-through is
//!      the sole writer (ref `refs/notes/spelunk`); there is no SQLite primary
//!   5. neither → fail with the dual-escape-hatch message.
//!
//! Pre-`init` (case 4) rides the same `append_to_git_notes` write-through that
//! already runs post-`init`, so every note carries an identical record shape.
//! These tests cover cases 1, 3, 4, and 5, the single-record invariant, record
//! shape parity between the pre-init and post-init write-through forms, and the
//! secret-scan gate on the git-notes path. The complementary refuse-only tests
//! (case 5 from a bare temp dir) live in `fail_closed_no_project.rs`.

mod plumbing_helpers;
use plumbing_helpers::spelunk_bin_in;

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::Path;
use tempfile::TempDir;

/// ADR-068 D3 dual-escape-hatch error (case 5): neither a project DB nor a
/// usable git repo. Kept in sync with `fail_closed_no_project.rs`.
const NO_PROJECT_NO_REPO_ERR: &str = "no spelunk project here, and not inside a git repo. Run 'spelunk init' first, \
     or run inside a git repository.";

/// A `spelunk` command with an isolated HOME (so the "global" store lives under
/// `<home>/.config/spelunk`) and no server contact, run in `cwd`.
fn bin(home: &Path, cwd: &Path) -> Command {
    let mut cmd = spelunk_bin_in(home);
    cmd.current_dir(cwd)
        .env("SPELUNK_NO_SERVER", "1")
        .env_remove("SPELUNK_SERVER_URL");
    cmd
}

/// The global memory store path under the isolated HOME. The git-notes fallback
/// must never create it.
fn global_memory_db(home: &Path) -> std::path::PathBuf {
    home.join(".config").join("spelunk").join("memory.db")
}

/// Run `git args` in `dir`, asserting success. Isolated identity so it works on a
/// machine with no global git config.
fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
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
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// stdout of `git args` in `dir` (whatever the exit status). Used for read-only
/// notes inspection where a missing ref is a legitimate empty result.
fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A git repo with one commit and no `.spelunk/`. `user.*` is set in the LOCAL
/// repo config so the `git notes add` that the spawned `spelunk` runs (which
/// does NOT inherit the test's `GIT_*` identity env) has a committer identity.
fn init_git_repo_with_commit(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    // Local (not env) identity: the spelunk child reads this from `.git/config`.
    git(dir, &["config", "user.name", "t"]);
    git(dir, &["config", "user.email", "t@example.com"]);
    std::fs::write(dir.join("f.txt"), "x\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

/// The spelunk records currently in HEAD's `refs/notes/spelunk` note (one JSON
/// object per line). Empty when the ref/note does not exist.
fn spelunk_note_lines(dir: &Path) -> Vec<String> {
    let blob = git_stdout(dir, &["notes", "--ref=spelunk", "show", "HEAD"]);
    blob.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

// ── case 4: happy-path round-trip via git notes ────────────────────────────────

#[test]
fn memory_add_list_round_trips_via_git_notes_fallback() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());

    let title = "fallback-roundtrip-abc123";

    // add: no `.spelunk/`, but inside a git repo → falls back to git-notes.
    bin(home.path(), repo.path())
        .args([
            "memory", "add", "--kind", "note", "--title", title, "--body", "b",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Stored [note]"));

    // The entry landed in `refs/notes/spelunk` on HEAD.
    let note_blob = git_stdout(repo.path(), &["notes", "--ref=spelunk", "show", "HEAD"]);
    assert!(
        note_blob.contains(title),
        "the note on HEAD must contain the added entry's title; got: {note_blob:?}"
    );
    // `git notes list` shows exactly one noted commit (HEAD).
    let list = git_stdout(repo.path(), &["notes", "--ref=spelunk", "list"]);
    assert_eq!(
        list.lines().filter(|l| !l.trim().is_empty()).count(),
        1,
        "exactly one commit (HEAD) should carry a spelunk note; got: {list:?}"
    );

    // list: reads the entry back through the same git-notes fallback.
    bin(home.path(), repo.path())
        .args(["memory", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains(title));

    // The fallback must not create a local `.spelunk/` nor touch the global store.
    assert!(
        !repo.path().join(".spelunk").exists(),
        "git-notes fallback must not create a local .spelunk/ project"
    );
    assert!(
        !global_memory_db(home.path()).exists(),
        "git-notes fallback must not create the machine-global memory store"
    );
}

// ── single record per single `add`: the carrier is the sole writer ─────────────

#[test]
fn single_add_writes_exactly_one_note_record() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());

    // Pre-init there is no SQLite primary: the write-through carrier is the sole
    // writer, so a single `add` must leave exactly one JSON record in the note,
    // not two (no separate primary append + write-through).
    bin(home.path(), repo.path())
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "one-and-only",
            "--body",
            "b",
        ])
        .assert()
        .success();

    let lines = spelunk_note_lines(repo.path());
    assert_eq!(
        lines.len(),
        1,
        "a single `memory add` must write exactly one record line to the note; got: {lines:?}"
    );
    assert!(
        lines[0].contains("\"schema_version\":1") && lines[0].contains("one-and-only"),
        "the single record must be the well-formed entry we added; got: {:?}",
        lines[0]
    );
}

// ── record-shape parity: pre-init carrier == post-init write-through form ──────

/// Top-level object keys of a one-line JSON object, sorted. A minimal
/// depth-aware scan (integration tests can't reach the crate's `serde_json`):
/// only quoted strings at brace-depth 1 that are immediately followed by `:`
/// count, so nested-array elements and string *values* are ignored.
fn json_top_level_keys(line: &str) -> Vec<String> {
    let bytes = line.as_bytes();
    let mut keys = Vec::new();
    let mut depth: i32 = 0;
    let mut in_str = false;
    let mut escaped = false;
    let mut cur = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if in_str {
            if escaped {
                cur.push(c);
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
                let mut j = i + 1;
                while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                    j += 1;
                }
                if depth == 1 && j < bytes.len() && bytes[j] as char == ':' {
                    keys.push(std::mem::take(&mut cur));
                } else {
                    cur.clear();
                }
            } else {
                cur.push(c);
            }
        } else {
            match c {
                '"' => {
                    in_str = true;
                    cur.clear();
                }
                '{' | '[' => depth += 1,
                '}' | ']' => depth -= 1,
                _ => {}
            }
        }
        i += 1;
    }
    keys.sort();
    keys
}

/// The single note record a pre-init `add` writes (via the carrier) must have
/// the exact same field set as the record a post-init `add` writes (via the
/// SQLite-primary write-through). Both flow through one `append_to_git_notes`
/// path, so any divergence in the pre-init record shape is a regression.
#[test]
fn pre_init_and_post_init_records_have_identical_shape() {
    let home = TempDir::new().unwrap();

    // Pre-init: no `.spelunk/`, inside a git repo → carrier writes the record.
    let pre = TempDir::new().unwrap();
    init_git_repo_with_commit(pre.path());
    bin(home.path(), pre.path())
        .args([
            "memory",
            "add",
            "--kind",
            "note",
            "--title",
            "shape-pre",
            "--body",
            "b",
        ])
        .assert()
        .success();
    let pre_lines = spelunk_note_lines(pre.path());
    assert_eq!(pre_lines.len(), 1, "pre-init add writes one record");

    // Post-init: a local `.spelunk/` makes SQLite the primary; the same
    // write-through then appends the note. Creating the dir is enough for
    // `require_project_db` to resolve the project (matches the precedence test).
    let post = TempDir::new().unwrap();
    init_git_repo_with_commit(post.path());
    std::fs::create_dir_all(post.path().join(".spelunk")).unwrap();
    bin(home.path(), post.path())
        .args([
            "memory",
            "add",
            "--kind",
            "note",
            "--title",
            "shape-post",
            "--body",
            "b",
        ])
        .assert()
        .success();
    let post_lines = spelunk_note_lines(post.path());
    assert_eq!(
        post_lines.len(),
        1,
        "post-init write-through writes one record"
    );

    assert_eq!(
        json_top_level_keys(&pre_lines[0]),
        json_top_level_keys(&post_lines[0]),
        "pre-init carrier and post-init write-through records must share one shape\n\
         pre:  {}\npost: {}",
        pre_lines[0],
        post_lines[0]
    );
}

// ── case 5: refuse when not inside a git repo (empty / no-HEAD repo) ────────────

#[test]
fn memory_add_refuses_in_git_repo_without_any_commit() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    // `git init` but no commit → HEAD is unresolvable, so the fallback cannot
    // attach a note. This is case 5, not case 4.
    git(repo.path(), &["init", "-q", "-b", "main"]);

    bin(home.path(), repo.path())
        .args([
            "memory", "add", "--kind", "note", "--title", "t", "--body", "b",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(NO_PROJECT_NO_REPO_ERR));

    assert!(!global_memory_db(home.path()).exists());
    assert!(
        spelunk_note_lines(repo.path()).is_empty(),
        "a refused add in an empty repo must not write any spelunk note"
    );
}

#[test]
fn memory_list_refuses_in_git_repo_without_any_commit() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    git(repo.path(), &["init", "-q", "-b", "main"]);

    bin(home.path(), repo.path())
        .args(["memory", "list"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(NO_PROJECT_NO_REPO_ERR));

    assert!(!global_memory_db(home.path()).exists());
}

// ── precedence #3 > #4: a local `.spelunk/` wins over the git-notes fallback ────

#[test]
fn local_dot_spelunk_takes_precedence_over_git_notes_fallback() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());
    // Both a git repo AND a local project: sqlite must win (fallback NOT taken).
    std::fs::create_dir_all(repo.path().join(".spelunk")).unwrap();

    bin(home.path(), repo.path())
        .args([
            "memory",
            "add",
            "--kind",
            "note",
            "--title",
            "sqlite-wins",
            "--body",
            "b",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Stored [note]"));

    // The entry went to the local sqlite store, proving branch 3 beat branch 4.
    assert!(
        repo.path().join(".spelunk").join("memory.db").exists(),
        "with a local .spelunk/, add must write sqlite, not fall back to git-notes"
    );

    // list resolves the same sqlite store and reads the entry back.
    bin(home.path(), repo.path())
        .args(["memory", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("sqlite-wins"));

    assert!(!global_memory_db(home.path()).exists());
}

// ── precedence #1: explicit `--backend git-notes` pre-init works ───────────────

#[test]
fn explicit_backend_git_notes_works_pre_init_in_git_repo() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());

    let title = "explicit-git-notes-xyz";
    bin(home.path(), repo.path())
        .args([
            "memory",
            "add",
            "--backend",
            "git-notes",
            "--kind",
            "note",
            "--title",
            title,
            "--body",
            "b",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Stored [note]"));

    let note_blob = git_stdout(repo.path(), &["notes", "--ref=spelunk", "show", "HEAD"]);
    assert!(
        note_blob.contains(title),
        "explicit git-notes add must write the note; got: {note_blob:?}"
    );

    bin(home.path(), repo.path())
        .args(["memory", "list", "--backend", "git-notes"])
        .assert()
        .success()
        .stdout(predicate::str::contains(title));

    // Explicit git-notes must not create a project or touch the global store.
    assert!(!repo.path().join(".spelunk").exists());
    assert!(!global_memory_db(home.path()).exists());
}

// ── secret-scan gate on the git-notes path ─────────────────────────────────────

#[test]
fn secret_in_entry_is_refused_and_leaves_git_notes_untouched() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());

    // Title matches the AWS access-key-id pattern (`AKIA` + 16 upper/digits). The
    // secret scan runs before any persistence, so the git-notes fallback path
    // must refuse and write nothing.
    bin(home.path(), repo.path())
        .args([
            "memory",
            "add",
            "--kind",
            "note",
            "--title",
            "AKIAIOSFODNN7EXAMPLE",
            "--body",
            "harmless body",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("secret pattern"));

    assert!(
        spelunk_note_lines(repo.path()).is_empty(),
        "a secret-blocked add must leave refs/notes/spelunk absent/unmodified"
    );
    // And a body-borne secret is likewise blocked before any note is written
    // (GitHub PAT pattern, same fixture the secrets unit test uses).
    bin(home.path(), repo.path())
        .args([
            "memory",
            "add",
            "--kind",
            "note",
            "--title",
            "looks-innocent",
            "--body",
            "token = ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdef123456789012",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("secret pattern"));

    assert!(
        spelunk_note_lines(repo.path()).is_empty(),
        "a body-secret-blocked add must also leave the note ref untouched"
    );
}
