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
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// ADR-068 D3 dual-escape-hatch error (case 5): neither a project DB nor a
/// usable git repo. Kept in sync with `fail_closed_no_project.rs`.
const NO_PROJECT_NO_REPO_ERR: &str = "no spelunk project here, and not inside a git repo. Run 'spelunk init' first, \
     or run inside a git repository.";

/// ADR-067 single-hatch error: no local `.spelunk/` project. This is what every
/// memory subcommand *except* the ADR-068 D3 add/list carrier still raises,
/// even inside a git repo (the carrier never widens to them). Distinct from
/// `NO_PROJECT_NO_REPO_ERR`: the dual-hatch text splices ", and not inside a git
/// repo" between "here" and ". Run", so this substring matches only the
/// single-hatch message.
const NO_PROJECT_ERR: &str = "no spelunk project here. Run 'spelunk init' first";

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

    let pre_keys = json_top_level_keys(&pre_lines[0]);
    let post_keys = json_top_level_keys(&post_lines[0]);

    // Guard against a degenerate match: an empty (or shrunk) key set on both
    // sides would satisfy a bare set-equality check. Assert both records actually
    // carry the canonical NoteRecord field set a `note` add with no
    // tags/files/dates serializes. (The Option-typed fields source_ref,
    // valid_at, invalid_at, superseded_by, and remote_id are omitted by serde
    // when None, so the always-present core below is the shape under test.)
    for expected in [
        "body",
        "created_at",
        "id",
        "kind",
        "linked_files",
        "schema_version",
        "status",
        "tags",
        "title",
    ] {
        assert!(
            pre_keys.iter().any(|k| k == expected),
            "pre-init record is missing the canonical key {expected:?}; got {pre_keys:?}"
        );
        assert!(
            post_keys.iter().any(|k| k == expected),
            "post-init record is missing the canonical key {expected:?}; got {post_keys:?}"
        );
    }

    assert_eq!(
        pre_keys, post_keys,
        "pre-init carrier and post-init write-through records must share one shape\n\
         pre:  {}\npost: {}",
        pre_lines[0], post_lines[0]
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

    // Double-write guard: with `--backend git-notes` git notes is the *primary*
    // store, so the universal write-through is suppressed. A single `add` must
    // therefore leave exactly one record (not a primary write plus a redundant
    // write-through): the other single-write path alongside the pre-init carrier.
    let lines = spelunk_note_lines(repo.path());
    assert_eq!(
        lines.len(),
        1,
        "explicit --backend git-notes must write exactly one record \
         (write-through suppressed); got: {lines:?}"
    );
    assert!(
        lines[0].contains("\"schema_version\":1") && lines[0].contains(title),
        "the single record must be the well-formed entry we added; got: {:?}",
        lines[0]
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

// ── carrier scope: only add/list ride it; siblings stay fail-closed ────────────

/// The ADR-068 D3 carrier is narrowed to `add`/`list`. Inside a git repo with a
/// commit (exactly the setup where `add`/`list` DO ride the carrier) every other
/// memory subcommand must still fail closed with the ADR-067 single-hatch
/// message, never reach the git-notes path, and never write a note. This guards
/// against the carrier accidentally widening its scope to non-add/list
/// subcommands.
#[test]
fn non_add_list_subcommands_stay_fail_closed_inside_git_repo() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());

    // A representative spread of the non-carrier subcommands, each needing no
    // server: read (search, timeline) and mutate (supersede).
    let invocations: [&[&str]; 3] = [
        &["memory", "search", "anything"],
        &["memory", "timeline", "anything"],
        &["memory", "supersede", "1", "2"],
    ];
    for args in invocations {
        bin(home.path(), repo.path())
            .args(args)
            .assert()
            .failure()
            // The ADR-067 single-hatch message, NOT the add/list dual-hatch: these
            // subcommands never consult the git repo for a carrier.
            .stderr(predicate::str::contains(NO_PROJECT_ERR))
            .stderr(predicate::str::contains("not inside a git repo").not());
    }

    assert!(
        spelunk_note_lines(repo.path()).is_empty(),
        "a fail-closed non-add/list subcommand must not write any spelunk note"
    );
    assert!(
        !global_memory_db(home.path()).exists(),
        "a fail-closed subcommand must not create the machine-global store"
    );
}

// ── case 6: post-init add writes BOTH the SQLite primary and the write-through ─

/// With a local `.spelunk/` project inside a git repo, a single `add` writes the
/// SQLite primary AND rides the universal git-notes write-through (exactly one
/// record, no double write), and `list` reads back from SQLite. This is the
/// unchanged post-`init` behaviour, asserted end-to-end in one flow.
#[test]
fn post_init_add_writes_sqlite_primary_and_git_notes_write_through() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());
    // A local project makes SQLite the primary (not the pre-init carrier).
    std::fs::create_dir_all(repo.path().join(".spelunk")).unwrap();

    bin(home.path(), repo.path())
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "post-init-both",
            "--body",
            "b",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Stored [decision]"));

    // Primary: the local SQLite store exists (proving branch 3, not the carrier).
    assert!(
        repo.path().join(".spelunk").join("memory.db").exists(),
        "post-init add must write the local SQLite primary"
    );

    // Write-through: exactly one record landed in refs/notes/spelunk (the SQLite
    // primary write plus the write-through must not double up).
    let lines = spelunk_note_lines(repo.path());
    assert_eq!(
        lines.len(),
        1,
        "post-init add must ride the write-through exactly once; got: {lines:?}"
    );
    assert!(
        lines[0].contains("post-init-both"),
        "the write-through record must be the entry we added; got: {:?}",
        lines[0]
    );

    // `list` (default sqlite backend) reads the entry back from SQLite.
    bin(home.path(), repo.path())
        .args(["memory", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("post-init-both"));

    assert!(!global_memory_db(home.path()).exists());
}

// ── case 7: a failed pre-init carry is fatal (no primary to fall back on) ───────

/// Pre-`init` the carrier is the SOLE writer, so a failed carry has no SQLite
/// primary to absorb it and must surface as a non-zero exit (an `Err`), never a
/// false "Stored". The carry is forced to fail deterministically: HEAD resolves
/// (so `git_head_reachable` engages the carrier) but the `git notes add` the
/// carrier runs has no usable committer identity: no local `user.*`, no
/// system/global config, and `user.useConfigOnly` on so git cannot auto-derive a
/// USER@host fallback (nor honour a stray `$EMAIL`). `git rev-parse HEAD` needs
/// no identity, so the carrier still engages and the failure is in the write.
#[test]
fn failed_pre_init_carry_is_fatal_and_writes_nothing() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();

    // A commit with NO local `user.*` identity in `.git/config`: the setup `git`
    // helper supplies identity via env for the commit only, so HEAD is resolvable
    // but the child's `git notes add` has nothing local to use.
    git(repo.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(repo.path().join("f.txt"), "x\n").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-q", "-m", "init"]);

    let mut cmd = spelunk_bin_in(home.path());
    cmd.current_dir(repo.path())
        .env("SPELUNK_NO_SERVER", "1")
        .env_remove("SPELUNK_SERVER_URL")
        // Neutralize every identity source for the git subprocess spelunk spawns.
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "user.useConfigOnly")
        .env("GIT_CONFIG_VALUE_0", "true")
        .args([
            "memory",
            "add",
            "--kind",
            "note",
            "--title",
            "carry-fails",
            "--body",
            "b",
        ]);

    cmd.assert()
        .failure()
        // No false success line, and the error names the fatal-carry context.
        .stdout(predicate::str::contains("Stored").not())
        .stderr(predicate::str::contains(
            "recording memory entry to git notes",
        ));

    assert!(
        spelunk_note_lines(repo.path()).is_empty(),
        "a fatal failed carry must not leave a partial spelunk note"
    );
    assert!(
        !global_memory_db(home.path()).exists(),
        "a fatal failed carry must not create the machine-global store"
    );
}

// ── the notes lock must not weaponize the fatal carry (ADR-069 D6 x D3) ────────

/// The wait budget the carrier allows before giving up on a contended notes
/// lock. Mirrors `LOCK_WAIT_BUDGET` in `storage/git_notes/lock.rs`.
const LOCK_WAIT_BUDGET: Duration = Duration::from_secs(5);

/// `<git-common-dir>/spelunk-notes.lock` — the file the carrier locks, resolved
/// the way the production code resolves it.
fn notes_lock_path(repo: &Path) -> std::path::PathBuf {
    let raw = git_stdout(repo, &["rev-parse", "--git-common-dir"]);
    let raw = raw.trim();
    let raw = Path::new(raw);
    let common_dir = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        repo.join(raw)
    };
    common_dir.join("spelunk-notes.lock")
}

/// A contended notes lock must never turn a working `memory add` into a failure.
///
/// Case 7 above makes a failed pre-`init` carry fatal: there is no SQLite
/// primary to absorb it. The carrier's lock (ADR-069 D6) is therefore bounded
/// and non-fatal by design — on contention it warns and writes unlocked. If it
/// ever returned an `Err` instead, contention alone would break `memory add` on
/// exactly the path that has nowhere to fall back to.
///
/// Deterministic: this test holds the lock across the child's whole run, from a
/// separate process, so the child is guaranteed to exhaust its budget.
#[test]
fn contended_notes_lock_does_not_fail_the_fatal_pre_init_carry() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());

    let title = "contended-lock-still-stored";

    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(notes_lock_path(repo.path()))
        .expect("open the notes lock file");
    held.lock()
        .expect("hold the notes lock across the child run");

    let started = Instant::now();
    bin(home.path(), repo.path())
        .args([
            "memory", "add", "--kind", "note", "--title", title, "--body", "b",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Stored [note]"));
    let took = started.elapsed();

    drop(held);

    // Negative control: the child must actually have contended. A fast run means
    // it locked a different path than the one held here, leaving the assertions
    // below vacuous.
    assert!(
        took >= LOCK_WAIT_BUDGET,
        "the child must wait out its {LOCK_WAIT_BUDGET:?} lock budget; it returned after \
         {took:?}, so it never contended on {}",
        notes_lock_path(repo.path()).display()
    );

    // The whole point: the entry is still there, written unlocked.
    let lines = spelunk_note_lines(repo.path());
    assert_eq!(
        lines.len(),
        1,
        "a contended carry must still write exactly one record; got: {lines:?}"
    );
    assert!(
        lines[0].contains(title),
        "the record must be the entry we added; got: {:?}",
        lines[0]
    );
}
