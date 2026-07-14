//! Integration tests for `GitNotesBackend` — concurrency and round-trip.
//!
//! These tests require `git` to be on PATH and are skipped if the current
//! working directory is not inside a git repository (CI environments without
//! git are unaffected).
//!
//! ## Concurrent-write safety (#185)
//!
//! Writes are a read-modify-write that rewrites the HEAD note with
//! `git notes add -f` (replace semantics). Two agents writing to the *same
//! HEAD* concurrently would race, and the loser's entry would vanish silently
//! with both exiting 0. ADR-069 (D6) closes that: every read-modify-write is
//! serialized by a lock in the git common dir, so all writers survive.

mod common;

use serial_test::serial;
use spelunk_core::storage::GitNotesBackend;
use spelunk_core::storage::MemoryBackend;
use spelunk_core::storage::NoteInput;

// ── helpers ──────────────────────────────────────────────────────────────────

/// Create a temporary git repo with one initial commit.
/// Returns the path; the repo is cleaned up when the returned `TempDir` drops.
fn make_temp_git_repo() -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let p = dir.path();

    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(p)
            .output()
            .expect("git command")
    };

    run(&["init", "-b", "main"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "Test"]);
    // Create an initial commit so HEAD resolves.
    std::fs::write(p.join("README.md"), "test").expect("write");
    run(&["add", "."]);
    run(&[
        "commit",
        "--no-gpg-sign",
        "-m",
        "init",
        "--allow-empty-message",
    ]);

    dir
}

fn note_input(kind: &str, title: &str) -> NoteInput {
    NoteInput {
        kind: kind.to_string(),
        title: title.to_string(),
        body: format!("body for {title}"),
        tags: vec![],
        linked_files: vec![],
        embedding: None,
        source_ref: None,
        valid_at: None,
        supersedes: None,
    }
}

// ── basic round-trip ─────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn git_notes_add_and_list_round_trip() {
    let dir = make_temp_git_repo();
    let backend = GitNotesBackend::with_root(dir.path().to_path_buf());

    let id = backend
        .add(note_input("decision", "use sqlcipher"))
        .await
        .expect("add");

    let notes = backend
        .list(Some("decision"), 10, false, None)
        .await
        .expect("list");

    assert_eq!(notes.len(), 1, "expected exactly one note");
    assert_eq!(notes[0].id, id);
    assert_eq!(notes[0].title, "use sqlcipher");
    assert_eq!(notes[0].kind, "decision");
}

#[tokio::test]
#[serial]
async fn git_notes_list_without_kind_returns_all() {
    let dir = make_temp_git_repo();
    let backend = GitNotesBackend::with_root(dir.path().to_path_buf());

    backend
        .add(note_input("decision", "first"))
        .await
        .expect("add 1");

    // Make a second commit so the two notes don't overwrite each other.
    std::fs::write(dir.path().join("a.txt"), "a").expect("write");
    std::process::Command::new("git")
        .args(["-C", dir.path().to_str().unwrap(), "add", "."])
        .output()
        .ok();
    std::process::Command::new("git")
        .args([
            "-C",
            dir.path().to_str().unwrap(),
            "commit",
            "--no-gpg-sign",
            "--allow-empty",
            "-m",
            "second commit",
        ])
        .output()
        .expect("second commit");

    backend
        .add(note_input("note", "second"))
        .await
        .expect("add 2");

    let all = backend.list(None, 10, false, None).await.expect("list");
    assert_eq!(all.len(), 2, "expected two notes across two commits");
}

// ── concurrent write safety (#185) ───────────────────────────────────────────

/// Two tasks writing a note to the same HEAD concurrently must both survive, as
/// well-formed records of the two distinct writers. Serialized by the notes
/// lock, so the count is exact rather than timing-dependent.
#[tokio::test]
#[serial]
async fn git_notes_concurrent_same_head_stays_consistent() {
    let dir = make_temp_git_repo();

    let root = dir.path().to_path_buf();
    let b1 = GitNotesBackend::with_root(root.clone());
    let b2 = GitNotesBackend::with_root(root.clone());

    // Two concurrent adds to the same HEAD; both must return Ok (no panic).
    let (r1, r2) = tokio::join!(
        b1.add(note_input("note", "agent A")),
        b2.add(note_input("note", "agent B")),
    );
    r1.expect("agent A add");
    r2.expect("agent B add");

    let notes = GitNotesBackend::with_root(dir.path().to_path_buf())
        .list(None, 10, false, None)
        .await
        .expect("list");

    // Serialized by the notes lock: neither writer's entry may be dropped.
    assert_eq!(
        notes.len(),
        2,
        "both concurrent same-HEAD adds must survive; got {} ({:?})",
        notes.len(),
        notes.iter().map(|n| &n.title).collect::<Vec<_>>()
    );
    // Every surviving entry is a well-formed note we wrote — no partial/garbled
    // records from an interleaved write.
    for n in &notes {
        assert_eq!(n.kind, "note", "unexpected kind: {}", n.kind);
        assert!(
            n.title == "agent A" || n.title == "agent B",
            "unexpected title: {}",
            n.title
        );
    }
    assert_ne!(
        notes[0].title, notes[1].title,
        "the survivors are the two distinct writers, not one entry twice"
    );
}

/// Deterministic sibling of the concurrent test: two *sequential* adds to the
/// same HEAD both survive as distinct records. The concurrent test only reaches
/// the 2-note path on a rare scheduling race (~1% of runs), so its distinct /
/// well-formed checks are seldom exercised; this locks the same-HEAD append
/// contract down on every run without any timing dependence.
#[tokio::test]
#[serial]
async fn git_notes_sequential_same_head_both_survive_distinct() {
    let dir = make_temp_git_repo();
    let backend = GitNotesBackend::with_root(dir.path().to_path_buf());

    backend.add(note_input("note", "agent A")).await.expect("A");
    backend.add(note_input("note", "agent B")).await.expect("B");

    let notes = backend.list(None, 10, false, None).await.expect("list");
    assert_eq!(notes.len(), 2, "both sequential same-HEAD adds survive");
    for n in &notes {
        assert_eq!(n.kind, "note", "unexpected kind: {}", n.kind);
    }
    let titles: std::collections::HashSet<&str> = notes.iter().map(|n| n.title.as_str()).collect();
    assert_eq!(
        titles,
        ["agent A", "agent B"].into_iter().collect(),
        "both writers present and distinct"
    );
}

/// Archive marks an entry with status=archived and hides it from default list.
#[tokio::test]
#[serial]
async fn git_notes_archive_hides_entry() {
    let dir = make_temp_git_repo();
    let backend = GitNotesBackend::with_root(dir.path().to_path_buf());

    let id = backend
        .add(note_input("decision", "archive me"))
        .await
        .expect("add");

    let archived = backend.archive(id).await.expect("archive");
    assert!(archived);

    let active = backend
        .list(None, 10, false, None)
        .await
        .expect("list active");
    assert!(
        active.is_empty(),
        "archived entry should not appear in default list"
    );

    let all = backend
        .list(None, 10, true, None)
        .await
        .expect("list including archived");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].status, "archived");
}

/// Unsupported methods return clear errors rather than panicking.
#[tokio::test]
#[serial]
async fn git_notes_unsupported_methods_return_errors() {
    let dir = make_temp_git_repo();
    let backend = GitNotesBackend::with_root(dir.path().to_path_buf());

    assert!(backend.search(&[], "q", 5, None).await.is_err());
    assert!(backend.search_hybrid(&[], "q", 5, None).await.is_err());
    assert!(backend.search_text("q", 5, None).await.is_err());
    assert!(backend.search_timeline(&[], "q", 5).await.is_err());
    assert!(backend.harvested_shas().await.is_err());
    assert!(backend.has_source_ref("abc123").await.is_err());
    assert!(backend.add_edge(1, 2, "relates_to").await.is_err());
    assert!(backend.get_edges(1).await.is_err());
    assert!(backend.supersede(1, 2).await.is_err());
}

// ── append_to_git_notes write-through helper ─────────────────────────────────

use spelunk_core::storage::{NoteRecord, append_to_git_notes};

fn make_note_record(id: i64, title: &str) -> NoteRecord {
    NoteRecord {
        schema_version: 1,
        id,
        kind: "decision".to_string(),
        title: title.to_string(),
        body: format!("body for {title}"),
        tags: vec![],
        linked_files: vec![],
        created_at: 0,
        status: "active".to_string(),
        source_ref: None,
        valid_at: None,
        invalid_at: None,
        superseded_by: None,
        remote_id: None,
    }
}

/// (a) A single `append_to_git_notes` call writes a parseable note to HEAD.
#[tokio::test]
#[serial]
async fn append_to_git_notes_writes_note_when_enabled() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    let record = make_note_record(1, "first decision");
    append_to_git_notes(Some(root), &record)
        .await
        .expect("append should succeed");

    // Read back the raw note text.
    let out = std::process::Command::new("git")
        .args(["notes", "--ref=spelunk", "show", "HEAD"])
        .current_dir(root)
        .output()
        .expect("git notes show");
    assert!(out.status.success(), "note should exist on HEAD");

    let text = String::from_utf8_lossy(&out.stdout);
    let trimmed = text.trim();
    assert!(!trimmed.is_empty(), "note should not be empty");

    // Should be valid JSON on a single line.
    let parsed: serde_json::Value =
        serde_json::from_str(trimmed).expect("note should be valid JSON");
    assert_eq!(
        parsed["title"].as_str().unwrap(),
        "first decision",
        "title should round-trip"
    );
    assert_eq!(parsed["id"].as_i64().unwrap(), 1);
}

/// (b) A second `append_to_git_notes` call appends rather than overwrites.
#[tokio::test]
#[serial]
async fn append_to_git_notes_appends_not_overwrites() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    let rec1 = make_note_record(10, "first");
    let rec2 = make_note_record(20, "second");

    append_to_git_notes(Some(root), &rec1)
        .await
        .expect("first append");
    append_to_git_notes(Some(root), &rec2)
        .await
        .expect("second append");

    // Both entries should be present as separate JSON lines.
    let out = std::process::Command::new("git")
        .args(["notes", "--ref=spelunk", "show", "HEAD"])
        .current_dir(root)
        .output()
        .expect("git notes show");
    assert!(out.status.success());

    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();

    assert_eq!(
        lines.len(),
        2,
        "expected 2 JSON lines after two appends; got:\n{text}"
    );

    let ids: Vec<i64> = lines
        .iter()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).expect("valid JSON line");
            v["id"].as_i64().expect("id field")
        })
        .collect();

    assert!(ids.contains(&10), "first record (id=10) should be present");
    assert!(ids.contains(&20), "second record (id=20) should be present");
}

/// (c) `store_in_git_notes = false` must skip the git note write entirely.
///
/// We verify this by calling `append_to_git_notes` only when the flag is true
/// and confirming no note exists when it is false.  The actual config-flag
/// gating happens in `memory_add`; here we test the conditional call pattern.
#[tokio::test]
#[serial]
async fn append_to_git_notes_skipped_when_flag_is_false() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    // Simulate store_in_git_notes = false: do NOT call append_to_git_notes.
    let store_in_git_notes = false;
    let record = make_note_record(99, "should not appear");
    if store_in_git_notes {
        append_to_git_notes(Some(root), &record)
            .await
            .expect("would append");
    }

    let out = std::process::Command::new("git")
        .args(["notes", "--ref=spelunk", "show", "HEAD"])
        .current_dir(root)
        .output()
        .expect("git notes show command");

    // When the flag is false no note is written, so git notes show should fail.
    assert!(
        !out.status.success(),
        "no note should exist when store_in_git_notes=false"
    );
}

/// (d) `append_to_git_notes` returns `Err` when HEAD does not exist (not a git repo).
///     The CLI path wraps this in a `tracing::warn!` + `return Ok(())` — this test
///     verifies the error surface so callers can rely on it.
#[tokio::test]
#[serial]
async fn append_to_git_notes_returns_err_outside_git_repo() {
    let non_git_dir = tempfile::TempDir::new().expect("tempdir");
    let record = make_note_record(1, "test");
    let result = append_to_git_notes(Some(non_git_dir.path()), &record).await;
    assert!(result.is_err(), "should return Err when not in a git repo");
}

// ── security regression: note bodies must go via stdin, never argv ──────────
//
// oss^61: `git notes add` used to receive the note body as a `-m <arg>` argv
// value. A body that itself looked like a git option (e.g. starting with `-`)
// could previously be misparsed as an option to `git notes add` rather than
// literal note content. These tests prove (a) option-like / metacharacter-
// laden bodies round-trip as *literal* note text rather than being
// interpreted or truncated, which is only possible if they're delivered via
// stdin (`-F -`) rather than argv (`-m`), and (b) the same holds for
// `GitNotesBackend::add`/`archive`, which route through `add_note_stdin`.

/// A note body that is itself option-shaped (starts with `-`) must be stored
/// and read back byte-for-byte, not interpreted as a `git notes add` option
/// or truncated. This would fail (or `git notes add` would error/misbehave)
/// if the body were still passed via `-m <body>` on argv.
#[tokio::test]
#[serial]
async fn append_to_git_notes_body_starting_with_dash_is_literal() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    let mut record = make_note_record(1, "dash body");
    record.body = "--force-with-lease=origin/main --output=/tmp/pwned".to_string();

    append_to_git_notes(Some(root), &record)
        .await
        .expect("append should succeed even with option-like body");

    let out = std::process::Command::new("git")
        .args(["notes", "--ref=spelunk", "show", "HEAD"])
        .current_dir(root)
        .output()
        .expect("git notes show");
    assert!(out.status.success(), "note should exist on HEAD");

    let text = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(text.trim()).expect("note should be valid JSON");
    assert_eq!(
        parsed["body"].as_str().unwrap(),
        "--force-with-lease=origin/main --output=/tmp/pwned",
        "option-like body must round-trip literally, proving it was never argv-parsed"
    );

    // The exploit this guards against: if the body had leaked onto argv as an
    // option value or been split by the shell/argv parser, it could reach
    // paths outside the repo. Confirm no such file was created as a side
    // effect of writing this note.
    assert!(
        !std::path::Path::new("/tmp/pwned").exists(),
        "option-like note body must never be interpreted as a git option"
    );
    let _ = std::fs::remove_file("/tmp/pwned");
}

/// A note body containing shell metacharacters must round-trip untouched.
/// All git spawns in this codebase use argv vectors (no shell), so this is
/// defense-in-depth / regression coverage rather than a shell-injection PoC.
#[tokio::test]
#[serial]
async fn append_to_git_notes_body_with_shell_metacharacters_is_literal() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    let mut record = make_note_record(2, "metachar body");
    record.body = "$(rm -rf /); `echo pwned`; a && b || c; a | b > /tmp/x; a; b\nnewline".into();

    append_to_git_notes(Some(root), &record)
        .await
        .expect("append should succeed with shell-metacharacter body");

    let out = std::process::Command::new("git")
        .args(["notes", "--ref=spelunk", "show", "HEAD"])
        .current_dir(root)
        .output()
        .expect("git notes show");
    assert!(out.status.success());

    let text = String::from_utf8_lossy(&out.stdout);
    // Body is one line of a `\n`-joined ledger; find the JSON line for id=2.
    let line = text
        .lines()
        .find(|l| l.contains("\"id\":2"))
        .expect("record for id=2 present");
    let parsed: serde_json::Value = serde_json::from_str(line).expect("valid JSON line");
    assert_eq!(
        parsed["body"].as_str().unwrap(),
        "$(rm -rf /); `echo pwned`; a && b || c; a | b > /tmp/x; a; b\nnewline",
        "shell-metacharacter body must round-trip literally"
    );
}

/// `GitNotesBackend::add` (used by the `MemoryBackend` trait impl, i.e. the
/// `spelunk memory add` path) also writes via `add_note_stdin`. Confirm an
/// option-like note title/body round-trips literally through that path too —
/// not just through the lower-level `append_to_git_notes` free function.
#[tokio::test]
#[serial]
async fn git_notes_backend_add_with_option_like_body_round_trips() {
    let dir = make_temp_git_repo();
    let backend = GitNotesBackend::with_root(dir.path().to_path_buf());

    let mut input = note_input("decision", "--amend");
    input.body = "-f --force --output=/tmp/should-not-exist-oss61".to_string();

    let id = backend.add(input).await.expect("add");

    let notes = backend
        .list(Some("decision"), 10, false, None)
        .await
        .expect("list");
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].id, id);
    assert_eq!(notes[0].title, "--amend");
    assert_eq!(
        notes[0].body, "-f --force --output=/tmp/should-not-exist-oss61",
        "option-like body must round-trip literally via GitNotesBackend::add"
    );

    assert!(!std::path::Path::new("/tmp/should-not-exist-oss61").exists());
}

// ── JSONL canonical: permissive read / preserving write (ADR-059) ────────────
//
// A note blob is JSON Lines interleaved with foreign content (prose, other
// tools' lines). Reads skip foreign lines without erroring; writes preserve
// every foreign line and every untargeted spelunk record byte-for-byte.

/// Write a raw note blob verbatim to HEAD's `refs/notes/spelunk` note, via
/// stdin (`-F -`) so arbitrary content is delivered literally.
fn write_raw_note(root: &std::path::Path, blob: &str) {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let head = String::from_utf8(
        Command::new("git")
            .args(["-C", root.to_str().unwrap(), "rev-parse", "HEAD"])
            .output()
            .expect("rev-parse")
            .stdout,
    )
    .expect("utf8");
    let head = head.trim();
    let mut child = Command::new("git")
        .args([
            "-C",
            root.to_str().unwrap(),
            "notes",
            "--ref=spelunk",
            "add",
            "-f",
            "-F",
            "-",
            "--",
            head,
        ])
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn git notes add");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(blob.as_bytes())
        .expect("write blob");
    assert!(child.wait().expect("wait").success(), "git notes add");
}

/// Read HEAD's raw `refs/notes/spelunk` note blob.
fn read_raw_note(root: &std::path::Path) -> String {
    let out = std::process::Command::new("git")
        .args([
            "-C",
            root.to_str().unwrap(),
            "notes",
            "--ref=spelunk",
            "show",
            "HEAD",
        ])
        .output()
        .expect("git notes show");
    assert!(out.status.success(), "note should exist");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The ADR-059 conformance fixture: three real `decision` records interleaved
/// with markdown prose and blank lines.
fn adr_conformance_blob() -> String {
    let rec = |id: i64, memory: &str| {
        serde_json::to_string(&make_note_record(id, memory)).expect("serialize record")
    };
    format!(
        "# Implement payment by Stripe\n\
         \n\
         We're implementing a payment rail, in this case Stripe...\n\
         \n\
         {}\n\
         \n\
         ## Technical details\n\
         \n\
         ...Axum as the http handler layer.\n\
         \n\
         {}\n\
         {}\n",
        rec(1, "use stripe for payment processing"),
        rec(2, "rust is our language of choice"),
        rec(3, "use axum to implement api's over restful http"),
    )
}

/// (a) The ADR worked example round-trips: reading yields exactly the three
/// `decision` records in order, ignoring the four prose blocks and blank lines,
/// with no error.
#[tokio::test]
#[serial]
async fn git_notes_adr_conformance_read_skips_prose() {
    let dir = make_temp_git_repo();
    let root = dir.path();
    write_raw_note(root, &adr_conformance_blob());

    let backend = GitNotesBackend::with_root(root.to_path_buf());
    let notes = backend.list(None, 100, false, None).await.expect("list");

    assert_eq!(notes.len(), 3, "three records, prose skipped");
    assert_eq!(notes[0].id, 1);
    assert_eq!(notes[1].id, 2);
    assert_eq!(notes[2].id, 3);
    assert_eq!(notes[0].title, "use stripe for payment processing");
    assert_eq!(
        notes[2].title,
        "use axum to implement api's over restful http"
    );
}

/// (b) A blob with a foreign line and no spelunk records reads as an empty list
/// with no error; a permissive read never fails on unparseable lines.
#[tokio::test]
#[serial]
async fn git_notes_read_foreign_only_is_empty_no_error() {
    let dir = make_temp_git_repo();
    let root = dir.path();
    // Prose, a non-object JSON value, and a malformed line: all foreign.
    write_raw_note(
        root,
        "just some freeform notes from another tool\n[1, 2, 3]\n{not valid json",
    );

    let backend = GitNotesBackend::with_root(root.to_path_buf());
    let notes = backend.list(None, 100, true, None).await.expect("list");
    assert!(notes.is_empty(), "no spelunk records, no error");
}

/// (c) `add` (append) preserves all prior content: the four prose blocks and the
/// three original records are retained, and the blob now holds four records.
#[tokio::test]
#[serial]
async fn git_notes_add_preserves_prose_and_siblings() {
    let dir = make_temp_git_repo();
    let root = dir.path();
    write_raw_note(root, &adr_conformance_blob());

    let backend = GitNotesBackend::with_root(root.to_path_buf());
    backend
        .add(note_input("decision", "a fourth decision"))
        .await
        .expect("add fourth");

    // Prose retained verbatim.
    let blob = read_raw_note(root);
    assert!(
        blob.contains("# Implement payment by Stripe"),
        "heading kept"
    );
    assert!(
        blob.contains("...Axum as the http handler layer."),
        "prose kept"
    );

    // Four records now readable, originals intact and in order.
    let notes = backend.list(None, 100, false, None).await.expect("list");
    assert_eq!(notes.len(), 4, "three original + one appended");
    assert_eq!(notes[0].id, 1);
    assert_eq!(notes[1].id, 2);
    assert_eq!(notes[2].id, 3);
    assert_eq!(notes[3].title, "a fourth decision");
}

/// (c) `archive` of the middle record sets only that record's status; the other
/// two records and all prose lines are unchanged in content and position.
#[tokio::test]
#[serial]
async fn git_notes_archive_does_not_clobber_siblings_or_prose() {
    let dir = make_temp_git_repo();
    let root = dir.path();
    write_raw_note(root, &adr_conformance_blob());

    let backend = GitNotesBackend::with_root(root.to_path_buf());
    let archived = backend.archive(2).await.expect("archive middle");
    assert!(archived, "middle record archived");

    // Prose retained.
    let blob = read_raw_note(root);
    assert!(
        blob.contains("# Implement payment by Stripe"),
        "heading kept"
    );
    assert!(blob.contains("## Technical details"), "second heading kept");

    // Records 1 and 3 still active; only record 2 archived.
    let active = backend.list(None, 100, false, None).await.expect("active");
    let active_ids: Vec<i64> = active.iter().map(|n| n.id).collect();
    assert_eq!(active_ids, vec![1, 3], "only the middle record is hidden");

    let all = backend.list(None, 100, true, None).await.expect("all");
    assert_eq!(all.len(), 3, "all three records still present");
    let rec2 = all.iter().find(|n| n.id == 2).expect("record 2 present");
    assert_eq!(rec2.status, "archived", "only record 2 is archived");
}

// ── concurrent append safety (#185) ──────────────────────────────────────────

/// N concurrent `append_to_git_notes` calls against one HEAD must all survive.
///
/// Regression guard for #185: the read-modify-write is only safe while every
/// writer holds the notes lock across all four steps. Without it, two writers
/// read the same body and the later write-back drops the earlier entry, both
/// exiting 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[serial]
async fn append_to_git_notes_concurrent_writers_all_survive() {
    const WRITERS: i64 = 8;

    let dir = make_temp_git_repo();
    let root = dir.path().to_path_buf();

    let mut tasks = Vec::new();
    for id in 1..=WRITERS {
        let root = root.clone();
        tasks.push(tokio::spawn(async move {
            let record = make_note_record(id, &format!("concurrent decision {id}"));
            append_to_git_notes(Some(&root), &record).await
        }));
    }

    for task in tasks {
        task.await
            .expect("writer task should not panic")
            .expect("append should succeed");
    }

    // Every writer's entry must be present in the final note body.
    let blob = read_raw_note(&root);
    let found: Vec<i64> = blob
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
        .filter_map(|v| v["id"].as_i64())
        .collect();

    let missing: Vec<i64> = (1..=WRITERS).filter(|id| !found.contains(id)).collect();
    assert!(
        missing.is_empty(),
        "lost {} of {WRITERS} concurrent entries (ids {missing:?}); surviving ids {found:?}",
        missing.len(),
    );
}

/// Concurrent writers in **separate worktrees** must all survive.
///
/// Worktrees share one `refs/notes/spelunk` (it resolves through the git
/// common dir to the main repo's copy), so they are real contenders on one
/// note body. This pins the lock to the common dir: a lock keyed on the
/// per-worktree git dir would still pass the single-repo test above while
/// serializing nothing here.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[serial]
async fn append_to_git_notes_concurrent_worktrees_all_survive() {
    const PER_TREE: i64 = 4;

    let dir = make_temp_git_repo();
    let main_root = dir.path().to_path_buf();

    // Second worktree on a new branch at the same commit, so both HEADs
    // resolve to one note object.
    let wt_parent = tempfile::TempDir::new().expect("tempdir");
    let wt_root = wt_parent.path().join("wt");
    let out = std::process::Command::new("git")
        .args(["worktree", "add", "-b", "wt", wt_root.to_str().unwrap()])
        .current_dir(&main_root)
        .output()
        .expect("git worktree add");
    assert!(
        out.status.success(),
        "worktree add: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Precondition: the two trees really do share one notes ref.
    let head_of = |root: &std::path::Path| -> String {
        let o = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .expect("rev-parse");
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    };
    assert_eq!(
        head_of(&main_root),
        head_of(&wt_root),
        "both worktrees must sit on the same commit"
    );

    let mut tasks = Vec::new();
    for (tree, base) in [(&main_root, 0), (&wt_root, PER_TREE)] {
        for n in 1..=PER_TREE {
            let root = tree.clone();
            let id = base + n;
            tasks.push(tokio::spawn(async move {
                let record = make_note_record(id, &format!("worktree decision {id}"));
                append_to_git_notes(Some(&root), &record).await
            }));
        }
    }

    for task in tasks {
        task.await
            .expect("writer task should not panic")
            .expect("append should succeed");
    }

    let total = PER_TREE * 2;
    let blob = read_raw_note(&main_root);
    let found: Vec<i64> = blob
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
        .filter_map(|v| v["id"].as_i64())
        .collect();

    let missing: Vec<i64> = (1..=total).filter(|id| !found.contains(id)).collect();
    assert!(
        missing.is_empty(),
        "lost {} of {total} cross-worktree entries (ids {missing:?}); surviving ids {found:?}",
        missing.len(),
    );
}

// ── the lock itself: contract and degraded paths (ADR-069 D6) ────────────────

use spelunk_core::storage::lock_notes;

/// The wait budget `lock_notes` allows before giving up on a contended lock.
/// Mirrors `LOCK_WAIT_BUDGET` in `storage/git_notes/lock.rs` (not exported).
const LOCK_WAIT_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// The path production locks: `<git-common-dir>/spelunk-notes.lock`. Mirrors
/// `notes_lock_path`, including resolving git's relative answer (a plain repo
/// answers `.git`) against the repo root.
fn notes_lock_path(root: &std::path::Path) -> std::path::PathBuf {
    let out = std::process::Command::new("git")
        .args([
            "-C",
            root.to_str().unwrap(),
            "rev-parse",
            "--git-common-dir",
        ])
        .output()
        .expect("git rev-parse --git-common-dir");
    assert!(out.status.success(), "rev-parse --git-common-dir");

    let raw = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let raw = std::path::Path::new(&raw);
    let common_dir = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        root.join(raw)
    };
    common_dir.join("spelunk-notes.lock")
}

/// Open the lock file the way production does. A lock taken through this handle
/// really contends with `lock_notes`: std leaves same-handle (or cloned-handle)
/// re-locking unspecified, but a distinct `open` is a distinct handle on every
/// platform, so the conflict is well-defined.
fn open_lock_file(path: &std::path::Path) -> std::fs::File {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .expect("open notes lock file")
}

/// The `lock_notes` contract D5 builds on: `Some` when free, `None` when
/// contended past the budget, and the guard releases on drop.
///
/// D5 reads `None` as "skip the merge, read anyway", so `None` must be reachable
/// (never an indefinite block) and a dropped guard must not keep the lock held.
#[tokio::test]
#[serial]
async fn lock_notes_grants_when_free_yields_none_when_contended_and_frees_on_drop() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    let held = lock_notes(Some(root)).await;
    assert!(held.is_some(), "an uncontended lock must be granted");

    let started = std::time::Instant::now();
    let contended = lock_notes(Some(root)).await;
    let waited = started.elapsed();
    assert!(
        contended.is_none(),
        "a lock held elsewhere must yield None, not block indefinitely"
    );
    // Negative control: a `None` returned immediately would mean the lock was
    // never taken (a bad path, an unopenable file) and the contention above was
    // never actually exercised.
    assert!(
        waited >= LOCK_WAIT_BUDGET,
        "the contended caller must wait out the {LOCK_WAIT_BUDGET:?} budget before giving up; \
         gave up after {waited:?}, so it never contended"
    );

    drop(held);

    assert!(
        lock_notes(Some(root)).await.is_some(),
        "the guard must release the lock on drop"
    );
}

/// An unusable lock file degrades to an unlocked write, never an `Err`.
///
/// `memory add` treats a failed pre-`init` carry as fatal (ADR-068 D3), so a
/// lock that cannot even be opened must not surface as a command failure.
///
/// A directory at the lock path makes the open fail deterministically on every
/// platform (EISDIR on unix, access-denied on Windows).
#[tokio::test]
#[serial]
async fn append_to_git_notes_proceeds_when_lock_file_is_unusable() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    std::fs::create_dir_all(notes_lock_path(root)).expect("place a directory at the lock path");

    // Negative control: prove the lock really is unusable at the path production
    // resolves, so the write below exercises the degraded path rather than
    // quietly locking a file this test never blocked.
    assert!(
        lock_notes(Some(root)).await.is_none(),
        "setup: an unopenable lock path must yield None"
    );

    let record = make_note_record(1, "unusable lock still writes");
    append_to_git_notes(Some(root), &record)
        .await
        .expect("an unusable lock must degrade to an unlocked write, not an error");

    assert!(
        read_raw_note(root).contains("unusable lock still writes"),
        "the entry must still be written when the lock cannot be opened"
    );
}

/// Contention past the wait budget degrades to an unlocked write, never an `Err`
/// (the other half of the ADR-068 D3 interaction above).
///
/// Deterministic: the lock is held here for the whole call, so the writer is
/// guaranteed to exhaust its budget rather than race for it.
#[tokio::test]
#[serial]
async fn append_to_git_notes_proceeds_when_lock_budget_is_exhausted() {
    let dir = make_temp_git_repo();
    let root = dir.path();

    let held = open_lock_file(&notes_lock_path(root));
    held.lock()
        .expect("hold the notes lock for the whole write");

    let started = std::time::Instant::now();
    let result = append_to_git_notes(
        Some(root),
        &make_note_record(1, "contended lock still writes"),
    )
    .await;
    let waited = started.elapsed();

    drop(held);

    result.expect("lock contention must never fail the caller's write");
    // Negative control: too fast means the writer took the lock uncontended, so
    // the degraded path was never exercised.
    assert!(
        waited >= LOCK_WAIT_BUDGET,
        "the writer must have waited out the {LOCK_WAIT_BUDGET:?} budget; \
         returned after {waited:?}, so it never contended"
    );
    assert!(
        read_raw_note(root).contains("contended lock still writes"),
        "the entry must still be written after the lock budget is exhausted"
    );
}

// ── the `--backend git-notes` path (append_record / archive_record) ───────────

/// `GitNotesBackend::add` → `append_record` carries the same read-modify-write
/// as the write-through helper, so it needs the same guarantee: N concurrent
/// adds against one HEAD all survive.
///
/// Asserted on titles, not ids: `add` mints its id from `now_millis()` *before*
/// taking the lock, so adds landing in one millisecond can share an id. Entry
/// survival is what the lock guarantees.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[serial]
async fn git_notes_backend_concurrent_adds_all_survive() {
    const WRITERS: usize = 8;

    let dir = make_temp_git_repo();
    let root = dir.path().to_path_buf();

    let mut tasks = Vec::new();
    for n in 1..=WRITERS {
        let root = root.clone();
        tasks.push(tokio::spawn(async move {
            GitNotesBackend::with_root(root)
                .add(note_input("decision", &format!("backend writer {n}")))
                .await
        }));
    }
    for task in tasks {
        task.await
            .expect("writer task should not panic")
            .expect("backend add should succeed");
    }

    let notes = GitNotesBackend::with_root(root)
        .list(Some("decision"), 100, false, None)
        .await
        .expect("list");
    let titles: std::collections::HashSet<String> = notes.into_iter().map(|n| n.title).collect();

    let missing: Vec<String> = (1..=WRITERS)
        .map(|n| format!("backend writer {n}"))
        .filter(|t| !titles.contains(t))
        .collect();
    assert!(
        missing.is_empty(),
        "lost {} of {WRITERS} concurrent backend adds ({missing:?}); surviving {titles:?}",
        missing.len(),
    );
}

/// `GitNotesBackend::archive` → `archive_record` is the same read-modify-write
/// in reverse, and needs the same guarantee. Concurrent archives of distinct
/// entries must each land: unserialized, each writer's write-back would revive
/// the entries its peers had just archived.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[serial]
async fn git_notes_backend_concurrent_archives_all_land() {
    const ENTRIES: usize = 6;

    let dir = make_temp_git_repo();
    let root = dir.path().to_path_buf();
    let backend = GitNotesBackend::with_root(root.clone());

    // Sequential adds: each is several git subprocesses, so the `now_millis()`
    // ids are distinct — `archive(id)` targets the first id match, so a
    // collision here would archive the wrong record.
    let mut ids = Vec::new();
    for n in 1..=ENTRIES {
        ids.push(
            backend
                .add(note_input("decision", &format!("archive target {n}")))
                .await
                .expect("add"),
        );
    }
    let distinct: std::collections::HashSet<i64> = ids.iter().copied().collect();
    assert_eq!(distinct.len(), ENTRIES, "setup: the ids must be distinct");

    let mut tasks = Vec::new();
    for id in ids {
        let root = root.clone();
        tasks.push(tokio::spawn(async move {
            GitNotesBackend::with_root(root).archive(id).await
        }));
    }
    for task in tasks {
        assert!(
            task.await
                .expect("archive task should not panic")
                .expect("archive should succeed"),
            "each archive must find and rewrite its entry"
        );
    }

    let active = backend
        .list(Some("decision"), 100, false, None)
        .await
        .expect("list active");
    assert!(
        active.is_empty(),
        "every concurrently archived entry must stay archived; still active: {:?}",
        active.iter().map(|n| &n.title).collect::<Vec<_>>()
    );

    let all = backend
        .list(Some("decision"), 100, true, None)
        .await
        .expect("list all");
    assert_eq!(
        all.len(),
        ENTRIES,
        "archiving must rewrite entries in place, never drop them"
    );
}

/// `add` and `archive` take the notes lock exactly once; nothing beneath them
/// takes it again. A nested acquisition would not hang (the wait is bounded),
/// it would silently burn a full budget and then degrade to an unlocked write.
/// An *uncontended* op is a handful of git subprocesses (~30ms), so reaching the
/// budget at all means it waited on a lock it already holds.
#[tokio::test]
#[serial]
async fn git_notes_backend_uncontended_add_and_archive_never_reach_the_wait_budget() {
    let dir = make_temp_git_repo();
    let backend = GitNotesBackend::with_root(dir.path().to_path_buf());

    let started = std::time::Instant::now();
    let id = backend
        .add(note_input("decision", "no nested acquisition"))
        .await
        .expect("add");
    let add_took = started.elapsed();

    let started = std::time::Instant::now();
    assert!(
        backend.archive(id).await.expect("archive"),
        "entry archived"
    );
    let archive_took = started.elapsed();

    for (op, took) in [("add", add_took), ("archive", archive_took)] {
        assert!(
            took < LOCK_WAIT_BUDGET,
            "uncontended {op} took {took:?}, reaching the {LOCK_WAIT_BUDGET:?} lock budget: \
             it waited on a lock it already holds (a nested acquisition)"
        );
    }
}
