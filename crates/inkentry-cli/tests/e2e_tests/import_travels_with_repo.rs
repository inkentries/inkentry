// Coverage for `inkentry import`'s git-notes write-through carrier (#51).
//
// Counting rows in the importing repo's own store proves nothing about what
// travels: that is exactly the state the defect describes, where every entry
// was present locally and none of it cloned. So the round trip here is a real
// one — import into a repo, push its notes, clone from the same origin,
// hydrate the clone, and read what arrived.

use crate::plumbing_helpers;
use plumbing_helpers::inkentry_bin_in;

use sha2::{Digest, Sha256};
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// A well-formed dump, footer computed as `docs/dump-format.md` specifies.
fn dump(body: &[&str], counts: &str) -> String {
    let header = r#"{"record":"header","format":"portable-dump","format_version":1,"generated_at":1786370293,"generator":"test/1.0.0"}"#;
    let mut lines = vec![header.to_string()];
    lines.extend(body.iter().map(|s| s.to_string()));

    let mut fold = Sha256::new();
    for line in &lines {
        fold.update(hex(&Sha256::digest(line.as_bytes())).as_bytes());
    }
    let digest = format!("sha256:{}", hex(&fold.finalize()));
    lines.push(format!(
        r#"{{"record":"footer","counts":{counts},"digest":"{digest}"}}"#
    ));
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

fn entry(dump_ref: &str, title: &str, created_at: i64, extra: &str) -> String {
    format!(
        r#"{{"record":"entity","type":"memory_entry","ref":"{dump_ref}","kind":"decision","title":"{title}","body":"body of {title}","created_at":{created_at}{extra}}}"#
    )
}

// Two entries and the supersede edge between them: enough to show that the
// edge travels too, not just the entries it links.
fn two_entries_and_a_supersede() -> String {
    dump(
        &[
            &entry("e1", "the older decision", 1000, r#","status":"archived""#),
            &entry("e2", "the newer decision", 2000, ""),
            r#"{"record":"relationship","type":"supersedes","from":"e2","to":"e1","created_at":2500}"#,
        ],
        r#"{"entity":{"memory_entry":2},"relationship":{"supersedes":1}}"#,
    )
}

fn empty_config(dir: &Path) -> PathBuf {
    let cfg = dir.join("config.toml");
    std::fs::write(&cfg, "").unwrap();
    cfg
}

// Run `inkentry import` in `dir`, writing to `dir/.inkentry/memory.db`, with
// `dir` as HOME. Offline: the embedding pass cannot reach a server and is
// reported rather than fatal, so this must still succeed.
fn run_import(dir: &Path, contents: &str) -> String {
    let path = dir.join("project.dump");
    std::fs::write(&path, contents).unwrap();
    let cfg = empty_config(dir);
    let out = inkentry_bin_in(dir)
        .current_dir(dir)
        .env("INKENTRY_NO_SERVER", "1")
        .env_remove("INKENTRY_SERVER_URL")
        .arg("--config")
        .arg(&cfg)
        .arg("import")
        .arg(&path)
        .arg("--db")
        .arg(dir.join(".inkentry").join("memory.db"))
        .arg("--no-embed")
        .output()
        .expect("spawn inkentry import");
    assert!(
        out.status.success(),
        "inkentry import failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn run_init(dir: &Path) {
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
}

// Entries in `dir`'s own memory store. `include_archived` selects between the
// default live view and the full one, which is what distinguishes "the entry
// arrived, marked archived" from "the entry arrived and reads as live".
fn local_entries(dir: &Path, include_archived: bool) -> Vec<serde_json::Value> {
    let mut args = vec!["memory", "list"];
    if include_archived {
        args.push("--archived");
    }
    args.extend(["--format", "jsonl", "--limit", "100"]);

    let out = inkentry_bin_in(dir)
        .current_dir(dir)
        .env("INKENTRY_NO_SERVER", "1")
        .args(&args)
        .output()
        .expect("spawn inkentry memory list");
    assert!(
        out.status.success(),
        "memory list failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .collect()
}

fn titles_of(entries: &[serde_json::Value]) -> Vec<String> {
    let mut titles: Vec<String> = entries
        .iter()
        .filter_map(|v| Some(v.get("title")?.as_str()?.to_string()))
        .collect();
    titles.sort();
    titles
}

// Titles of every entry (archived included) in `dir`'s own memory store.
fn local_titles(dir: &Path) -> Vec<String> {
    titles_of(&local_entries(dir, true))
}

fn entry_titled<'a>(entries: &'a [serde_json::Value], title: &str) -> &'a serde_json::Value {
    entries
        .iter()
        .find(|v| v.get("title").and_then(|t| t.as_str()) == Some(title))
        .unwrap_or_else(|| panic!("no entry titled {title:?} in {entries:#?}"))
}

fn carrier_record_titled(dir: &Path, title: &str) -> serde_json::Value {
    let records = carrier_records(dir);
    records
        .iter()
        .find(|v| v.get("title").and_then(|t| t.as_str()) == Some(title))
        .unwrap_or_else(|| panic!("no carrier record titled {title:?} in {records:#?}"))
        .clone()
}

// Every inkentry record on the notes ref, across all reachable commits, as raw
// lines. Deliberately unfolded: a duplicate append is invisible once the
// reader folds by `entity_id`, and duplication is the thing under test.
fn carrier_records(dir: &Path) -> Vec<serde_json::Value> {
    let listing = git_stdout(dir, &["notes", "--ref=inkentry", "list"]);
    let mut out = Vec::new();
    for line in listing.lines() {
        let Some(blob) = line.split_whitespace().next() else {
            continue;
        };
        for l in git_stdout(dir, &["cat-file", "-p", blob]).lines() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(l.trim())
                && v.get("entity_id").is_some()
            {
                out.push(v);
            }
        }
    }
    out
}

fn carrier_titles(dir: &Path) -> Vec<String> {
    let mut titles: Vec<String> = carrier_records(dir)
        .iter()
        .filter_map(|v| Some(v.get("title")?.as_str()?.to_string()))
        .collect();
    titles.sort();
    titles
}

fn init_repo_with_commit(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    std::fs::write(dir.join("f.txt"), "x\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

// A bare origin and one working clone "a", both on the same single commit.
// `a` gets a `.inkentry/` dir so the import resolves to the SQLite-primary
// path with the carrier write-through, not the pre-init fallback.
fn setup_origin_and_clone(tmp: &Path) -> (PathBuf, PathBuf) {
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

    (origin, a)
}

// ── what the import puts on the carrier ──────────────────────────────────────

#[test]
fn imported_entries_land_on_the_notes_ref() {
    let tmp = TempDir::new().unwrap();
    let (_origin, a) = setup_origin_and_clone(tmp.path());

    let stdout = run_import(&a, &two_entries_and_a_supersede());

    assert_eq!(
        carrier_titles(&a),
        vec!["the newer decision", "the older decision"],
        "both imported entries must reach the carrier"
    );
    assert!(
        stdout.contains("Carried 2 entries into git notes"),
        "the import must say what now travels; got: {stdout}"
    );
}

/// The dump's own values travel, not this machine's. `created_at` in
/// particular orders the carrier's fold, so stamping the wall clock would make
/// the same entry sort differently on every machine that imported the dump.
#[test]
fn the_carrier_record_keeps_the_dump_s_own_values() {
    let tmp = TempDir::new().unwrap();
    let (_origin, a) = setup_origin_and_clone(tmp.path());

    run_import(&a, &two_entries_and_a_supersede());

    let records = carrier_records(&a);
    let older = records
        .iter()
        .find(|v| v["title"] == "the older decision")
        .expect("the older entry is on the carrier");
    assert_eq!(
        older["created_at"], 1000,
        "creation time is carried, not re-stamped"
    );
    assert_eq!(older["status"], "archived", "status is carried");

    let newer = records
        .iter()
        .find(|v| v["title"] == "the newer decision")
        .expect("the newer entry is on the carrier");
    assert_eq!(
        older["superseded_by_entity_id"], newer["entity_id"],
        "the supersede edge must travel in its portable spelling, pointing at \
         the successor's entity_id"
    );
}

/// The conflict case the write-through forces: re-importing a dump this repo
/// already carries must not write it again. Folding would collapse a duplicate
/// on read, so the assertion is on the raw ref, which is where a re-import
/// would otherwise double the log every time.
#[test]
fn re_importing_the_same_dump_does_not_write_it_to_the_carrier_twice() {
    let tmp = TempDir::new().unwrap();
    let (_origin, a) = setup_origin_and_clone(tmp.path());
    let d = two_entries_and_a_supersede();

    run_import(&a, &d);
    let after_first = carrier_records(&a).len();
    let stdout = run_import(&a, &d);

    assert_eq!(after_first, 2);
    assert_eq!(
        carrier_records(&a).len(),
        2,
        "a second import of the same dump must add no carrier records"
    );
    assert!(
        stdout.contains("already in this repository's git notes"),
        "the import must say why it carried nothing; got: {stdout}"
    );
}

/// A repo the store does not sit inside has no carrier, and that is not a
/// failure: the import still lands every row and says nothing about travel.
#[test]
fn an_import_outside_a_git_repo_still_succeeds() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("loose");
    std::fs::create_dir_all(dir.join(".inkentry")).unwrap();

    let stdout = run_import(&dir, &two_entries_and_a_supersede());

    assert!(stdout.contains("Imported 2 memory entries"));
    assert!(
        !stdout.contains("git notes"),
        "there is no carrier to report on; got: {stdout}"
    );
    assert_eq!(
        local_titles(&dir),
        vec!["the newer decision", "the older decision"]
    );
}

// ── the round trip the issue asks for ────────────────────────────────────────

/// Import a dump, publish the notes, clone from the same origin, hydrate the
/// clone, and end up with exactly the entries you started with.
///
/// This is the property the defect broke and the one counts cannot show: the
/// teammate's repo is a genuine second clone whose memory store starts empty,
/// so every entry it ends up with arrived through git.
#[test]
fn an_imported_log_clones_with_the_repository_and_does_not_duplicate() {
    let tmp = TempDir::new().unwrap();
    let (origin, a) = setup_origin_and_clone(tmp.path());

    run_import(&a, &two_entries_and_a_supersede());
    git(
        &a,
        &[
            "push",
            "-q",
            origin.to_str().unwrap(),
            "refs/notes/inkentry:refs/notes/inkentry",
        ],
    );

    let b = tmp.path().join("b");
    git(
        tmp.path(),
        &["clone", "-q", origin.to_str().unwrap(), b.to_str().unwrap()],
    );
    git(&b, &["config", "user.email", "b@example.com"]);
    git(&b, &["config", "user.name", "B"]);
    // Explicit rather than relying on the refspec `init` configures, so the
    // fetch is not racing the config that enables it.
    git(
        &b,
        &[
            "fetch",
            "-q",
            "origin",
            &format!("refs/notes/inkentry:{TRACKING_REF}"),
        ],
    );

    run_init(&b);

    assert_eq!(
        local_titles(&b),
        vec!["the newer decision", "the older decision"],
        "the clone must hydrate exactly the imported entries — no losses, no \
         duplicates — from git alone"
    );
    // A second read re-walks the ref; dedup on the convergence key is what
    // keeps it from re-inserting what it already imported.
    assert_eq!(
        local_titles(&b),
        vec!["the newer decision", "the older decision"],
        "reading again must not grow the store"
    );

    // Status is the half of the supersede fact that a reader acts on, and it
    // is the one the fold could silently drop on the receiving side: a copy
    // that came back active would resurrect a decision the sender retired.
    let all = local_entries(&b, true);
    assert_eq!(
        entry_titled(&all, "the older decision")["status"],
        "archived",
        "the archived entry must still read as archived after hydrating"
    );
    assert_eq!(
        entry_titled(&all, "the newer decision")["status"],
        "active",
        "its successor must not have been archived along with it"
    );
    assert_eq!(
        titles_of(&local_entries(&b, false)),
        vec!["the newer decision"],
        "the archived entry must not come back live in the default view"
    );

    // The supersede edge in its portable spelling, on the receiving repo's own
    // carrier: the predecessor's record must name the successor's entity_id,
    // which is what lets any reader resolve the edge without a shared rowid.
    let older = carrier_record_titled(&b, "the older decision");
    let newer = carrier_record_titled(&b, "the newer decision");
    assert_eq!(
        older["superseded_by_entity_id"], newer["entity_id"],
        "the supersede edge must survive the clone, resolving to the \
         successor's entity_id"
    );
    assert!(
        !newer["entity_id"].as_str().unwrap_or_default().is_empty(),
        "negative control: the successor must actually carry an entity_id, so \
         the comparison above is not two absent fields matching"
    );
}
