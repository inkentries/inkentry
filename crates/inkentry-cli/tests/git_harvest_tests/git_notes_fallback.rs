use crate::plumbing_helpers;
use plumbing_helpers::inkentry_bin_in;

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::Path;
use std::time::{Duration, Instant};
use tempfile::TempDir;

// Kept in sync with `fail_closed_no_project.rs`.
const NO_PROJECT_NO_REPO_ERR: &str = "no inkentry project here, and not inside a git repo. Run 'inkentry init' first, \
     or run inside a git repository.";

// Matches only the single-hatch message: the dual-hatch text splices ", and not inside a git
// repo" between "here" and ". Run".
const NO_PROJECT_ERR: &str = "no inkentry project here. Run 'inkentry init' first";

fn bin(home: &Path, cwd: &Path) -> Command {
    let mut cmd = inkentry_bin_in(home);
    cmd.current_dir(cwd)
        .env("INKENTRY_NO_SERVER", "1")
        .env_remove("INKENTRY_SERVER_URL");
    cmd
}

fn global_memory_db(home: &Path) -> std::path::PathBuf {
    home.join(".config").join("inkentry").join("memory.db")
}

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

// A missing ref is a legitimate empty result, so exit status is ignored.
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

// `user.*` goes in the local config: the spawned `inkentry` does not inherit the test's `GIT_*` identity env.
fn init_git_repo_with_commit(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.name", "t"]);
    git(dir, &["config", "user.email", "t@example.com"]);
    std::fs::write(dir.join("f.txt"), "x\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

fn inkentry_note_lines(dir: &Path) -> Vec<String> {
    let blob = git_stdout(dir, &["notes", "--ref=inkentry", "show", "HEAD"]);
    blob.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

#[test]
fn memory_add_list_round_trips_via_git_notes_fallback() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());

    let title = "fallback-roundtrip-abc123";

    bin(home.path(), repo.path())
        .args([
            "memory", "add", "--kind", "note", "--title", title, "--body", "b",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Stored [note]"));

    let note_blob = git_stdout(repo.path(), &["notes", "--ref=inkentry", "show", "HEAD"]);
    assert!(
        note_blob.contains(title),
        "the note on HEAD must contain the added entry's title; got: {note_blob:?}"
    );
    let list = git_stdout(repo.path(), &["notes", "--ref=inkentry", "list"]);
    assert_eq!(
        list.lines().filter(|l| !l.trim().is_empty()).count(),
        1,
        "exactly one commit (HEAD) should carry a inkentry note; got: {list:?}"
    );

    bin(home.path(), repo.path())
        .args(["memory", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains(title));

    assert!(
        !repo.path().join(".inkentry").exists(),
        "git-notes fallback must not create a local .inkentry/ project"
    );
    assert!(
        !global_memory_db(home.path()).exists(),
        "git-notes fallback must not create the machine-global memory store"
    );
}

#[test]
fn single_add_writes_exactly_one_note_record() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());

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

    let lines = inkentry_note_lines(repo.path());
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

#[test]
fn pre_init_and_post_init_records_have_identical_shape() {
    let home = TempDir::new().unwrap();

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
    let pre_lines = inkentry_note_lines(pre.path());
    assert_eq!(pre_lines.len(), 1, "pre-init add writes one record");

    // Creating `.inkentry/` is enough for `require_project_db` to resolve the project.
    let post = TempDir::new().unwrap();
    init_git_repo_with_commit(post.path());
    std::fs::create_dir_all(post.path().join(".inkentry")).unwrap();
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
    let post_lines = inkentry_note_lines(post.path());
    assert_eq!(
        post_lines.len(),
        1,
        "post-init write-through writes one record"
    );

    let pre_keys = json_top_level_keys(&pre_lines[0]);
    let post_keys = json_top_level_keys(&post_lines[0]);

    // Guards against a vacuous match on empty key sets; only the always-present core is listed, since serde omits `None` fields.
    for expected in [
        "body",
        "created_at",
        "entity_id",
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

fn record_field(line: &str, key: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(line).expect("record parses as JSON");
    v.get(key)
        .unwrap_or_else(|| panic!("record has no {key:?}: {line}"))
        .to_string()
        .trim_matches('"')
        .to_string()
}

// A git-notes record's own `id` is a per-write stamp that never resolves against the store.
fn local_id_for_title(home: &Path, repo: &Path, title: &str) -> String {
    let out = bin(home, repo)
        .args(["memory", "list", "--format", "jsonl", "--limit", "100"])
        .output()
        .expect("spawn inkentry memory list");
    assert!(
        out.status.success(),
        "memory list failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    stdout
        .lines()
        .find_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            (v.get("title")?.as_str()? == title)
                .then(|| Some(v.get("id")?.as_str()?.to_string()))
                .flatten()
        })
        .unwrap_or_else(|| panic!("no local entry titled {title:?} in:\n{stdout}"))
}

// A record's `id` is a per-write stamp, not identity; `entity_id`s must still differ across a re-init.
#[test]
fn reinit_between_adds_yields_distinct_entity_ids() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());

    let add = |title: &str, body: &str| {
        bin(home.path(), repo.path())
            .args([
                "memory", "add", "--kind", "decision", "--title", title, "--body", body,
            ])
            .assert()
            .success();
    };

    std::fs::create_dir_all(repo.path().join(".inkentry")).unwrap();
    add("first decision", "body one");

    std::fs::remove_dir_all(repo.path().join(".inkentry")).unwrap();
    std::fs::create_dir_all(repo.path().join(".inkentry")).unwrap();
    add("second decision", "body two");

    let lines = inkentry_note_lines(repo.path());
    assert_eq!(lines.len(), 2, "both adds carried into the notes ref");

    let first = record_field(&lines[0], "entity_id");
    let second = record_field(&lines[1], "entity_id");
    assert_ne!(
        first, second,
        "two different decisions must have distinct entity_ids across the re-init"
    );
    assert_eq!(first.len(), 64, "entity_id is hex sha256: {first}");
    assert_eq!(second.len(), 64, "entity_id is hex sha256: {second}");
}

#[test]
fn entity_id_is_stable_across_stores() {
    let home = TempDir::new().unwrap();

    let entity_id_for = |title: &str, seed_extra: bool| -> String {
        let repo = TempDir::new().unwrap();
        init_git_repo_with_commit(repo.path());
        std::fs::create_dir_all(repo.path().join(".inkentry")).unwrap();
        // Advance the second store's rowid counter so the two entries cannot share a rowid.
        if seed_extra {
            for i in 0..3 {
                bin(home.path(), repo.path())
                    .args([
                        "memory",
                        "add",
                        "--kind",
                        "note",
                        "--title",
                        &format!("filler {i}"),
                        "--body",
                        "filler",
                    ])
                    .assert()
                    .success();
            }
        }
        bin(home.path(), repo.path())
            .args([
                "memory",
                "add",
                "--kind",
                "decision",
                "--title",
                title,
                "--body",
                "shared body",
            ])
            .assert()
            .success();
        let lines = inkentry_note_lines(repo.path());
        let last = lines.last().expect("at least one record");
        record_field(last, "entity_id")
    };

    assert_eq!(
        entity_id_for("portable", false),
        entity_id_for("portable", true),
        "entity_id must not depend on the store's rowid or write time"
    );
}

#[test]
fn memory_add_refuses_in_git_repo_without_any_commit() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    // No commit means HEAD is unresolvable, so the fallback cannot attach a note.
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
        inkentry_note_lines(repo.path()).is_empty(),
        "a refused add in an empty repo must not write any inkentry note"
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

#[test]
fn local_dot_inkentry_takes_precedence_over_git_notes_fallback() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());
    std::fs::create_dir_all(repo.path().join(".inkentry")).unwrap();

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

    assert!(
        repo.path().join(".inkentry").join("memory.db").exists(),
        "with a local .inkentry/, add must write sqlite, not fall back to git-notes"
    );

    bin(home.path(), repo.path())
        .args(["memory", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("sqlite-wins"));

    assert!(!global_memory_db(home.path()).exists());
}

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

    let note_blob = git_stdout(repo.path(), &["notes", "--ref=inkentry", "show", "HEAD"]);
    assert!(
        note_blob.contains(title),
        "explicit git-notes add must write the note; got: {note_blob:?}"
    );

    let lines = inkentry_note_lines(repo.path());
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

    assert!(!repo.path().join(".inkentry").exists());
    assert!(!global_memory_db(home.path()).exists());
}

#[test]
fn secret_in_entry_is_refused_and_leaves_git_notes_untouched() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());

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
        inkentry_note_lines(repo.path()).is_empty(),
        "a secret-blocked add must leave refs/notes/inkentry absent/unmodified"
    );
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
        inkentry_note_lines(repo.path()).is_empty(),
        "a body-secret-blocked add must also leave the note ref untouched"
    );
}

#[test]
fn non_add_list_subcommands_stay_fail_closed_inside_git_repo() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());

    let invocations: [&[&str]; 3] = [
        &["memory", "show", "1"],
        &["memory", "timeline", "anything"],
        &["memory", "supersede", "1", "2"],
    ];
    for args in invocations {
        bin(home.path(), repo.path())
            .args(args)
            .assert()
            .failure()
            .stderr(predicate::str::contains(NO_PROJECT_ERR))
            .stderr(predicate::str::contains("not inside a git repo").not());
    }

    assert!(
        inkentry_note_lines(repo.path()).is_empty(),
        "a fail-closed non-add/list subcommand must not write any inkentry note"
    );
    assert!(
        !global_memory_db(home.path()).exists(),
        "a fail-closed subcommand must not create the machine-global store"
    );
}

#[test]
fn post_init_add_writes_sqlite_primary_and_git_notes_write_through() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());
    std::fs::create_dir_all(repo.path().join(".inkentry")).unwrap();

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

    assert!(
        repo.path().join(".inkentry").join("memory.db").exists(),
        "post-init add must write the local SQLite primary"
    );

    let lines = inkentry_note_lines(repo.path());
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

    bin(home.path(), repo.path())
        .args(["memory", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("post-init-both"));

    assert!(!global_memory_db(home.path()).exists());
}

// Pre-init the carrier is the sole writer, so a failed carry must exit non-zero. The failure is forced by
// leaving `git notes add` no committer identity: no local `user.*`, no global/system config, and
// `user.useConfigOnly` so git cannot derive USER@host. `rev-parse HEAD` needs none, so the carrier still engages.
#[test]
fn failed_pre_init_carry_is_fatal_and_writes_nothing() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();

    // The setup helper supplies identity via env for the commit only, so `.git/config` has no `user.*`.
    git(repo.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(repo.path().join("f.txt"), "x\n").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-q", "-m", "init"]);

    let mut cmd = inkentry_bin_in(home.path());
    cmd.current_dir(repo.path())
        .env("INKENTRY_NO_SERVER", "1")
        .env_remove("INKENTRY_SERVER_URL")
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
        .stdout(predicate::str::contains("Stored").not())
        .stderr(predicate::str::contains(
            "recording memory entry to git notes",
        ));

    assert!(
        inkentry_note_lines(repo.path()).is_empty(),
        "a fatal failed carry must not leave a partial inkentry note"
    );
    assert!(
        !global_memory_db(home.path()).exists(),
        "a fatal failed carry must not create the machine-global store"
    );
}

// Mirrors `LOCK_WAIT_BUDGET` in `storage/git_notes/lock.rs`.
const LOCK_WAIT_BUDGET: Duration = Duration::from_secs(5);

// Resolved as production does, canonicalization included.
fn notes_lock_path(repo: &Path) -> std::path::PathBuf {
    let raw = git_stdout(repo, &["rev-parse", "--git-common-dir"]);
    let raw = raw.trim();
    let raw = Path::new(raw);
    let common_dir = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        repo.join(raw)
    };
    let common_dir = std::fs::canonicalize(&common_dir).unwrap_or(common_dir);
    common_dir.join("inkentry-notes.lock")
}

// Holding the lock across the child's whole run guarantees it exhausts its budget.
#[test]
fn contended_notes_lock_fails_the_pre_init_carry_and_writes_nothing() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());

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
            "memory",
            "add",
            "--kind",
            "note",
            "--title",
            "contended-lock-must-not-store",
            "--body",
            "b",
        ])
        .assert()
        .failure()
        .stdout(predicate::str::contains("Stored").not())
        .stderr(predicate::str::contains("notes lock").and(predicate::str::contains("Retry")));
    let took = started.elapsed();

    drop(held);

    // Negative control: a fast return means the child locked a different path, leaving the assertions below vacuous.
    assert!(
        took >= LOCK_WAIT_BUDGET,
        "the child must wait out its {LOCK_WAIT_BUDGET:?} lock budget; it returned after \
         {took:?}, so it never contended on {}",
        notes_lock_path(repo.path()).display()
    );

    let lines = inkentry_note_lines(repo.path());
    assert!(
        lines.is_empty(),
        "a contended carry must write nothing; got: {lines:?}"
    );
}

// A directory planted at the lock path makes the open fail deterministically on every platform.
#[test]
fn unusable_notes_lock_degradation_is_visible_without_rust_log() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());

    std::fs::create_dir_all(notes_lock_path(repo.path()))
        .expect("plant a directory at the notes lock path");

    bin(home.path(), repo.path())
        .env_remove("RUST_LOG")
        .args([
            "memory",
            "add",
            "--kind",
            "note",
            "--title",
            "unusable-lock-still-stores",
            "--body",
            "b",
        ])
        .assert()
        // Failing every write on a lock-hostile filesystem would make inkentry unusable there.
        .success()
        .stdout(predicate::str::contains("Stored [note]"))
        .stderr(predicate::str::contains("without the cross-process lock"));

    let lines = inkentry_note_lines(repo.path());
    assert!(
        lines.len() == 1 && lines[0].contains("unusable-lock-still-stores"),
        "the degraded write must still land exactly one record; got: {lines:?}"
    );
}

fn title_and_status(line: &str) -> (String, String) {
    (record_field(line, "title"), record_field(line, "status"))
}

#[test]
fn post_init_add_supersedes_carries_edge_for_old_entry() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());
    std::fs::create_dir_all(repo.path().join(".inkentry")).unwrap();

    bin(home.path(), repo.path())
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "old-decision",
            "--body",
            "b1",
        ])
        .assert()
        .success();
    let old_lines = inkentry_note_lines(repo.path());
    assert_eq!(old_lines.len(), 1, "setup: OLD's own add");
    let old_id = local_id_for_title(home.path(), repo.path(), "old-decision");
    let old_entity_id = record_field(&old_lines[0], "entity_id");

    bin(home.path(), repo.path())
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "new-decision",
            "--body",
            "b2",
            "--supersedes",
            &old_id,
        ])
        .assert()
        .success();

    let lines = inkentry_note_lines(repo.path());
    assert_eq!(
        lines.len(),
        3,
        "OLD's untouched original, NEW's record, and OLD's state-update; got: {lines:?}"
    );

    let new_line = lines
        .iter()
        .find(|l| title_and_status(l) == ("new-decision".to_string(), "active".to_string()))
        .unwrap_or_else(|| panic!("no active new-decision record in {lines:?}"));
    let new_entity_id = record_field(new_line, "entity_id");
    assert!(
        !new_line.contains("superseded_by_entity_id"),
        "the edge must never land on NEW's record; got: {new_line}"
    );

    let old_original = lines
        .iter()
        .find(|l| title_and_status(l) == ("old-decision".to_string(), "active".to_string()))
        .unwrap_or_else(|| {
            panic!("OLD's original active record must survive untouched: {lines:?}")
        });
    assert_eq!(
        record_field(old_original, "entity_id"),
        old_entity_id,
        "OLD's original record must be byte-identical in identity, never rewritten"
    );

    let old_update = lines
        .iter()
        .find(|l| title_and_status(l) == ("old-decision".to_string(), "archived".to_string()))
        .unwrap_or_else(|| panic!("OLD's state-update record is missing: {lines:?}"));
    assert_eq!(
        record_field(old_update, "entity_id"),
        old_entity_id,
        "the state-update record must carry OLD's own entity_id, not NEW's"
    );
    assert_eq!(
        record_field(old_update, "superseded_by_entity_id"),
        new_entity_id,
        "the edge must point at NEW's entity_id"
    );
    assert!(
        !record_field(old_update, "invalid_at").is_empty(),
        "the state-update record must set invalid_at"
    );
}

#[test]
fn post_init_supersede_command_carries_edge_to_git_notes() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());
    std::fs::create_dir_all(repo.path().join(".inkentry")).unwrap();

    let add = |title: &str, body: &str| {
        bin(home.path(), repo.path())
            .args([
                "memory", "add", "--kind", "decision", "--title", title, "--body", body,
            ])
            .assert()
            .success();
    };
    add("old-via-supersede", "b1");
    add("new-via-supersede", "b2");

    let seeded = inkentry_note_lines(repo.path());
    assert_eq!(seeded.len(), 2, "setup: two independent adds");
    let old_id = local_id_for_title(home.path(), repo.path(), "old-via-supersede");
    let new_id = local_id_for_title(home.path(), repo.path(), "new-via-supersede");
    let new_entity_id = record_field(&seeded[1], "entity_id");

    bin(home.path(), repo.path())
        .args(["memory", "supersede", &old_id, &new_id])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("Archived #").and(predicate::str::contains("superseded by #")),
        );

    let lines = inkentry_note_lines(repo.path());
    assert_eq!(
        lines.len(),
        3,
        "OLD's untouched original, NEW's record, and OLD's state-update; got: {lines:?}"
    );

    let old_update = lines
        .iter()
        .find(|l| title_and_status(l) == ("old-via-supersede".to_string(), "archived".to_string()))
        .unwrap_or_else(|| panic!("OLD's state-update record is missing: {lines:?}"));
    assert_eq!(
        record_field(old_update, "superseded_by_entity_id"),
        new_entity_id,
        "the edge must point at NEW's entity_id"
    );

    let new_line = lines
        .iter()
        .find(|l| title_and_status(l) == ("new-via-supersede".to_string(), "active".to_string()))
        .unwrap_or_else(|| panic!("NEW's record is missing: {lines:?}"));
    assert!(
        !new_line.contains("superseded_by_entity_id"),
        "the edge must never land on NEW's record; got: {new_line}"
    );
}

#[test]
fn pre_init_add_supersedes_carries_edge_for_old_entry() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());

    bin(home.path(), repo.path())
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "pre-init-old",
            "--body",
            "b1",
        ])
        .assert()
        .success();
    let old_lines = inkentry_note_lines(repo.path());
    assert_eq!(old_lines.len(), 1, "setup: OLD's own pre-init add");
    let old_id = record_field(&old_lines[0], "id");

    bin(home.path(), repo.path())
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "pre-init-new",
            "--body",
            "b2",
            "--supersedes",
            &old_id,
        ])
        .assert()
        .success();

    let lines = inkentry_note_lines(repo.path());
    assert_eq!(
        lines.len(),
        3,
        "expected OLD's untouched original, NEW's record, and a state-update \
         archiving OLD (mirroring the post-init behaviour proven above); got \
         only {lines:?} — pre-init, the `--supersedes` edge is being \
         silently dropped while the command still reports plain success"
    );
    assert!(
        lines
            .iter()
            .any(|l| title_and_status(l) == ("pre-init-old".to_string(), "archived".to_string())),
        "OLD must gain an archived state-update record even pre-init; got: {lines:?}"
    );
}

#[test]
fn post_init_add_supersedes_rejects_already_archived_old() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());
    std::fs::create_dir_all(repo.path().join(".inkentry")).unwrap();

    bin(home.path(), repo.path())
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "old-decision",
            "--body",
            "b1",
        ])
        .assert()
        .success();
    let old_lines = inkentry_note_lines(repo.path());
    assert_eq!(old_lines.len(), 1, "setup: OLD's own add");
    let old_id = local_id_for_title(home.path(), repo.path(), "old-decision");

    bin(home.path(), repo.path())
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "successor-a",
            "--body",
            "b2",
            "--supersedes",
            &old_id,
        ])
        .assert()
        .success();

    let lines_after_first_supersede = inkentry_note_lines(repo.path());
    assert_eq!(
        lines_after_first_supersede.len(),
        3,
        "setup: OLD's original, successor A's record, OLD's state-update"
    );

    bin(home.path(), repo.path())
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "successor-b",
            "--body",
            "b3",
            "--supersedes",
            &old_id,
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(format!(
            "No active memory entry with id {old_id} (old)"
        )));

    let list_output = bin(home.path(), repo.path())
        .args([
            "memory",
            "list",
            "--format",
            "jsonl",
            "--archived",
            "--limit",
            "100",
        ])
        .output()
        .unwrap();
    assert!(list_output.status.success());
    let stdout = String::from_utf8_lossy(&list_output.stdout);
    let entry_count = stdout.lines().filter(|l| !l.trim().is_empty()).count();
    assert_eq!(
        entry_count, 2,
        "a rejected --supersedes must not create an orphaned new note row; got: {stdout}"
    );

    let lines_after_rejected_supersede = inkentry_note_lines(repo.path());
    assert_eq!(
        lines_after_rejected_supersede.len(),
        3,
        "a rejected --supersedes must write neither a new-entry record nor a \
         second conflicting state-update for OLD; got: {lines_after_rejected_supersede:?}"
    );
}

#[test]
fn pre_init_add_supersedes_rejects_already_archived_old() {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    init_git_repo_with_commit(repo.path());

    bin(home.path(), repo.path())
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "pre-init-old",
            "--body",
            "b1",
        ])
        .assert()
        .success();
    let old_lines = inkentry_note_lines(repo.path());
    let old_id = record_field(&old_lines[0], "id");

    bin(home.path(), repo.path())
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "pre-init-successor-a",
            "--body",
            "b2",
            "--supersedes",
            &old_id,
        ])
        .assert()
        .success();

    let lines_after_first_supersede = inkentry_note_lines(repo.path());
    assert_eq!(
        lines_after_first_supersede.len(),
        3,
        "setup: OLD's original, successor A's record, OLD's state-update"
    );

    bin(home.path(), repo.path())
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "pre-init-successor-b",
            "--body",
            "b3",
            "--supersedes",
            &old_id,
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(format!(
            "No active memory entry with id {old_id} (old)"
        )));

    let lines_after_rejected_supersede = inkentry_note_lines(repo.path());
    assert_eq!(
        lines_after_rejected_supersede.len(),
        3,
        "a rejected pre-init --supersedes must not write successor B's own \
         record, nor a second state-update for OLD; got: {lines_after_rejected_supersede:?}"
    );
    assert!(
        !lines_after_rejected_supersede
            .iter()
            .any(|l| l.contains("pre-init-successor-b")),
        "successor B's record must never be written when the pre-flight check \
         rejects the supersede; got: {lines_after_rejected_supersede:?}"
    );
}
