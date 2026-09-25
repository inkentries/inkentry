//! Integration tests for `inkentry_core::metrics::build_snapshot` (ADR-098,
//! state source only): a temp `MemoryStore` paired with a temp git repo,
//! covering the empty store, no-git-repo, window-boundary, and
//! vector-less-entry cases the ADR calls out by name.

use crate::common;
use inkentry_core::metrics::build_snapshot;
use inkentry_core::storage::MemoryStore;
use std::path::Path;

fn register_sqlite_vec() {
    use std::sync::OnceLock;
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        #[allow(clippy::missing_transmute_annotations)]
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}

fn open_store() -> MemoryStore {
    register_sqlite_vec();
    MemoryStore::open(Path::new(":memory:")).expect("open in-memory memory store")
}

/// A temp directory that is deliberately NOT a git repository.
fn non_git_dir() -> tempfile::TempDir {
    tempfile::TempDir::new().expect("tempdir")
}

/// A temp git repo with one commit dated `commit_time` (unix seconds),
/// returning the dir and the commit's own sha.
fn git_repo_with_commit_at(
    commit_time: i64,
    file: &str,
    contents: &str,
) -> (tempfile::TempDir, String) {
    common::isolate_git_config();
    let dir = tempfile::TempDir::new().expect("tempdir");
    let p = dir.path();
    let date = format!("@{commit_time} +0000");
    let run = |args: &[&str], date_env: bool| {
        let mut cmd = std::process::Command::new("git");
        cmd.args(args).current_dir(p);
        if date_env {
            cmd.env("GIT_AUTHOR_DATE", &date)
                .env("GIT_COMMITTER_DATE", &date);
        }
        let out = cmd.output().expect("git command");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        out
    };
    run(&["init", "-b", "main"], false);
    run(&["config", "user.email", "test@example.com"], false);
    run(&["config", "user.name", "Test"], false);
    std::fs::write(p.join(file), contents).expect("write file");
    run(&["add", "."], false);
    run(
        &[
            "commit",
            "--no-gpg-sign",
            "-m",
            "seed commit",
            "--allow-empty-message",
        ],
        true,
    );
    let sha_out = run(&["rev-parse", "HEAD"], false);
    let sha = String::from_utf8_lossy(&sha_out.stdout).trim().to_string();
    (dir, sha)
}

/// Add another commit at `commit_time` touching `file`, returning its sha.
fn commit_at(root: &Path, commit_time: i64, file: &str, contents: &str) -> String {
    let date = format!("@{commit_time} +0000");
    let run = |args: &[&str], date_env: bool| {
        let mut cmd = std::process::Command::new("git");
        cmd.args(args).current_dir(root);
        if date_env {
            cmd.env("GIT_AUTHOR_DATE", &date)
                .env("GIT_COMMITTER_DATE", &date);
        }
        let out = cmd.output().expect("git command");
        assert!(out.status.success(), "git {:?} failed", args);
        out
    };
    std::fs::write(root.join(file), contents).expect("write file");
    run(&["add", "."], false);
    run(&["commit", "--no-gpg-sign", "-m", "later commit"], true);
    let sha_out = run(&["rev-parse", "HEAD"], false);
    String::from_utf8_lossy(&sha_out.stdout).trim().to_string()
}

const DAY: i64 = 86_400;

// ── no git repo ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn empty_store_outside_a_git_repo_has_no_commit_based_metrics() {
    let store = open_store();
    let dir = non_git_dir();

    let snap = build_snapshot(&store, dir.path(), "proj".into(), "0.0.0-test".into(), 30)
        .await
        .expect("build_snapshot");

    assert!(
        snap.header.commit.is_none(),
        "no git repo => no commit header"
    );
    let json = serde_json::to_value(&snap).unwrap();
    assert!(
        json["state"].get("rec.commit_coverage").is_none(),
        "commit_coverage must be ABSENT (not present-and-zero) outside a git repo: {json}"
    );
    assert!(
        json["state"].get("cmp.lines_per_decision").is_none(),
        "lines_per_decision must be ABSENT outside a git repo: {json}"
    );

    // Empty store: every rate is present with a null value (0/0), never a
    // fabricated number.
    assert_eq!(
        json["state"]["rec.supersede_rate"]["value"],
        serde_json::Value::Null
    );
    assert_eq!(json["state"]["rec.near_duplicate_rate"]["denominator"], 0);
    assert_eq!(json["state"]["rec.unresolved_conflicts"], 0);
}

// ── determinism ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn two_runs_over_the_same_state_are_byte_identical() {
    let store = open_store();
    store
        .add_note("decision", "Use X", "because Y", &[], &[], None, None)
        .unwrap();
    let dir = non_git_dir();

    let a = build_snapshot(&store, dir.path(), "proj".into(), "0.0.0-test".into(), 30)
        .await
        .unwrap();
    let b = build_snapshot(&store, dir.path(), "proj".into(), "0.0.0-test".into(), 30)
        .await
        .unwrap();

    let a_json = serde_json::to_string(&a).unwrap();
    let b_json = serde_json::to_string(&b).unwrap();
    assert_eq!(
        a_json, b_json,
        "same repository state must produce byte-identical output"
    );
}

// ── events block present, no eval, no identifying content ──────────────────

#[tokio::test]
async fn snapshot_carries_a_top_level_events_block_no_eval_and_no_entry_identifiers() {
    let store = open_store();
    store
        .add_note(
            "decision",
            "A very distinctive decision title",
            "a very distinctive decision body",
            &[],
            &[],
            None,
            None,
        )
        .unwrap();
    let dir = non_git_dir();

    let snap = build_snapshot(&store, dir.path(), "proj".into(), "0.0.0-test".into(), 30)
        .await
        .unwrap();
    let json = serde_json::to_value(&snap).unwrap();

    assert!(
        json["state"].get("events").is_none(),
        "the events block lives beside state, not inside it: {json}"
    );
    assert!(
        json["events"].is_object(),
        "the events block is always present, even with zero recorded events: {json}"
    );
    assert!(json.get("eval").is_none(), "no eval block, ever: {json}");

    let rendered = serde_json::to_string(&json).unwrap();
    assert!(
        !rendered.contains("A very distinctive decision title"),
        "snapshot must not carry entry titles: {rendered}"
    );
    assert!(
        !rendered.contains("a very distinctive decision body"),
        "snapshot must not carry entry bodies: {rendered}"
    );
}

// ── events block: seeded rows drive the formulas ────────────────────────────

#[tokio::test]
async fn snapshot_events_block_reflects_recorded_rows_within_its_own_seven_day_window() {
    use inkentry_core::storage::memory::{EventFields, record_event_at};

    register_sqlite_vec();
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let store = MemoryStore::open(tmp.path()).expect("open memory store");
    let dir = non_git_dir();
    let fields = |command: &'static str, trigger: &'static str| EventFields {
        command,
        surface: "cli",
        trigger,
        actor_kind: "human",
        session_ref: None,
        code_results: Some(1),
        memory_results: Some(0),
        returned_ids: None,
        tokens_out: Some(42),
        latency_ms: Some(10),
        ok: true,
    };
    record_event_at(tmp.path(), fields("search", "explicit"));
    record_event_at(tmp.path(), fields("search", "hook"));
    // The window ends at the newest entry's `created_at`, in whole seconds.
    // Added after the events, the entry cannot close the window before them;
    // added first, a second boundary crossed in between left both outside it.
    store
        .add_note("decision", "Use X", "because Y", &[], &[], None, None)
        .unwrap();

    let snap = build_snapshot(&store, dir.path(), "proj".into(), "0.0.0-test".into(), 30)
        .await
        .unwrap();
    let json = serde_json::to_value(&snap).unwrap();

    assert_eq!(json["events"]["calls"]["search"]["total"], 2);
    assert_eq!(json["events"]["calls"]["search"]["explicit"], 1);
    assert_eq!(json["events"]["calls"]["search"]["hook"], 1);
    assert_eq!(json["events"]["auto.read_rate"]["numerator"], 1);
    assert_eq!(json["events"]["auto.read_rate"]["denominator"], 2);
    let rendered = serde_json::to_string(&json).unwrap();
    assert!(
        !rendered.contains("rrftieterm") && rendered.len() < 20_000,
        "sanity: the events block stays small and carries no query text"
    );
}

// ── window boundaries ────────────────────────────────────────────────────────

#[tokio::test]
async fn entries_outside_the_window_are_excluded_from_in_window_counts() {
    let store = open_store();
    let (dir, head_sha) = git_repo_with_commit_at(1_000_000, "a.txt", "a");
    let window_days = 10u32;
    let window_start = 1_000_000 - i64::from(window_days) * DAY;

    // Inside the window.
    store
        .add_note_with_created_at(
            "decision",
            "In window",
            "b",
            &[],
            &[],
            None,
            "active",
            window_start + 10,
        )
        .unwrap();
    // Exactly on the earlier boundary — should still count (inclusive).
    store
        .add_note_with_created_at(
            "decision",
            "On boundary",
            "b",
            &[],
            &[],
            None,
            "active",
            window_start,
        )
        .unwrap();
    // Outside the window (before it starts).
    store
        .add_note_with_created_at(
            "decision",
            "Before window",
            "b",
            &[],
            &[],
            None,
            "active",
            window_start - 10,
        )
        .unwrap();

    let snap = build_snapshot(
        &store,
        dir.path(),
        "proj".into(),
        "0.0.0-test".into(),
        window_days,
    )
    .await
    .unwrap();

    let in_window_decisions = snap.state.rec_entries.in_window["decision"];
    assert_eq!(
        in_window_decisions,
        2,
        "exactly the two in-window (incl. boundary) decisions must count, got snapshot: {:?}",
        serde_json::to_value(&snap.state.rec_entries).unwrap()
    );
    assert_eq!(
        snap.state.rec_entries.total["decision"], 3,
        "total must still count every decision regardless of window"
    );
    // Sanity: HEAD is reachable and its own commit is inside the window.
    assert_eq!(snap.header.commit.unwrap().sha, head_sha);
}

#[tokio::test]
async fn an_entry_recorded_since_the_last_commit_is_inside_the_window() {
    let store = open_store();
    let forty_days_ago = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        - 40 * 86_400;
    let (dir, _) = git_repo_with_commit_at(forty_days_ago, "a.txt", "a");
    store
        .add_note("decision", "Recorded today", "b", &[], &[], None, None)
        .unwrap();

    let snap = build_snapshot(&store, dir.path(), "proj".into(), "0.0.0-test".into(), 30)
        .await
        .unwrap();

    assert_eq!(
        snap.state.rec_entries.in_window["decision"], 1,
        "the window closes at the newest entry when that is later than HEAD"
    );
}

// ── commit coverage: source_ref column match ────────────────────────────────

#[tokio::test]
async fn a_commit_with_a_matching_source_ref_column_counts_as_covered() {
    let store = open_store();
    // The entry below is stamped with the wall clock, and the window closes at
    // the later of HEAD and the newest entry, so the commit has to be recent.
    let an_hour_ago = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        - 3_600;
    let (dir, head_sha) = git_repo_with_commit_at(an_hour_ago, "a.txt", "a");

    store
        .add_note(
            "decision",
            "Harvested from HEAD",
            "b",
            &[],
            &[],
            Some(&head_sha),
            None,
        )
        .unwrap();

    let snap = build_snapshot(&store, dir.path(), "proj".into(), "0.0.0-test".into(), 30)
        .await
        .unwrap();

    let coverage = snap
        .state
        .rec_commit_coverage
        .expect("commit_coverage must be present inside a git repo");
    assert_eq!(coverage.denominator, 1, "exactly one commit in the window");
    assert_eq!(
        coverage.numerator, 1,
        "that commit has a matching source_ref"
    );
}

#[tokio::test]
async fn a_commit_with_no_matching_entry_is_uncovered() {
    let store = open_store();
    let (dir, _head_sha) = git_repo_with_commit_at(2_000_000, "a.txt", "a");

    let snap = build_snapshot(&store, dir.path(), "proj".into(), "0.0.0-test".into(), 30)
        .await
        .unwrap();

    let coverage = snap.state.rec_commit_coverage.unwrap();
    assert_eq!(coverage.denominator, 1);
    assert_eq!(coverage.numerator, 0);
    assert_eq!(coverage.value, Some(0.0));
}

// ── near-duplicate rate: entries without a vector are excluded ─────────────

#[tokio::test]
async fn active_entries_without_a_vector_are_excluded_from_near_duplicate_rate() {
    let store = open_store();
    // No embedding attached to either — both must be excluded, not counted
    // as "not near-duplicate".
    store
        .add_note("note", "First", "body one", &[], &[], None, None)
        .unwrap();
    store
        .add_note("note", "Second", "body two", &[], &[], None, None)
        .unwrap();
    let dir = non_git_dir();

    let snap = build_snapshot(&store, dir.path(), "proj".into(), "0.0.0-test".into(), 30)
        .await
        .unwrap();

    let nd = snap.state.rec_near_duplicate_rate;
    assert_eq!(nd.excluded_without_vector, 2);
    assert_eq!(nd.denominator, 0);
    assert_eq!(nd.numerator, 0);
    assert_eq!(nd.value, None);
}

#[tokio::test]
async fn two_active_entries_with_near_identical_vectors_are_flagged_as_near_duplicates() {
    let store = open_store();
    let (id_a, _) = store
        .add_note("note", "First", "body one", &[], &[], None, None)
        .unwrap();
    let (id_b, _) = store
        .add_note("note", "Second", "body two", &[], &[], None, None)
        .unwrap();
    let (id_c, _) = store
        .add_note("note", "Unrelated", "body three", &[], &[], None, None)
        .unwrap();

    let dim = inkentry_core::embeddings::EMBEDDING_DIM;
    let mut near_a = vec![0.0_f32; dim];
    near_a[0] = 1.0;
    let mut near_b = near_a.clone();
    near_b[1] = 0.01; // cosine-ish distance well under 0.15 from near_a
    let mut far = vec![0.0_f32; dim];
    far[dim - 1] = 1.0;

    store
        .insert_embedding(&id_a, &inkentry_core::embeddings::vec_to_blob(&near_a))
        .unwrap();
    store
        .insert_embedding(&id_b, &inkentry_core::embeddings::vec_to_blob(&near_b))
        .unwrap();
    store
        .insert_embedding(&id_c, &inkentry_core::embeddings::vec_to_blob(&far))
        .unwrap();

    let dir = non_git_dir();
    let snap = build_snapshot(&store, dir.path(), "proj".into(), "0.0.0-test".into(), 30)
        .await
        .unwrap();

    let nd = snap.state.rec_near_duplicate_rate;
    assert_eq!(nd.excluded_without_vector, 0);
    assert_eq!(nd.denominator, 3);
    assert_eq!(
        nd.numerator, 2,
        "the two near-identical entries must be flagged; the unrelated one must not"
    );
}

// ── supersede rate + time-to-supersede ──────────────────────────────────────

#[tokio::test]
async fn a_supersede_inside_the_window_counts_toward_supersede_rate_and_time_to_supersede() {
    let store = open_store();
    // "Active at window start" requires the old entry to have existed BEFORE
    // the window even began, so it is created far earlier than the window
    // the supersede itself falls into (window_end - 30 days).
    let old_created_at = 1_000;
    let new_created_at = 3_000_000; // window_end; window_start = 3_000_000 - 2_592_000 = 408_000
    let (old_id, _) = store
        .add_note_with_created_at(
            "decision",
            "Old",
            "b",
            &[],
            &[],
            None,
            "active",
            old_created_at,
        )
        .unwrap();
    let (_new_id, _) = store
        .add_note_superseding(
            "decision",
            "New",
            "b2",
            &[],
            &[],
            Some(new_created_at),
            &old_id,
        )
        .unwrap();
    // Force created_at on the superseding row so the delta is exact and
    // known, rather than "whenever the test ran".
    store
        .execute_batch(&format!(
            "UPDATE notes SET created_at = {new_created_at} WHERE title = 'New'"
        ))
        .unwrap();
    // And pin the supersede edge's own created_at inside the window too.
    store
        .execute_batch(&format!(
            "UPDATE memory_edges SET created_at = {new_created_at} WHERE kind = 'supersedes'"
        ))
        .unwrap();

    let dir = non_git_dir();
    let snap = build_snapshot(&store, dir.path(), "proj".into(), "0.0.0-test".into(), 30)
        .await
        .unwrap();

    // window_end defaults to the newest entry's created_at since there is no
    // git repo; the supersede lands exactly at window_end, well inside the
    // window, and the old decision was active at window_start.
    assert_eq!(snap.state.rec_supersede_rate.numerator, 1);
    assert_eq!(snap.state.rec_supersede_rate.denominator, 1);
    assert_eq!(snap.state.rec_time_to_supersede_p50.sample_size, 1);
    assert_eq!(
        snap.state.rec_time_to_supersede_p50.median_seconds,
        Some(new_created_at - old_created_at)
    );
}

// ── open question age, closest honest definition ────────────────────────────

#[tokio::test]
async fn a_question_with_no_related_answer_is_open_and_ages_from_window_end() {
    let store = open_store();
    store
        .add_note_with_created_at(
            "question",
            "Unanswered",
            "b",
            &[],
            &[],
            None,
            "active",
            1_000_000,
        )
        .unwrap();
    let dir = non_git_dir();

    let snap = build_snapshot(&store, dir.path(), "proj".into(), "0.0.0-test".into(), 30)
        .await
        .unwrap();

    // window_end anchors to the newest entry's created_at (1_000_000, the
    // only entry) since there is no git repo, so the question's age is 0.
    assert_eq!(snap.state.rec_open_question_age_p50.sample_size, 1);
    assert_eq!(snap.state.rec_open_question_age_p50.median_seconds, Some(0));
}

#[tokio::test]
async fn a_question_related_to_an_answer_is_not_counted_as_open() {
    let store = open_store();
    let (q_id, _) = store
        .add_note_with_created_at(
            "question",
            "Answered",
            "b",
            &[],
            &[],
            None,
            "active",
            1_000_000,
        )
        .unwrap();
    let (a_id, _) = store
        .add_note_with_created_at(
            "answer",
            "The answer",
            "b",
            &[],
            &[],
            None,
            "active",
            1_000_100,
        )
        .unwrap();
    store.add_edge(&a_id, &q_id, "relates_to").unwrap();

    let dir = non_git_dir();
    let snap = build_snapshot(&store, dir.path(), "proj".into(), "0.0.0-test".into(), 30)
        .await
        .unwrap();

    assert_eq!(
        snap.state.rec_open_question_age_p50.sample_size, 0,
        "a question related to an answer entry must not be counted as open"
    );
}

// ── unresolved conflicts ─────────────────────────────────────────────────────

#[tokio::test]
async fn a_contradicts_edge_between_two_active_entries_is_an_unresolved_conflict() {
    let store = open_store();
    let (a_id, _) = store
        .add_note("decision", "A", "b", &[], &[], None, None)
        .unwrap();
    let (b_id, _) = store
        .add_note("decision", "B", "b", &[], &[], None, None)
        .unwrap();
    store.add_edge(&a_id, &b_id, "contradicts").unwrap();

    let dir = non_git_dir();
    let snap = build_snapshot(&store, dir.path(), "proj".into(), "0.0.0-test".into(), 30)
        .await
        .unwrap();

    assert_eq!(snap.state.rec_unresolved_conflicts, 1);
}

#[tokio::test]
async fn a_contradicts_edge_where_one_side_was_superseded_is_resolved() {
    let store = open_store();
    let (a_id, _) = store
        .add_note("decision", "A", "b", &[], &[], None, None)
        .unwrap();
    let (b_id, _) = store
        .add_note("decision", "B", "b", &[], &[], None, None)
        .unwrap();
    store.add_edge(&a_id, &b_id, "contradicts").unwrap();
    store
        .add_note_superseding("decision", "A2", "b3", &[], &[], None, &a_id)
        .unwrap();

    let dir = non_git_dir();
    let snap = build_snapshot(&store, dir.path(), "proj".into(), "0.0.0-test".into(), 30)
        .await
        .unwrap();

    assert_eq!(
        snap.state.rec_unresolved_conflicts, 0,
        "a superseded endpoint resolves the conflict"
    );
}

// ── cmp.lines_per_decision via git log --numstat ────────────────────────────

#[tokio::test]
async fn lines_per_decision_sums_numstat_across_commits_in_the_window() {
    let store = open_store();
    let (dir, _head_sha) = git_repo_with_commit_at(3_000_000, "a.txt", "one\ntwo\nthree\n");
    // A second commit adding two more lines to the same file.
    commit_at(
        dir.path(),
        3_000_100,
        "a.txt",
        "one\ntwo\nthree\nfour\nfive\n",
    );

    store
        .add_note_with_created_at("decision", "D1", "b", &[], &[], None, "active", 3_000_050)
        .unwrap();

    let snap = build_snapshot(&store, dir.path(), "proj".into(), "0.0.0-test".into(), 30)
        .await
        .unwrap();

    let lpd = snap
        .state
        .cmp_lines_per_decision
        .expect("present inside a git repo");
    assert_eq!(lpd.denominator, 1, "one decision recorded in the window");
    // First commit adds 3 lines (new file); second adds 2 more. Total: 5.
    assert_eq!(lpd.numerator, 5, "lines added+removed across both commits");
}

// ── cmp.review_items_per_day ─────────────────────────────────────────────────

#[tokio::test]
async fn review_items_per_day_counts_the_four_review_kinds_in_the_window() {
    let store = open_store();
    for kind in ["decision", "requirement", "question", "antipattern", "note"] {
        store
            .add_note(kind, &format!("{kind} title"), "b", &[], &[], None, None)
            .unwrap();
    }
    let dir = non_git_dir();

    let snap = build_snapshot(&store, dir.path(), "proj".into(), "0.0.0-test".into(), 10)
        .await
        .unwrap();

    // 4 of the 5 seeded kinds are review kinds; `note` is not.
    assert_eq!(snap.state.cmp_review_items_per_day.numerator, 4);
    assert_eq!(snap.state.cmp_review_items_per_day.denominator, 10);
}

// ── cmp.tokens_context_estimate ──────────────────────────────────────────────

#[tokio::test]
async fn tokens_context_estimate_reflects_the_default_sections_only() {
    let store = open_store();
    store
        .add_note("decision", "D", "decision body text", &[], &[], None, None)
        .unwrap();
    // `note` is not one of context's default sections and must not be counted.
    store
        .add_note(
            "note",
            "N",
            "a very long note body that would inflate the estimate a lot if counted",
            &[],
            &[],
            None,
            None,
        )
        .unwrap();
    let dir = non_git_dir();

    let empty_store = open_store();
    let baseline = build_snapshot(
        &empty_store,
        dir.path(),
        "proj".into(),
        "0.0.0-test".into(),
        30,
    )
    .await
    .unwrap();
    let snap = build_snapshot(&store, dir.path(), "proj".into(), "0.0.0-test".into(), 30)
        .await
        .unwrap();

    assert_eq!(baseline.state.cmp_tokens_context_estimate, 0);
    assert!(
        snap.state.cmp_tokens_context_estimate > 0,
        "the seeded decision must contribute tokens"
    );
}

// ── header shape ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn header_carries_schema_window_and_embedder_facts() {
    let store = open_store();
    let dir = non_git_dir();

    let snap = build_snapshot(&store, dir.path(), "my-proj".into(), "1.2.3".into(), 7)
        .await
        .unwrap();

    assert_eq!(snap.schema, "inkentry.metrics/1");
    assert_eq!(snap.header.project, "my-proj");
    assert_eq!(snap.header.inkentry_version, "1.2.3");
    assert_eq!(snap.header.window_days, 7);
    assert_eq!(
        snap.header.embedder.dim,
        inkentry_core::embeddings::EMBEDDING_DIM
    );
}
