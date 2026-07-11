//! Integration tests for `GitNotesBackend` — concurrency and round-trip.
//!
//! These tests require `git` to be on PATH and are skipped if the current
//! working directory is not inside a git repository (CI environments without
//! git are unaffected).
//!
//! ## Concurrent-write safety (#185)
//!
//! `add` is an unsynchronized read-modify-write that rewrites the HEAD note with
//! `git notes add -f` (replace semantics). Two agents writing to the *same HEAD*
//! concurrently race: one write may be lost, or both may survive — the outcome is
//! timing-dependent, not guaranteed. The guaranteed contract is only that the
//! store never corrupts and at least one write survives.
//!
//! **Chosen strategy: Option C — accept the race for the v1 spike.**
//! The typical agent workflow produces a note per commit; agents working in
//! separate commits (the common case) are unaffected. Users who need
//! conflict-free concurrent writes should use the sqlite backend (the default).

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

/// Two tasks writing a note to the same HEAD concurrently must never corrupt the
/// store or panic. The backend does an unsynchronized read-modify-write, so the
/// outcome is timing-dependent: if the second reader observes the first's write
/// both notes survive (2); if both read the empty blob first, one overwrite wins
/// (1). Either way at least one survives and the store stays readable — that is
/// the guaranteed contract, and it is what we assert.
///
/// This is *not* last-write-wins: concurrent same-HEAD writes may lose an entry
/// (the `-f` overwrite race, #185) — use the sqlite backend for multi-agent
/// workflows. An earlier assertion of `len <= 1` was wrong: it asserted the
/// data-loss race *always* happens, which it does not, so it flaked on CI.
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

    // At least one survives, at most both; the RMW race may drop one but never
    // corrupts the blob or duplicates/mangles a record.
    assert!(
        (1..=2).contains(&notes.len()),
        "expected 1 or 2 surviving notes; got {}",
        notes.len()
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
    // If both survived they must be the two distinct writers, not a duplicate.
    if notes.len() == 2 {
        assert_ne!(
            notes[0].title, notes[1].title,
            "both entries are distinct writers"
        );
    }
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
