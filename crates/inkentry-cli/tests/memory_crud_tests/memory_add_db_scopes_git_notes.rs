use crate::plumbing_helpers;
use plumbing_helpers::{init_git_repo, inkentry_bin_in};

use assert_cmd::Command;
use std::path::Path;
use tempfile::TempDir;

fn git_out(dir: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git")
}

fn inkentry_note_lines(dir: &Path) -> Option<Vec<String>> {
    let out = git_out(dir, &["notes", "--ref=inkentry", "show", "HEAD"]);
    if !out.status.success() {
        return None;
    }
    let blob = String::from_utf8_lossy(&out.stdout);
    Some(
        blob.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

fn record_field(line: &str, key: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(line).expect("record parses as JSON");
    v.get(key)
        .unwrap_or_else(|| panic!("record has no {key:?}: {line}"))
        .to_string()
        .trim_matches('"')
        .to_string()
}

fn bin(home: &Path, cwd: &Path) -> Command {
    let mut cmd = inkentry_bin_in(home);
    cmd.current_dir(cwd)
        .env("INKENTRY_NO_SERVER", "1")
        .env_remove("INKENTRY_SERVER_URL");
    cmd
}

// A `--db` target with no git repo of its own must never fall back to the CWD repo's notes: fixture
// seeding points `--db` at a tmpdir while inheriting the developer's checkout as CWD.
#[test]
fn db_target_outside_any_repo_never_writes_cwd_repos_notes() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();

    let host_repo = tmp.path().join("host_repo");
    std::fs::create_dir_all(&host_repo).unwrap();
    init_git_repo(&host_repo);

    let db_target = tmp.path().join("db_target");
    std::fs::create_dir_all(&db_target).unwrap();

    bin(home.path(), &host_repo)
        .arg("memory")
        .arg("--db")
        .arg(db_target.join("memory.db"))
        .arg("add")
        .arg("--kind")
        .arg("note")
        .arg("--title")
        .arg("should-not-pollute-host")
        .arg("--body")
        .arg("b")
        .assert()
        .success();

    assert!(
        inkentry_note_lines(&host_repo).is_none(),
        "the host repo's refs/notes/inkentry must stay untouched when --db \
         points outside it"
    );
}

#[test]
fn db_target_inside_its_own_repo_writes_there_not_cwd_repo() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();

    let host_repo = tmp.path().join("host_repo");
    std::fs::create_dir_all(&host_repo).unwrap();
    init_git_repo(&host_repo);

    let project_repo = tmp.path().join("project_repo");
    std::fs::create_dir_all(&project_repo).unwrap();
    init_git_repo(&project_repo);

    bin(home.path(), &host_repo)
        .arg("memory")
        .arg("--db")
        .arg(project_repo.join("memory.db"))
        .arg("add")
        .arg("--kind")
        .arg("note")
        .arg("--title")
        .arg("goes-to-project-repo")
        .arg("--body")
        .arg("b")
        .assert()
        .success();

    assert!(
        inkentry_note_lines(&host_repo).is_none(),
        "the CWD repo must not receive the note"
    );
    let project_lines = inkentry_note_lines(&project_repo)
        .expect("the --db target's own repo must receive the note");
    assert_eq!(project_lines.len(), 1);
    assert_eq!(
        record_field(&project_lines[0], "title"),
        "goes-to-project-repo"
    );
}

// A git-notes record's own `id` never resolves against the store.
fn local_id_for_title(home: &Path, cwd: &Path, db_path: &Path, title: &str) -> String {
    let out = bin(home, cwd)
        .arg("memory")
        .arg("--db")
        .arg(db_path)
        .args(["list", "--format", "jsonl", "--limit", "100"])
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

#[test]
fn supersedes_state_update_also_scoped_to_db_target_repo() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();

    let host_repo = tmp.path().join("host_repo");
    std::fs::create_dir_all(&host_repo).unwrap();
    init_git_repo(&host_repo);

    let project_repo = tmp.path().join("project_repo");
    std::fs::create_dir_all(&project_repo).unwrap();
    init_git_repo(&project_repo);
    let db_path = project_repo.join("memory.db");

    // Both adds run from host_repo: only `--db` should decide where the carrier lands.
    bin(home.path(), &host_repo)
        .arg("memory")
        .arg("--db")
        .arg(&db_path)
        .arg("add")
        .arg("--kind")
        .arg("decision")
        .arg("--title")
        .arg("old-entry")
        .arg("--body")
        .arg("b1")
        .assert()
        .success();
    let old_lines = inkentry_note_lines(&project_repo).expect("first add wrote a note");
    assert_eq!(old_lines.len(), 1);
    let old_id = local_id_for_title(home.path(), &host_repo, &db_path, "old-entry");
    let old_entity_id = record_field(&old_lines[0], "entity_id");

    bin(home.path(), &host_repo)
        .arg("memory")
        .arg("--db")
        .arg(&db_path)
        .arg("add")
        .arg("--kind")
        .arg("decision")
        .arg("--title")
        .arg("new-entry")
        .arg("--body")
        .arg("b2")
        .arg("--supersedes")
        .arg(&old_id)
        .assert()
        .success();

    assert!(
        inkentry_note_lines(&host_repo).is_none(),
        "the CWD repo must not receive the new record or the supersede \
         state-update"
    );
    let project_lines = inkentry_note_lines(&project_repo)
        .expect("the --db target's own repo must hold every record");
    assert_eq!(
        project_lines.len(),
        3,
        "expected OLD's original active record, NEW's record, and OLD's \
         archived state-update, got:\n{project_lines:?}"
    );
    assert!(
        project_lines
            .iter()
            .any(|l| record_field(l, "entity_id") == old_entity_id
                && record_field(l, "status") == "archived"),
        "OLD's state-update (status=archived, same entity_id) must be present, got:\n{project_lines:?}"
    );
}

#[test]
fn pre_init_add_with_no_local_project_uses_cwd_repo() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();

    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);

    bin(home.path(), &repo)
        .arg("memory")
        .arg("add")
        .arg("--kind")
        .arg("note")
        .arg("--title")
        .arg("pre-init-entry")
        .arg("--body")
        .arg("b")
        .assert()
        .success();

    let lines = inkentry_note_lines(&repo).expect("pre-init add must write to CWD's repo");
    assert_eq!(lines.len(), 1);
    assert_eq!(record_field(&lines[0], "title"), "pre-init-entry");
}

#[test]
fn pre_init_supersedes_reads_and_writes_cwd_repo() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();

    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);

    bin(home.path(), &repo)
        .arg("memory")
        .arg("add")
        .arg("--kind")
        .arg("decision")
        .arg("--title")
        .arg("pre-init-old")
        .arg("--body")
        .arg("b1")
        .assert()
        .success();
    let old_lines = inkentry_note_lines(&repo).expect("first pre-init add wrote a note");
    assert_eq!(old_lines.len(), 1);
    let old_id = record_field(&old_lines[0], "id");
    let old_entity_id = record_field(&old_lines[0], "entity_id");

    bin(home.path(), &repo)
        .arg("memory")
        .arg("add")
        .arg("--kind")
        .arg("decision")
        .arg("--title")
        .arg("pre-init-new")
        .arg("--body")
        .arg("b2")
        .arg("--supersedes")
        .arg(&old_id)
        .assert()
        .success();

    let lines = inkentry_note_lines(&repo).expect("pre-init add must write to CWD's repo");
    assert_eq!(
        lines.len(),
        3,
        "expected OLD's original record, NEW's record, and OLD's archived \
         state-update, got:\n{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| record_field(l, "entity_id") == old_entity_id
                && record_field(l, "status") == "archived"),
        "OLD's state-update (status=archived, same entity_id), written via the \
         pre-init GitNotesBackend::with_root preflight read, must be present, \
         got:\n{lines:?}"
    );
}

// The target repo is nested inside the CWD repo rather than a sibling: git discovery must stop at the
// nearest `.git`, which unrelated sibling repos cannot distinguish from finding some other repo.
#[test]
fn db_target_nested_inside_cwd_repo_writes_there_not_cwd_repo() {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();

    let host_repo = tmp.path().join("host_repo");
    std::fs::create_dir_all(&host_repo).unwrap();
    init_git_repo(&host_repo);

    let nested_repo = host_repo.join("vendor").join("nested_repo");
    std::fs::create_dir_all(&nested_repo).unwrap();
    init_git_repo(&nested_repo);

    bin(home.path(), &host_repo)
        .arg("memory")
        .arg("--db")
        .arg(nested_repo.join("memory.db"))
        .arg("add")
        .arg("--kind")
        .arg("note")
        .arg("--title")
        .arg("goes-to-nested-repo")
        .arg("--body")
        .arg("b")
        .assert()
        .success();

    assert!(
        inkentry_note_lines(&host_repo).is_none(),
        "the outer host repo must not receive the note even though the --db \
         target's repo is nested inside it"
    );
    let nested_lines = inkentry_note_lines(&nested_repo)
        .expect("the --db target's own nested repo must receive the note");
    assert_eq!(nested_lines.len(), 1);
    assert_eq!(
        record_field(&nested_lines[0], "title"),
        "goes-to-nested-repo"
    );
}
