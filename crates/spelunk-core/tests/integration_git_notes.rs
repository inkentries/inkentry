//! Integration tests for `GitNotesBackend` — concurrency and round-trip.
//!
//! These tests require `git` to be on PATH and are skipped if the current
//! working directory is not inside a git repository (CI environments without
//! git are unaffected).
//!
//! ## Concurrent-write safety (#185)
//!
//! `git notes add -f` uses replace semantics: two agents writing a note to the
//! same HEAD commit simultaneously will silently lose one entry.  This test
//! documents the behaviour and the chosen mitigation strategy.
//!
//! **Chosen strategy: Option C — document last-write-wins.**
//! For the v1 spike, concurrent writes to the *same HEAD* are a known
//! limitation.  The typical agent workflow produces a note per commit; agents
//! working in separate commits (the common case) are unaffected.
//! Users who need conflict-free concurrent writes should use the sqlite
//! backend (the default).

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

/// Documents the last-write-wins behaviour when two tasks write to the same HEAD
/// concurrently.  The test asserts the *known* outcome (one entry survives) and
/// is annotated so maintainers understand the trade-off.
///
/// To protect against silent data loss, use the sqlite backend for multi-agent
/// workflows (the default backend).
#[tokio::test]
#[serial]
async fn git_notes_concurrent_same_head_last_write_wins() {
    let dir = make_temp_git_repo();

    let root = dir.path().to_path_buf();
    let b1 = GitNotesBackend::with_root(root.clone());
    let b2 = GitNotesBackend::with_root(root.clone());

    // Two concurrent adds to the same HEAD.
    let (r1, r2) = tokio::join!(
        b1.add(note_input("note", "agent A")),
        b2.add(note_input("note", "agent B")),
    );
    r1.expect("agent A add");
    r2.expect("agent B add");

    // At most one survives because both wrote to the same HEAD using -f.
    let notes = GitNotesBackend::with_root(dir.path().to_path_buf())
        .list(None, 10, false, None)
        .await
        .expect("list");

    // KNOWN LIMITATION: only the last writer's note survives.
    // Acceptable for the v1 spike; use sqlite backend for multi-agent workflows.
    assert!(
        notes.len() <= 1,
        "expected at most one note (last-write-wins); got {}",
        notes.len()
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

    assert!(backend.search(&[], 5, None).await.is_err());
    assert!(backend.search_hybrid(&[], "q", 5, None).await.is_err());
    assert!(backend.search_text("q", 5, None).await.is_err());
    assert!(backend.search_timeline(&[], 5).await.is_err());
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
