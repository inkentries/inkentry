// Runs across two real clones of a shared origin: only a clone that must MERGE (not
// fast-forward) can tell an append from an in-place rewrite.
use crate::plumbing_helpers;
use plumbing_helpers::inkentry_bin_in;

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const TRACKING_REF: &str = "refs/notes/origin/inkentry";

fn git(dir: &Path, args: &[&str]) {
    let out = git_out(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_out(dir: &Path, args: &[&str]) -> std::process::Output {
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

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    String::from_utf8_lossy(&git_out(dir, args).stdout).into_owned()
}

fn fetch_notes(dir: &Path) {
    git(
        dir,
        &[
            "fetch",
            "-q",
            "origin",
            &format!("refs/notes/inkentry:{TRACKING_REF}"),
        ],
    );
}

fn init_repo_with_commit(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    std::fs::write(dir.join("f.txt"), "x\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

fn bin(home: &Path, cwd: &Path) -> Command {
    let mut cmd = inkentry_bin_in(home);
    cmd.current_dir(cwd)
        .env("INKENTRY_NO_SERVER", "1")
        .env_remove("INKENTRY_SERVER_URL");
    cmd
}

fn empty_config(dir: &Path) -> PathBuf {
    let cfg = dir.join("config.toml");
    std::fs::write(&cfg, "").unwrap();
    cfg
}

fn run_init(dir: &Path) -> String {
    let cfg = empty_config(dir);
    let out = inkentry_bin_in(dir)
        .current_dir(dir)
        .env("INKENTRY_NO_SERVER", "1")
        .arg("--config")
        .arg(&cfg)
        .args(["init", "--no-index"])
        .output()
        .expect("spawn inkentry init");
    assert!(
        out.status.success(),
        "inkentry init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn inkentry_note_lines(dir: &Path) -> Vec<String> {
    let blob = git_stdout(dir, &["notes", "--ref=inkentry", "show", "HEAD"]);
    blob.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

fn record_field(line: &str, key: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(line).expect("record parses as JSON");
    v.get(key)
        .unwrap_or_else(|| panic!("record has no {key:?}: {line}"))
        .to_string()
        .trim_matches('"')
        .to_string()
}

// Differs from the `id` on a git-notes record: each clone mints its own id for the same entity.
fn local_id_for_title(home: &Path, dir: &Path, title: &str) -> String {
    let out = bin(home, dir)
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

// `.inkentry/` in both clones makes `memory add` take the SQLite-primary-plus-carrier
// path, not the pre-init fallback.
fn setup_origin_with_two_clones(tmp: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let origin = tmp.join("origin.git");
    git(
        tmp,
        &[
            "init",
            "--bare",
            "-q",
            "-b",
            "main",
            origin.to_str().unwrap(),
        ],
    );

    let a = tmp.join("a");
    std::fs::create_dir_all(&a).unwrap();
    init_repo_with_commit(&a);
    git(&a, &["remote", "add", "origin", origin.to_str().unwrap()]);
    git(&a, &["push", "-q", "-u", "origin", "main"]);
    std::fs::create_dir_all(a.join(".inkentry")).unwrap();

    let b = tmp.join("b");
    git(
        tmp,
        &["clone", "-q", origin.to_str().unwrap(), b.to_str().unwrap()],
    );
    git(&b, &["config", "user.email", "b@example.com"]);
    git(&b, &["config", "user.name", "B"]);
    std::fs::create_dir_all(b.join(".inkentry")).unwrap();

    (origin, a, b)
}

// B holds a divergent local note, so the merge unions two histories instead of
// fast-forwarding; otherwise an in-place rewrite of X's status would look correct too.
#[test]
fn two_clone_archive_travels_and_folds_to_archived_once_despite_divergent_note() {
    let tmp = TempDir::new().unwrap();
    let home_a = TempDir::new().unwrap();
    let home_b = TempDir::new().unwrap();
    let (_origin, a, b) = setup_origin_with_two_clones(tmp.path());

    bin(home_a.path(), &a)
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "clone-a-archives-me",
            "--body",
            "b1",
        ])
        .assert()
        .success();
    let seeded = inkentry_note_lines(&a);
    assert_eq!(seeded.len(), 1, "setup: A's own add");
    let x_id = local_id_for_title(home_a.path(), &a, "clone-a-archives-me");
    git(&a, &["push", "-q", "origin", "refs/notes/inkentry"]);

    // B adopts X before diverging, so this merge is a plain fast-forward.
    fetch_notes(&b);
    bin(home_b.path(), &b)
        .args(["memory", "--backend", "git-notes", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("clone-a-archives-me"));

    bin(home_b.path(), &b)
        .args([
            "memory",
            "add",
            "--kind",
            "note",
            "--title",
            "clone-b-local-only",
            "--body",
            "b2",
        ])
        .assert()
        .success();

    bin(home_a.path(), &a)
        .args(["memory", "archive", &x_id])
        .assert()
        .success()
        .stdout(predicate::str::contains("Archived memory entry"));
    git(&a, &["push", "-q", "origin", "refs/notes/inkentry"]);

    fetch_notes(&b);

    let default_list = bin(home_b.path(), &b)
        .args(["memory", "--backend", "git-notes", "list"])
        .output()
        .expect("spawn inkentry memory list");
    assert!(default_list.status.success());
    let default_stdout = String::from_utf8_lossy(&default_list.stdout);
    assert!(
        !default_stdout.contains("clone-a-archives-me"),
        "archived X must not appear in a default list, got:\n{default_stdout}"
    );
    assert!(
        default_stdout.contains("clone-b-local-only"),
        "the union must not drop B's own divergent entry, got:\n{default_stdout}"
    );

    // Exactly once: the archived update must fold onto X's original, not sit beside it.
    let archived_list = bin(home_b.path(), &b)
        .args(["memory", "--backend", "git-notes", "list", "--archived"])
        .output()
        .expect("spawn inkentry memory list --archived");
    let archived_stdout = String::from_utf8_lossy(&archived_list.stdout);
    assert_eq!(
        archived_stdout.matches("clone-a-archives-me").count(),
        1,
        "X must fold to exactly one entry, got:\n{archived_stdout}"
    );
    assert!(
        archived_stdout.contains("[archived]"),
        "X's single folded copy must be marked archived, got:\n{archived_stdout}"
    );

    // Also assert on the raw ref: the fold could hide a rewrite (one line) or an
    // over-eager append (three-plus). Exactly two lines carry X's entity_id: the untouched
    // active original and the appended archived update.
    let raw_lines = inkentry_note_lines(&b);
    let x_entity_id = record_field(&seeded[0], "entity_id");
    let x_lines: Vec<&String> = raw_lines
        .iter()
        .filter(|l| record_field(l, "entity_id") == x_entity_id)
        .collect();
    assert_eq!(
        x_lines.len(),
        2,
        "X must carry exactly two raw lines after the merge (original + \
         appended state-update); a rewrite collapses to one, an unbounded \
         re-append would exceed two, got:\n{raw_lines:?}"
    );
    assert!(
        x_lines
            .iter()
            .any(|l| record_field(l, "status") == "active"),
        "the original active line must survive byte-for-byte on the raw ref, got:\n{raw_lines:?}"
    );
    assert!(
        x_lines
            .iter()
            .any(|l| record_field(l, "status") == "archived"),
        "the appended state-update line must be present on the raw ref, got:\n{raw_lines:?}"
    );
}

#[test]
fn concurrent_archives_from_two_clones_fold_to_one_archived_entry() {
    let tmp = TempDir::new().unwrap();
    let home_a = TempDir::new().unwrap();
    let (_origin, a, b) = setup_origin_with_two_clones(tmp.path());

    bin(home_a.path(), &a)
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "double-archived-entry",
            "--body",
            "b1",
        ])
        .assert()
        .success();
    let seeded = inkentry_note_lines(&a);
    assert_eq!(seeded.len(), 1, "setup: A's own add");
    let a_id = local_id_for_title(home_a.path(), &a, "double-archived-entry");
    git(&a, &["push", "-q", "origin", "refs/notes/inkentry"]);

    // B needs its own SQLite copy of X to archive it through the normal command, hence a
    // real `init` import rather than the manual `.inkentry` mkdir.
    fetch_notes(&b);
    let init_stdout = run_init(&b);
    assert!(
        init_stdout.contains("imported 1 entries from git notes"),
        "setup: B must import X via init, got:\n{init_stdout}"
    );
    let b_id = local_id_for_title(&b, &b, "double-archived-entry");

    bin(&b, &b)
        .args(["memory", "archive", &b_id])
        .assert()
        .success();

    bin(home_a.path(), &a)
        .args(["memory", "archive", &a_id])
        .assert()
        .success();
    git(&a, &["push", "-q", "origin", "refs/notes/inkentry"]);

    fetch_notes(&b);

    let archived_list = bin(&b, &b)
        .args(["memory", "--backend", "git-notes", "list", "--archived"])
        .output()
        .expect("spawn inkentry memory list --archived");
    assert!(archived_list.status.success());
    let archived_stdout = String::from_utf8_lossy(&archived_list.stdout);
    assert_eq!(
        archived_stdout.matches("double-archived-entry").count(),
        1,
        "two independent archives of the same entity must fold to one entry, got:\n{archived_stdout}"
    );
    assert!(
        archived_stdout.contains("[archived]"),
        "the folded entry must be marked archived, got:\n{archived_stdout}"
    );
}

// The carrier write is best-effort: if `refs/notes` is unwritable, archive still
// succeeds and the SQLite primary still holds it.
#[cfg(unix)]
#[test]
fn carrier_write_failure_does_not_fail_the_sqlite_archive() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let dir = tmp.path().join("repo");
    std::fs::create_dir_all(&dir).unwrap();
    init_repo_with_commit(&dir);
    std::fs::create_dir_all(dir.join(".inkentry")).unwrap();

    bin(home.path(), &dir)
        .args([
            "memory",
            "add",
            "--kind",
            "note",
            "--title",
            "carrier-fail-probe",
            "--body",
            "b",
        ])
        .assert()
        .success();
    let id = local_id_for_title(home.path(), &dir, "carrier-fail-probe");

    let refs_notes = dir.join(".git/refs/notes");
    let original = std::fs::metadata(&refs_notes).unwrap().permissions();
    let mut read_only = original.clone();
    read_only.set_mode(0o555);
    std::fs::set_permissions(&refs_notes, read_only).unwrap();

    // Probe with raw git, never the code under test: root (or a mount ignoring the mode)
    // can still write there, leaving no failure to assert against.
    let enforced = !git_out(
        &dir,
        &["notes", "--ref=inkentry", "add", "-f", "-m", "x", "HEAD"],
    )
    .status
    .success();
    if !enforced {
        std::fs::set_permissions(&refs_notes, original).unwrap();
        return;
    }

    let out = bin(home.path(), &dir)
        .args(["memory", "archive", &id])
        .output()
        .expect("spawn inkentry memory archive");

    // Restore before asserting so a panic does not leave a read-only dir that `TempDir`
    // cannot clean up.
    std::fs::set_permissions(&refs_notes, original).unwrap();

    assert!(
        out.status.success(),
        "a failed git-notes carry must not fail the command, got:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("git-notes carry failed"),
        "the failure must surface as a warning, got:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let archived = bin(home.path(), &dir)
        .args([
            "memory",
            "list",
            "--archived",
            "--format",
            "jsonl",
            "--limit",
            "10",
        ])
        .output()
        .expect("spawn inkentry memory list --archived");
    assert!(
        String::from_utf8_lossy(&archived.stdout).contains("carrier-fail-probe"),
        "the SQLite primary must hold the archive even though the carrier failed, got:\n{}",
        String::from_utf8_lossy(&archived.stdout)
    );
}
