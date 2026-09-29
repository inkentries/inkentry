// `memory add` computes duplicate/related candidates before it writes, and
// (opt-in) blocks on an unresolved duplicate-band one.
//
// Mirrors `memory_reindex.rs`'s fixture: a mock `index/embed` route standing
// in for the loopback embedder, reached via the fixed-port discovery
// override rather than the real `server.port` file.

use crate::plumbing_helpers;
use plumbing_helpers::{inkentry_bin, mount_health};

use assert_cmd::Command;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tempfile::TempDir;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn ensure_sqlite_vec() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        #[allow(clippy::missing_transmute_annotations)]
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}

// A constant embedding vector (every dimension = `value`), so the L2
// distance between two entries is `|v1 - v2| * sqrt(896)` — controllable
// precisely enough to land in a chosen band. `~0.005` apart clears the
// duplicate floor (0.15 L2), `~0.04` apart clears the related ceiling
// (1.2032 L2) without clearing the duplicate one, and `1.0` apart clears
// neither.
#[derive(Clone, Copy)]
struct ConstEmbed(f32);

impl wiremock::Respond for ConstEmbed {
    fn respond(&self, _req: &wiremock::Request) -> ResponseTemplate {
        let mut bytes = Vec::with_capacity(896 * 4);
        for _ in 0..896 {
            bytes.extend_from_slice(&self.0.to_le_bytes());
        }
        ResponseTemplate::new(200)
            .insert_header("content-type", "application/octet-stream")
            .set_body_bytes(bytes)
    }
}

struct MockServerHandle {
    _rt: tokio::runtime::Runtime,
    server: MockServer,
}

impl MockServerHandle {
    fn uri(&self) -> String {
        self.server.uri()
    }
}

// Each entry embedded through this fixture gets `base_value + i * step` in
// arrival order, letting one server stand in for entries at controlled
// distances from each other without per-call routing.
fn start_mock(values: Vec<f32>) -> MockServerHandle {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let server = rt.block_on(async {
        let server = MockServer::start().await;
        mount_health(&server).await;
        for v in values {
            Mock::given(method("POST"))
                .and(path_regex(r"^/v1/projects/.+/index/embed$"))
                .respond_with(ConstEmbed(v))
                .up_to_n_times(1)
                .mount(&server)
                .await;
        }
        server
    });
    MockServerHandle { _rt: rt, server }
}

struct Fixture {
    _tmp: TempDir,
    project_dir: PathBuf,
    mem_path: PathBuf,
    global_config: PathBuf,
    state_dir: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = TempDir::new().expect("tempdir");
    let project_dir = tmp.path().to_path_buf();
    let inkentry_dir = project_dir.join(".inkentry");
    std::fs::create_dir_all(&inkentry_dir).expect("create .inkentry");
    let mem_path = inkentry_dir.join("memory.db");
    let index_db = inkentry_dir.join("index.db");
    let global_config = project_dir.join("global-config.toml");
    std::fs::write(
        &global_config,
        format!(
            "db_path = {:?}\nstore_in_git_notes = false\n",
            index_db.display().to_string()
        ),
    )
    .expect("write global config");
    let state_dir = project_dir.join("state");
    std::fs::create_dir_all(&state_dir).expect("create state dir");
    Fixture {
        _tmp: tmp,
        project_dir,
        mem_path,
        global_config,
        state_dir,
    }
}

fn add_cmd(f: &Fixture, server_uri: &str) -> Command {
    let port = server_uri
        .rsplit(':')
        .next()
        .expect("uri has a port")
        .trim_end_matches('/')
        .to_string();
    let mut cmd = inkentry_bin();
    cmd.current_dir(&f.project_dir)
        .env("INKENTRY_STATE_DIR", &f.state_dir)
        .env("INKENTRY_TEST_DISCOVERY_PORT", port)
        .arg("--config")
        .arg(&f.global_config)
        .arg("memory")
        .arg("--db")
        .arg(&f.mem_path)
        .arg("add");
    cmd
}

fn no_server_add_cmd(f: &Fixture) -> Command {
    let mut cmd = inkentry_bin();
    cmd.current_dir(&f.project_dir)
        .env("INKENTRY_NO_SERVER", "1")
        .env_remove("INKENTRY_SERVER_URL")
        .arg("--config")
        .arg(&f.global_config)
        .arg("memory")
        .arg("--db")
        .arg(&f.mem_path)
        .arg("add");
    cmd
}

fn note_uuid_by_title(mem_path: &Path, title: &str) -> String {
    ensure_sqlite_vec();
    let conn = Connection::open(mem_path).expect("open memory.db");
    conn.query_row(
        "SELECT uuid FROM notes WHERE title = ?1",
        rusqlite::params![title],
        |r| r.get(0),
    )
    .expect("note uuid by title")
}

fn note_status(mem_path: &Path, uuid: &str) -> String {
    let conn = Connection::open(mem_path).expect("open memory.db");
    conn.query_row(
        "SELECT status FROM notes WHERE uuid = ?1",
        rusqlite::params![uuid],
        |r| r.get(0),
    )
    .expect("note status by uuid")
}

fn note_count(mem_path: &Path) -> i64 {
    ensure_sqlite_vec();
    let conn = Connection::open(mem_path).expect("open memory.db");
    conn.query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))
        .expect("note count")
}

#[derive(Debug, PartialEq)]
struct AddEvent {
    reconcile: Option<String>,
    resolution: Option<String>,
    ok: bool,
    memory_results: Option<i64>,
}

fn add_events(mem_path: &Path) -> Vec<AddEvent> {
    ensure_sqlite_vec();
    let conn = Connection::open(mem_path).expect("open memory.db");
    let mut stmt = conn
        .prepare(
            "SELECT reconcile, resolution, ok, memory_results FROM events \
             WHERE command = 'memory.add' ORDER BY rowid",
        )
        .expect("prepare events query");
    stmt.query_map([], |r| {
        Ok(AddEvent {
            reconcile: r.get(0)?,
            resolution: r.get(1)?,
            ok: r.get::<_, i64>(2)? != 0,
            memory_results: r.get(3)?,
        })
    })
    .expect("query events")
    .collect::<rusqlite::Result<_>>()
    .expect("collect events")
}

fn event(reconcile: &str, resolution: Option<&str>, ok: bool, results: i64) -> AddEvent {
    AddEvent {
        reconcile: Some(reconcile.to_string()),
        resolution: resolution.map(str::to_string),
        ok,
        memory_results: Some(results),
    }
}

#[test]
fn reconcile_block_with_a_duplicate_and_no_resolution_writes_nothing() {
    let f = fixture();
    let server = start_mock(vec![0.0100, 0.0102]);

    add_cmd(&f, &server.uri())
        .args(["--kind", "decision", "--title", "First", "--body", "b1"])
        .assert()
        .success();
    assert_eq!(note_count(&f.mem_path), 1);

    let out = add_cmd(&f, &server.uri())
        .args([
            "--kind",
            "decision",
            "--title",
            "First restated",
            "--body",
            "b2",
            "--reconcile",
            "--format",
            "json",
        ])
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();

    assert_eq!(
        note_count(&f.mem_path),
        1,
        "a blocked write must not add a row"
    );
    let body: serde_json::Value = serde_json::from_slice(&out).expect("stdout is json");
    assert_eq!(body["created"], serde_json::json!(false));
    assert_eq!(body["reason"], serde_json::json!("candidates"));
    let candidates = body["candidates"].as_array().expect("candidates array");
    assert!(!candidates.is_empty());
    assert_eq!(candidates[0]["band"], serde_json::json!("duplicate"));
}

#[test]
fn reconcile_block_with_a_supersedes_resolution_writes_and_archives_old() {
    let f = fixture();
    let server = start_mock(vec![0.0100, 0.0102]);

    add_cmd(&f, &server.uri())
        .args([
            "--kind",
            "decision",
            "--title",
            "Old policy",
            "--body",
            "b1",
        ])
        .assert()
        .success();
    let old_id = note_uuid_by_title(&f.mem_path, "Old policy");

    add_cmd(&f, &server.uri())
        .args([
            "--kind",
            "decision",
            "--title",
            "Old policy restated",
            "--body",
            "b2",
            "--reconcile",
            "--supersedes",
            &old_id,
        ])
        .assert()
        .success();

    assert_eq!(note_count(&f.mem_path), 2);
    assert_eq!(note_status(&f.mem_path, &old_id), "archived");
}

#[test]
fn without_reconcile_a_duplicate_is_written_with_candidates_in_the_response() {
    let f = fixture();
    let server = start_mock(vec![0.0100, 0.0102]);

    add_cmd(&f, &server.uri())
        .args(["--kind", "note", "--title", "Alpha", "--body", "b1"])
        .assert()
        .success();

    let out = add_cmd(&f, &server.uri())
        .args([
            "--kind",
            "note",
            "--title",
            "Alpha restated",
            "--body",
            "b2",
            "--format",
            "json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    assert_eq!(
        note_count(&f.mem_path),
        2,
        "without --reconcile a duplicate is written like any other entry"
    );
    let body: serde_json::Value = serde_json::from_slice(&out).expect("stdout is json");
    assert_eq!(body["created"], serde_json::json!(true));
    let candidates = body["candidates"].as_array().expect("candidates array");
    assert_eq!(candidates[0]["band"], serde_json::json!("duplicate"));
}

#[test]
fn a_related_but_not_duplicate_entry_is_reported_as_related_and_never_blocks() {
    let f = fixture();
    // ~0.04 apart clears the related ceiling but not the duplicate floor.
    let server = start_mock(vec![0.0100, 0.0500]);

    add_cmd(&f, &server.uri())
        .args(["--kind", "note", "--title", "Retry backoff", "--body", "b1"])
        .assert()
        .success();

    let out = add_cmd(&f, &server.uri())
        .args([
            "--kind",
            "note",
            "--title",
            "Retry jitter",
            "--body",
            "b2",
            "--reconcile",
            "--format",
            "json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    assert_eq!(note_count(&f.mem_path), 2, "a related entry never blocks");
    let body: serde_json::Value = serde_json::from_slice(&out).expect("stdout is json");
    assert!(
        body.get("candidates")
            .is_none_or(|c| c.as_array().unwrap().is_empty()),
        "a related-only candidate must not appear in the duplicate-band candidates field"
    );
    let related = body["related"].as_array().expect("related array");
    assert_eq!(related[0]["band"], serde_json::json!("related"));
}

#[test]
fn a_resolution_naming_an_id_outside_the_candidate_set_is_accepted() {
    let f = fixture();
    // 1.0 apart is far outside both bands: the second write reports no
    // candidates against the first at all.
    let server = start_mock(vec![0.0100, 1.0100]);

    add_cmd(&f, &server.uri())
        .args(["--kind", "note", "--title", "Unrelated", "--body", "b1"])
        .assert()
        .success();
    let unrelated_id = note_uuid_by_title(&f.mem_path, "Unrelated");

    add_cmd(&f, &server.uri())
        .args([
            "--kind",
            "note",
            "--title",
            "Something else",
            "--body",
            "b2",
            "--reconcile",
            "--distinct-from",
            &unrelated_id,
        ])
        .assert()
        .success();

    assert_eq!(note_count(&f.mem_path), 2);
}

#[test]
fn config_reconcile_block_blocks_without_the_flag() {
    let f = fixture();
    let server = start_mock(vec![0.0100, 0.0102]);
    std::fs::write(
        f.project_dir.join(".inkentry").join("config.toml"),
        "[memory]\nreconcile = \"block\"\n",
    )
    .expect("write project config");

    add_cmd(&f, &server.uri())
        .args(["--kind", "note", "--title", "Policy", "--body", "b1"])
        .assert()
        .success();

    add_cmd(&f, &server.uri())
        .args([
            "--kind",
            "note",
            "--title",
            "Policy restated",
            "--body",
            "b2",
        ])
        .assert()
        .code(3);

    assert_eq!(
        note_count(&f.mem_path),
        1,
        "the project config's reconcile = \"block\" must block without --reconcile too"
    );
}

// An unreachable embedder falls back to FTS-only candidates (related band
// only, never duplicate), and the write proceeds regardless of --reconcile.
#[test]
fn no_embedder_reachable_falls_back_to_fts_only_and_the_write_proceeds() {
    let f = fixture();

    no_server_add_cmd(&f)
        .args([
            "--kind",
            "decision",
            "--title",
            "Use exponential backoff for retries",
            "--body",
            "b1",
        ])
        .assert()
        .success();

    let out = no_server_add_cmd(&f)
        .args([
            "--kind",
            "decision",
            "--title",
            "Use exponential backoff for retries",
            "--body",
            "b2",
            "--reconcile",
            "--format",
            "json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    assert_eq!(
        note_count(&f.mem_path),
        2,
        "an FTS-only candidate must never block, even under --reconcile"
    );
    let body: serde_json::Value = serde_json::from_slice(&out).expect("stdout is json");
    assert!(
        body.get("candidates")
            .is_none_or(|c| c.as_array().unwrap().is_empty()),
        "an FTS-only match can only ever be related, never duplicate"
    );
    let related = body["related"].as_array().expect("related array");
    assert_eq!(related[0]["band"], serde_json::json!("related"));
    assert!(
        related[0].get("distance").is_none(),
        "an FTS-only candidate carries no distance"
    );
}

#[test]
fn a_blocked_write_is_recorded_as_abandoned_and_not_ok_with_the_duplicate_count() {
    let f = fixture();
    let server = start_mock(vec![0.0100, 0.0102]);

    add_cmd(&f, &server.uri())
        .args(["--kind", "decision", "--title", "First", "--body", "b1"])
        .assert()
        .success();
    add_cmd(&f, &server.uri())
        .args([
            "--kind",
            "decision",
            "--title",
            "First restated",
            "--body",
            "b2",
            "--reconcile",
        ])
        .assert()
        .code(3);

    assert_eq!(
        add_events(&f.mem_path),
        vec![
            event("off", None, true, 1),
            event("block", Some("abandoned"), false, 1),
        ]
    );
}

#[test]
fn a_blocked_write_under_config_block_records_the_block_mode() {
    let f = fixture();
    let server = start_mock(vec![0.0100, 0.0102]);
    std::fs::write(
        f.project_dir.join(".inkentry").join("config.toml"),
        "[memory]\nreconcile = \"block\"\n",
    )
    .expect("write project config");

    add_cmd(&f, &server.uri())
        .args(["--kind", "note", "--title", "Policy", "--body", "b1"])
        .assert()
        .success();
    add_cmd(&f, &server.uri())
        .args(["--kind", "note", "--title", "Policy again", "--body", "b2"])
        .assert()
        .code(3);

    assert_eq!(
        add_events(&f.mem_path),
        vec![
            event("block", None, true, 1),
            event("block", Some("abandoned"), false, 1),
        ]
    );
}

#[test]
fn a_write_that_resolves_a_block_records_the_resolution_kind() {
    let f = fixture();
    let server = start_mock(vec![0.0100, 0.0102]);

    add_cmd(&f, &server.uri())
        .args([
            "--kind",
            "decision",
            "--title",
            "Old policy",
            "--body",
            "b1",
        ])
        .assert()
        .success();
    let old_id = note_uuid_by_title(&f.mem_path, "Old policy");
    add_cmd(&f, &server.uri())
        .args([
            "--kind",
            "decision",
            "--title",
            "Old policy restated",
            "--body",
            "b2",
            "--reconcile",
            "--supersedes",
            &old_id,
        ])
        .assert()
        .success();

    assert_eq!(
        add_events(&f.mem_path),
        vec![
            event("off", None, true, 1),
            event("block", Some("supersedes"), true, 1),
        ]
    );
}

#[test]
fn a_plain_write_with_a_duplicate_present_records_no_resolution() {
    let f = fixture();
    let server = start_mock(vec![0.0100, 0.0102]);

    add_cmd(&f, &server.uri())
        .args(["--kind", "note", "--title", "Alpha", "--body", "b1"])
        .assert()
        .success();
    add_cmd(&f, &server.uri())
        .args([
            "--kind",
            "note",
            "--title",
            "Alpha restated",
            "--body",
            "b2",
        ])
        .assert()
        .success();

    assert_eq!(
        add_events(&f.mem_path),
        vec![event("off", None, true, 1), event("off", None, true, 1)]
    );
}

#[test]
fn a_related_only_write_under_reconcile_records_block_mode_and_no_resolution() {
    let f = fixture();
    let server = start_mock(vec![0.0100, 0.0500]);

    add_cmd(&f, &server.uri())
        .args(["--kind", "note", "--title", "Retry backoff", "--body", "b1"])
        .assert()
        .success();
    add_cmd(&f, &server.uri())
        .args([
            "--kind",
            "note",
            "--title",
            "Retry jitter",
            "--body",
            "b2",
            "--reconcile",
        ])
        .assert()
        .success();

    assert_eq!(
        add_events(&f.mem_path),
        vec![event("off", None, true, 1), event("block", None, true, 1)]
    );
}

#[test]
fn each_resolution_flag_is_recorded_as_its_kind() {
    let f = fixture();
    no_server_add_cmd(&f)
        .args(["--kind", "note", "--title", "Target", "--body", "b0"])
        .assert()
        .success();
    let target = note_uuid_by_title(&f.mem_path, "Target");

    for (i, (flag, kind)) in [
        ("--relates-to", "relates_to"),
        ("--contradicts", "contradicts"),
        ("--distinct-from", "distinct"),
        ("--supersedes", "supersedes"),
    ]
    .into_iter()
    .enumerate()
    {
        let title = format!("Resolved {i}");
        no_server_add_cmd(&f)
            .args(["--kind", "note", "--title", &title, "--body", "b"])
            .args(["--reconcile", flag, &target])
            .assert()
            .success();
        let recorded = add_events(&f.mem_path);
        assert_eq!(
            recorded.last().and_then(|e| e.resolution.as_deref()),
            Some(kind),
            "{flag}"
        );
        assert_eq!(
            recorded.last().and_then(|e| e.reconcile.as_deref()),
            Some("block")
        );
    }
}

#[test]
fn a_write_naming_several_resolutions_records_the_supersede() {
    let f = fixture();
    no_server_add_cmd(&f)
        .args(["--kind", "note", "--title", "Target", "--body", "b0"])
        .assert()
        .success();
    let target = note_uuid_by_title(&f.mem_path, "Target");

    no_server_add_cmd(&f)
        .args(["--kind", "note", "--title", "Both", "--body", "b"])
        .args(["--relates-to", &target, "--supersedes", &target])
        .assert()
        .success();

    let recorded = add_events(&f.mem_path);
    assert_eq!(
        recorded.last().unwrap().resolution.as_deref(),
        Some("supersedes")
    );
    assert_eq!(recorded.last().unwrap().reconcile.as_deref(), Some("off"));
}
