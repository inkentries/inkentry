// The portable handle on the command surface: what `memory list`, `memory show`
// and `context` display, and what `show`, `archive` and `supersede` accept.

use crate::plumbing_helpers;
use plumbing_helpers::{init_git_repo, inkentry_bin_in, write_config};

use assert_cmd::Command;
use inkentry_core::storage::entity_id;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

struct Project {
    tmp: TempDir,
    home: TempDir,
    repo: PathBuf,
    mem: PathBuf,
    cfg: PathBuf,
}

// A git repo with its own memory store, so the git-notes carrier writes into
// the fixture's repo and never the checkout the test runs from.
fn project() -> Project {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);
    let db = repo.join(".inkentry").join("inkentry.db");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    let mem = db.with_file_name("memory.db");
    let cfg = write_config(tmp.path(), &db, "http://127.0.0.1:1");
    Project {
        tmp,
        home,
        repo,
        mem,
        cfg,
    }
}

impl Project {
    fn cmd(&self) -> Command {
        let mut cmd = inkentry_bin_in(self.home.path());
        cmd.current_dir(&self.repo)
            .env("INKENTRY_NO_SERVER", "1")
            .env_remove("INKENTRY_SERVER_URL")
            .arg("--config")
            .arg(&self.cfg);
        cmd
    }

    fn memory(&self) -> Command {
        let mut cmd = self.cmd();
        cmd.arg("memory").arg("--db").arg(&self.mem);
        cmd
    }

    // Returns the entry's entity id, computed here from the text rather than
    // read back from the command under test.
    fn add(&self, title: &str, body: &str) -> String {
        self.memory()
            .args([
                "add", "--kind", "decision", "--title", title, "--body", body,
            ])
            .assert()
            .success();
        entity_id("decision", title, body)
    }

    fn list_json(&self) -> Vec<serde_json::Value> {
        let out = self
            .memory()
            .args(["list", "--format", "json", "--archived"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        serde_json::from_slice(&out).expect("list --format json parses")
    }

    fn entry(&self, title: &str) -> serde_json::Value {
        self.list_json()
            .into_iter()
            .find(|n| n["title"] == title)
            .unwrap_or_else(|| panic!("no entry titled {title:?}"))
    }

    fn uuid_of(&self, title: &str) -> String {
        self.entry(title)["id"].as_str().unwrap().to_string()
    }

    fn status_of(&self, title: &str) -> String {
        self.entry(title)["status"].as_str().unwrap().to_string()
    }

    fn show(&self, id: &str) -> assert_cmd::assert::Assert {
        self.memory().args(["show", id]).assert()
    }

    fn note_lines(&self) -> Vec<String> {
        note_lines_in(&self.repo)
    }
}

// Two entries sharing eight leading hex characters is a 32-bit coincidence no
// fixture can hash its way to, so the ambiguous pair is crafted in the store.
const CRAFT_COLLIDING_ENTITY_IDS: &str = "\
    UPDATE notes SET entity_id = \
      'aaaaaaaa11111111111111111111111111111111111111111111111111111111' \
      WHERE title = 'Twin one'; \
    UPDATE notes SET entity_id = \
      'aaaaaaaa22222222222222222222222222222222222222222222222222222222' \
      WHERE title = 'Twin two';";

fn note_lines_in(dir: &Path) -> Vec<String> {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(["notes", "--ref=inkentry", "show", "HEAD"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git");
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

fn handle(entity_id: &str) -> &str {
    &entity_id[..12]
}

fn stdout(assert: assert_cmd::assert::Assert) -> String {
    String::from_utf8(assert.get_output().stdout.clone()).expect("utf8 stdout")
}

// ── display ─────────────────────────────────────────────────────────────────

#[test]
fn list_leads_each_line_with_the_handle_and_not_the_local_id() {
    let p = project();
    let eid = p.add("Handle on the listing", "body of the listing entry");
    let uuid = p.uuid_of("Handle on the listing");

    let out = stdout(p.memory().arg("list").assert().success());
    assert!(
        out.contains(handle(&eid)),
        "list should lead with the handle {}, got:\n{out}",
        handle(&eid)
    );
    assert!(
        !out.contains(&uuid),
        "list should not print the per-machine id {uuid}, got:\n{out}"
    );
}

#[test]
fn list_json_and_jsonl_carry_the_full_entity_id_beside_the_local_id() {
    let p = project();
    let eid = p.add("Both ids in json", "body for the json entry");
    let uuid = p.uuid_of("Both ids in json");

    let entry = p.entry("Both ids in json");
    assert_eq!(entry["entity_id"].as_str().unwrap(), eid);
    assert_eq!(entry["entity_id"].as_str().unwrap().len(), 64);
    assert_eq!(entry["id"].as_str().unwrap(), uuid);

    let out = stdout(
        p.memory()
            .args(["list", "--format", "jsonl"])
            .assert()
            .success(),
    );
    let line: serde_json::Value =
        serde_json::from_str(out.lines().next().expect("one line")).expect("jsonl line parses");
    assert_eq!(line["entity_id"].as_str().unwrap(), eid);
    assert_eq!(line["id"].as_str().unwrap(), uuid);
}

#[test]
fn show_prints_the_handle_in_the_heading_and_both_ids_in_full() {
    let p = project();
    let eid = p.add("Both ids on show", "body for the show entry");
    let uuid = p.uuid_of("Both ids on show");

    let out = stdout(p.show(&uuid).success());
    let heading = out.lines().next().expect("heading");
    assert!(
        heading.contains(handle(&eid)) && !heading.contains(&uuid),
        "heading should carry the handle, not the local id: {heading}"
    );
    assert!(out.contains(&format!("entity_id:  {eid}")), "got:\n{out}");
    assert!(out.contains(&format!("id:         {uuid}")), "got:\n{out}");

    let json = stdout(
        p.memory()
            .args(["show", &uuid, "--format", "json"])
            .assert()
            .success(),
    );
    let note: serde_json::Value = serde_json::from_str(&json).expect("show json parses");
    assert_eq!(note["entity_id"].as_str().unwrap(), eid);
    assert_eq!(note["id"].as_str().unwrap(), uuid);
}

#[test]
fn context_shows_the_handle_and_carries_the_entity_id_in_json() {
    let p = project();
    let eid = p.add("Handle in context", "body for the context entry");
    let uuid = p.uuid_of("Handle in context");

    let out = stdout(
        p.cmd()
            .args(["context", "--db"])
            .arg(&p.mem)
            .arg("--no-conventions")
            .assert()
            .success(),
    );
    assert!(out.contains(handle(&eid)), "got:\n{out}");
    assert!(!out.contains(&uuid), "got:\n{out}");

    let json = stdout(
        p.cmd()
            .args(["context", "--db"])
            .arg(&p.mem)
            .args(["--no-conventions", "--format", "json"])
            .assert()
            .success(),
    );
    assert!(
        json.contains(&eid),
        "context json should carry the full entity id, got:\n{json}"
    );
}

#[test]
fn the_same_entry_shows_the_same_handle_on_both_backends() {
    let p = project();
    let eid = p.add("One handle everywhere", "body carried to the notes ref");

    let sqlite = stdout(p.memory().arg("list").assert().success());
    let carried = stdout(
        p.memory()
            .args(["list", "--backend", "git-notes"])
            .assert()
            .success(),
    );
    assert!(sqlite.contains(handle(&eid)), "got:\n{sqlite}");
    assert!(
        carried.contains(handle(&eid)),
        "the carrier listing should show the same handle, got:\n{carried}"
    );
    // The carrier's own record token is a small integer; it must not be
    // displayed where an id is expected.
    for token in 0..4 {
        let rendered = format!("#{token} ");
        assert!(
            !carried.contains(&rendered),
            "the carrier token should not appear as an id, got:\n{carried}"
        );
    }
}

// ── lookup ──────────────────────────────────────────────────────────────────

#[test]
fn show_resolves_the_local_id_the_full_entity_id_and_a_prefix_of_it() {
    let p = project();
    let eid = p.add(
        "Four ways to the same entry",
        "body of the resolvable entry",
    );
    let uuid = p.uuid_of("Four ways to the same entry");

    for token in [uuid.as_str(), eid.as_str(), &eid[..12], &eid[..8]] {
        let out = stdout(p.show(token).success());
        assert!(
            out.contains("Four ways to the same entry"),
            "token {token} should resolve, got:\n{out}"
        );
        assert!(out.contains(&format!("entity_id:  {eid}")), "token {token}");
    }
}

#[test]
fn a_prefix_shorter_than_the_floor_is_not_tried_as_a_handle() {
    let p = project();
    let eid = p.add("Below the floor", "body below the floor");

    let assert = p.show(&eid[..7]).failure();
    let err = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        err.contains(&format!("No memory entry with id {}", &eid[..7])),
        "got: {err}"
    );
    assert!(!stdout(assert).contains("Below the floor"));
}

#[test]
fn a_token_that_matches_nothing_keeps_the_existing_not_found_message() {
    let p = project();
    p.add("Present", "body of the present entry");

    let assert = p.show("0199a0f1-4d3c-7c2a-9b1e-6f0a2c5d8e33").failure();
    let err = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        err.contains("No memory entry with id 0199a0f1-4d3c-7c2a-9b1e-6f0a2c5d8e33"),
        "got: {err}"
    );
}

#[test]
fn archiving_by_a_handle_archives_that_entry_and_carries_the_state_update() {
    let p = project();
    let eid = p.add("Archived by handle", "body of the entry to archive");
    p.add("Left alone", "body of the entry to leave alone");

    let out = stdout(p.memory().args(["archive", &eid[..12]]).assert().success());
    assert!(out.contains(handle(&eid)), "got:\n{out}");

    assert_eq!(p.status_of("Archived by handle"), "archived");
    assert_eq!(p.status_of("Left alone"), "active");

    let carried = p.note_lines();
    assert!(
        carried.iter().any(|l| {
            let v: serde_json::Value = serde_json::from_str(l).expect("record parses");
            v["entity_id"] == eid && v["status"] == "archived"
        }),
        "the archive should still reach the notes ref, got:\n{carried:#?}"
    );
}

#[test]
fn superseding_by_handles_links_the_entries_and_carries_the_edge() {
    let p = project();
    let old = p.add("Superseded by handle", "body of the superseded entry");
    let new = p.add("Supersedes by handle", "body of the successor entry");

    p.memory()
        .args(["supersede", &old[..12], &new[..8]])
        .assert()
        .success();

    assert_eq!(p.status_of("Superseded by handle"), "archived");
    assert_eq!(
        p.entry("Superseded by handle")["superseded_by"]
            .as_str()
            .unwrap(),
        p.uuid_of("Supersedes by handle")
    );

    let carried = p.note_lines();
    assert!(
        carried.iter().any(|l| {
            let v: serde_json::Value = serde_json::from_str(l).expect("record parses");
            v["entity_id"] == old && v["superseded_by_entity_id"] == new
        }),
        "the supersede edge should still reach the notes ref, got:\n{carried:#?}"
    );
}

#[test]
fn an_ambiguous_handle_resolves_nothing_and_names_the_count() {
    plumbing_helpers::register_sqlite_vec();
    let p = project();
    p.add("Twin one", "body of the first twin");
    p.add("Twin two", "body of the second twin");
    let unrelated = p.add("Not a twin", "body of the unrelated entry");
    inkentry_core::storage::MemoryStore::open(&p.mem)
        .expect("open the fixture store")
        .execute_batch(CRAFT_COLLIDING_ENTITY_IDS)
        .expect("craft colliding entity ids");

    for args in [
        vec!["show", "aaaaaaaa"],
        vec!["archive", "aaaaaaaa"],
        vec!["supersede", "aaaaaaaa", &unrelated[..12]],
        vec!["supersede", &unrelated[..12], "aaaaaaaa"],
    ] {
        let assert = p.memory().args(&args).assert().failure();
        let err = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
        assert!(
            err.contains("'aaaaaaaa' matches 2 memory entries"),
            "{args:?} should name the prefix and the count, got: {err}"
        );
        assert!(
            !stdout(assert).contains("Twin"),
            "{args:?} should show no entry"
        );
    }

    assert_eq!(p.status_of("Twin one"), "active");
    assert_eq!(p.status_of("Twin two"), "active");

    // A longer prefix separates them again.
    let out = stdout(p.show("aaaaaaaa1111").success());
    assert!(out.contains("Twin one"), "got:\n{out}");
}

#[test]
fn a_numeric_token_still_says_entries_are_identified_by_uuid() {
    let p = project();
    p.add("Numbered no more", "body of the numbered entry");

    let assert = p.memory().args(["graph", "42"]).assert().failure();
    let err = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(err.contains("identified by UUID"), "got: {err}");
}

// ── across machines ─────────────────────────────────────────────────────────

fn git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed");
}

#[test]
fn a_clone_remints_the_local_id_and_keeps_the_entity_id() {
    let p = project();
    let first = p.add("Crosses the wire", "body that crosses the wire");
    let second = p.add("Crosses it too", "second body that crosses the wire");

    let origin = p.tmp.path().join("origin.git");
    git(
        p.tmp.path(),
        &["init", "-q", "--bare", origin.to_str().unwrap()],
    );
    git(
        &p.repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&p.repo, &["push", "-q", "origin", "HEAD"]);
    git(
        &p.repo,
        &[
            "push",
            "-q",
            "origin",
            "refs/notes/inkentry:refs/notes/inkentry",
        ],
    );

    let clone = p.tmp.path().join("clone");
    git(
        p.tmp.path(),
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );

    let clone_home = TempDir::new().unwrap();
    let clone_cfg = clone.join("config.toml");
    std::fs::write(&clone_cfg, "").unwrap();
    let init_out = inkentry_bin_in(clone_home.path())
        .current_dir(&clone)
        .env("INKENTRY_NO_SERVER", "1")
        .env_remove("INKENTRY_SERVER_URL")
        .arg("--config")
        .arg(&clone_cfg)
        .args(["init", "--no-index"])
        .assert()
        .success();
    let init_stdout = stdout(init_out);
    assert!(
        init_stdout.contains("imported 2 entries from git notes"),
        "the clone should hydrate from the ref, got:\n{init_stdout}"
    );
    assert!(
        init_stdout.contains("minted on this machine") && init_stdout.contains("entity id"),
        "init must say the ids it shows are local, got:\n{init_stdout}"
    );

    let listed = stdout(
        inkentry_bin_in(clone_home.path())
            .current_dir(&clone)
            .env("INKENTRY_NO_SERVER", "1")
            .env_remove("INKENTRY_SERVER_URL")
            .arg("--config")
            .arg(&clone_cfg)
            .args(["memory", "--db"])
            .arg(clone.join(".inkentry").join("memory.db"))
            .args(["list", "--format", "json", "--archived"])
            .assert()
            .success(),
    );
    let there: Vec<serde_json::Value> = serde_json::from_str(&listed).expect("list json parses");

    for (title, eid) in [("Crosses the wire", &first), ("Crosses it too", &second)] {
        let mine = p.entry(title);
        let theirs = there
            .iter()
            .find(|n| n["title"] == title)
            .unwrap_or_else(|| panic!("the clone is missing {title:?}"));
        assert_eq!(theirs["entity_id"].as_str().unwrap(), eid.as_str());
        assert_eq!(mine["entity_id"].as_str().unwrap(), eid.as_str());
        assert_ne!(
            mine["id"].as_str().unwrap(),
            theirs["id"].as_str().unwrap(),
            "the local id is minted per machine, so it must differ across the clone"
        );
    }
}

// ── an entry cannot supersede itself ────────────────────────────────────────

// Handles widened this: a 12-character handle and an 8-character prefix are two
// different-looking tokens for one entry, so a user can name the same entry
// twice without noticing. Letting it through archives the entry and points its
// successor link at itself.
#[test]
fn an_entry_cannot_supersede_itself_however_it_is_named() {
    let p = project();
    let eid = p.add("Its own successor", "body of the self-superseding entry");
    let uuid = p.uuid_of("Its own successor");

    for (old, new) in [
        (&eid[..12], &eid[..8]),
        (eid.as_str(), &eid[..12]),
        (uuid.as_str(), &eid[..12]),
        (uuid.as_str(), uuid.as_str()),
    ] {
        let assert = p.memory().args(["supersede", old, new]).assert().failure();
        let err = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
        assert!(
            err.contains("name the same memory entry"),
            "supersede {old} {new} should be refused, got: {err}"
        );
        assert_eq!(
            p.status_of("Its own successor"),
            "active",
            "supersede {old} {new} must not archive the entry"
        );
    }

    assert!(
        p.entry("Its own successor")["superseded_by"].is_null(),
        "nothing may have been linked"
    );
}
