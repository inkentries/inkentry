// Exit-code contract for the plumbing commands (docs/stability.md).
//
// The 0/1/2 split is the part of the plumbing interface a shell script depends
// on most directly, and it is the easiest to break by accident: exit 2 is a
// single catch-all in `main.rs`, and every exit 1 is an inline
// `std::process::exit(1)` inside its own handler. Nothing but these tests keeps
// an empty result from starting to look like a failure.
//
//   0: succeeded, one or more results emitted
//   1: succeeded, no results (an empty set, not an error)
//   2: hard error; diagnostics on stderr, nothing on stdout
//
// Three commands cannot reach 1 by construction. Those are asserted as
// deliberate exceptions rather than quietly skipped.

mod plumbing_helpers;

use plumbing_helpers::{
    FIXTURE_PROJECT_ID, IndexEmbedResponder, index_fixture_project, init_git_repo, inkentry_bin,
    inkentry_cmd, write_config, write_project_server_config,
};

use std::path::{Path, PathBuf};
use tempfile::TempDir;

const FIXTURE_EMBEDDING_DIM: usize = 896;

const EMPTY: i32 = 1;
const HARD_ERROR: i32 = 2;

// A project directory with no index at all, for the "missing DB" error path.
fn unindexed_project() -> (TempDir, PathBuf, PathBuf) {
    let tmp = TempDir::new().expect("create temp dir");
    let db_path = tmp.path().join("index.db");
    let config_path = write_config(tmp.path(), &db_path, "http://127.0.0.1:1");
    (tmp, db_path, config_path)
}

fn assert_exit(label: &str, output: &std::process::Output, expected: i32) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "{label}: expected exit {expected}, got {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

// Exit 2 promises diagnostics on stderr and nothing on stdout, so a consumer
// that pipes stdout into a JSON parser never sees a half-written record.
fn assert_hard_error(label: &str, output: &std::process::Output) {
    assert_exit(label, output, HARD_ERROR);
    assert!(
        output.stdout.is_empty(),
        "{label}: exit 2 must leave stdout empty, got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        !output.stderr.is_empty(),
        "{label}: exit 2 must write a diagnostic to stderr"
    );
}

// Exit 1 is an empty result, not an error, so it must not print a stack of
// error text that a caller would mistake for a failure.
fn assert_empty(label: &str, output: &std::process::Output) {
    assert_exit(label, output, EMPTY);
    assert!(
        output.stdout.is_empty(),
        "{label}: exit 1 means no results, so stdout must be empty, got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
}

// ── the three codes are distinct, and 2 covers usage errors too ──────────────

// One command, all three codes, in one place. The per-command tests below check
// each code where it is reachable, but nothing there would notice if two of the
// three collapsed onto the same number, which is the break that silently turns
// "no results" into "failed" for every script downstream.
#[test]
fn the_three_exit_codes_are_distinct_for_a_single_command() {
    let (_tmp, db, cfg) = index_fixture_project();

    let results = inkentry_cmd(&db, &cfg)
        .args(["cat-chunks", "src/lib.rs"])
        .output()
        .unwrap();
    let empty = inkentry_cmd(&db, &cfg)
        .args(["cat-chunks", "src/never_indexed.rs"])
        .output()
        .unwrap();
    let (_t2, missing_db, cfg2) = unindexed_project();
    let error = inkentry_cmd(&missing_db, &cfg2)
        .args(["cat-chunks", "src/lib.rs"])
        .output()
        .unwrap();

    let codes: Vec<Option<i32>> = vec![
        results.status.code(),
        empty.status.code(),
        error.status.code(),
    ];
    assert_eq!(
        codes,
        vec![Some(0), Some(EMPTY), Some(HARD_ERROR)],
        "results, empty, and error must land on three different codes"
    );
    assert!(!results.stdout.is_empty(), "exit 0 carries the results");
    assert!(empty.stdout.is_empty(), "exit 1 carries none");
    assert!(error.stdout.is_empty(), "exit 2 carries none");
}

// A usage error is not an empty result set, so clap's own exit path has to land
// on 2 as well. If it ever returned 1, a script would read a typo'd flag as
// "the query matched nothing" and carry on.
#[test]
fn an_unknown_flag_is_a_hard_error_not_an_empty_result() {
    let (_tmp, db, cfg) = index_fixture_project();

    for args in [
        vec!["ls-files", "--no-such-flag"],
        vec!["knn", "--limit", "not-a-number"],
    ] {
        let out = inkentry_cmd(&db, &cfg).args(&args).output().unwrap();
        assert_hard_error(&format!("plumbing {}", args.join(" ")), &out);
    }
}

// ── cat-chunks ───────────────────────────────────────────────────────────────

#[test]
fn cat_chunks_exit_codes() {
    let (_tmp, db, cfg) = index_fixture_project();

    let ok = inkentry_cmd(&db, &cfg)
        .args(["cat-chunks", "src/lib.rs"])
        .output()
        .unwrap();
    assert_exit("cat-chunks results", &ok, 0);
    assert!(!ok.stdout.is_empty(), "exit 0 must emit at least one row");

    let empty = inkentry_cmd(&db, &cfg)
        .args(["cat-chunks", "src/never_indexed.rs"])
        .output()
        .unwrap();
    assert_empty("cat-chunks unknown file", &empty);

    let (_t2, missing_db, cfg2) = unindexed_project();
    let err = inkentry_cmd(&missing_db, &cfg2)
        .args(["cat-chunks", "src/lib.rs"])
        .output()
        .unwrap();
    assert_hard_error("cat-chunks no index", &err);
}

// ── ls-files ─────────────────────────────────────────────────────────────────

#[test]
fn ls_files_exit_codes() {
    let (_tmp, db, cfg) = index_fixture_project();

    let ok = inkentry_cmd(&db, &cfg).arg("ls-files").output().unwrap();
    assert_exit("ls-files results", &ok, 0);
    assert!(!ok.stdout.is_empty(), "exit 0 must emit at least one row");

    // A prefix nothing matches is an empty set, not a failure.
    let empty = inkentry_cmd(&db, &cfg)
        .args(["ls-files", "--prefix", "no/such/directory/"])
        .output()
        .unwrap();
    assert_empty("ls-files unmatched prefix", &empty);

    let (_t2, missing_db, cfg2) = unindexed_project();
    let err = inkentry_cmd(&missing_db, &cfg2)
        .arg("ls-files")
        .output()
        .unwrap();
    assert_hard_error("ls-files no index", &err);
}

// ── parse-file ───────────────────────────────────────────────────────────────

#[test]
fn parse_file_exit_codes() {
    let (_tmp, db, cfg) = unindexed_project();
    let source = plumbing_helpers::fixture_path().join("src/lib.rs");

    let ok = inkentry_cmd(&db, &cfg)
        .arg("parse-file")
        .arg(&source)
        .output()
        .unwrap();
    assert_exit("parse-file results", &ok, 0);
    assert!(!ok.stdout.is_empty(), "exit 0 must emit at least one row");

    // An unrecognised extension yields no chunks; that is an empty set.
    let unsupported = _tmp.path().join("payload.bin");
    std::fs::write(&unsupported, [0u8, 1, 2, 3]).unwrap();
    let empty = inkentry_cmd(&db, &cfg)
        .arg("parse-file")
        .arg(&unsupported)
        .output()
        .unwrap();
    assert_empty("parse-file unsupported type", &empty);

    let err = inkentry_cmd(&db, &cfg)
        .arg("parse-file")
        .arg(_tmp.path().join("absent.rs"))
        .output()
        .unwrap();
    assert_hard_error("parse-file unreadable", &err);
}

// ── hash-file ────────────────────────────────────────────────────────────────

#[test]
fn hash_file_exit_codes() {
    let (_tmp, db, cfg) = index_fixture_project();
    let source = plumbing_helpers::fixture_path().join("src/lib.rs");

    let ok = inkentry_cmd(&db, &cfg)
        .arg("hash-file")
        .arg(&source)
        .output()
        .unwrap();
    assert_exit("hash-file results", &ok, 0);
    assert!(!ok.stdout.is_empty(), "exit 0 must emit at least one row");

    let err = inkentry_cmd(&db, &cfg)
        .arg("hash-file")
        .arg(_tmp.path().join("absent.rs"))
        .output()
        .unwrap();
    assert_hard_error("hash-file unreadable", &err);
}

#[test]
fn hash_file_never_reports_an_empty_result() {
    // Documented exception: a hash always exists for a readable file, so this
    // command answers with one row or fails. A future exit 1 here would be a
    // new state for callers to handle, not a bug fix.
    let (tmp, db, cfg) = index_fixture_project();
    let never_indexed = tmp.path().join("scratch.rs");
    std::fs::write(&never_indexed, "fn scratch() {}\n").unwrap();

    let out = inkentry_cmd(&db, &cfg)
        .arg("hash-file")
        .arg(&never_indexed)
        .output()
        .unwrap();
    assert_exit("hash-file un-indexed file", &out, 0);
    assert!(
        !out.stdout.is_empty(),
        "an un-indexed file still has a hash to report"
    );
}

// ── knn ──────────────────────────────────────────────────────────────────────

fn knn_query() -> String {
    serde_json::json!({
        "model": "test-model",
        "dimensions": FIXTURE_EMBEDDING_DIM,
        "vector": vec![0.1f32; FIXTURE_EMBEDDING_DIM],
    })
    .to_string()
}

#[test]
fn knn_exit_codes() {
    let (_tmp, db, cfg) = index_fixture_project();

    let ok = inkentry_cmd(&db, &cfg)
        .arg("knn")
        .write_stdin(knn_query())
        .output()
        .unwrap();
    assert_exit("knn results", &ok, 0);
    assert!(!ok.stdout.is_empty(), "exit 0 must emit at least one row");

    // A similarity threshold above 1.0 is unreachable, so every result is
    // filtered out and the empty set is the honest answer.
    let empty = inkentry_cmd(&db, &cfg)
        .args(["knn", "--min-score", "1.5"])
        .write_stdin(knn_query())
        .output()
        .unwrap();
    assert_empty("knn min-score filters everything", &empty);

    let err = inkentry_cmd(&db, &cfg)
        .arg("knn")
        .write_stdin("not json at all")
        .output()
        .unwrap();
    assert_hard_error("knn malformed stdin", &err);
}

// ── graph-edges ──────────────────────────────────────────────────────────────

#[test]
fn graph_edges_exit_codes() {
    let (_tmp, db, cfg) = index_fixture_project();

    let ok = inkentry_cmd(&db, &cfg)
        .args(["graph-edges", "--file", "src/main.rs"])
        .output()
        .unwrap();
    assert_exit("graph-edges results", &ok, 0);
    assert!(!ok.stdout.is_empty(), "exit 0 must emit at least one row");

    let empty = inkentry_cmd(&db, &cfg)
        .args(["graph-edges", "--symbol", "no_such_symbol_anywhere"])
        .output()
        .unwrap();
    assert_empty("graph-edges unknown symbol", &empty);

    // Neither filter given is a usage error, not an empty set.
    let err = inkentry_cmd(&db, &cfg).arg("graph-edges").output().unwrap();
    assert_hard_error("graph-edges no filter", &err);
}

// ── read-memory ──────────────────────────────────────────────────────────────

#[test]
fn read_memory_exit_codes() {
    let (tmp, db, cfg) = index_fixture_project();
    let mem_path = db.with_file_name("memory.db");

    inkentry_bin()
        .current_dir(tmp.path())
        .env("INKENTRY_NO_SERVER", "1")
        .arg("--config")
        .arg(&cfg)
        .arg("memory")
        .arg("--db")
        .arg(&mem_path)
        .args([
            "add",
            "--kind",
            "decision",
            "--title",
            "Exit codes are part of the interface",
            "--body",
            "Scripts branch on them.",
        ])
        .assert()
        .success();

    let ok = inkentry_cmd(&db, &cfg).arg("read-memory").output().unwrap();
    assert_exit("read-memory results", &ok, 0);
    assert!(!ok.stdout.is_empty(), "exit 0 must emit at least one row");

    let empty = inkentry_cmd(&db, &cfg)
        .args(["read-memory", "--kind", "no-such-kind"])
        .output()
        .unwrap();
    assert_empty("read-memory unmatched kind", &empty);

    // No memory store at all, which is not the same as a store holding no
    // entries: exit 2, never the exit 1 that means "no results".
    let (_t2, missing_db, cfg2) = unindexed_project();
    let err = inkentry_cmd(&missing_db, &cfg2)
        .arg("read-memory")
        .output()
        .unwrap();
    assert_hard_error("read-memory no store", &err);

    // The same refusal when an index *is* present. This is the input the
    // helper above cannot reach, and the only one whose exit code this change
    // moves: resolution used to key off the index, so a present index carried
    // the command through to a store it then created empty, and an absent
    // store reported itself as an empty one. Pinned separately because a
    // regression here is invisible to every other case.
    let t3 = TempDir::new().expect("create temp dir");
    let indexed_db = t3.path().join("index.db");
    std::fs::write(&indexed_db, b"").expect("create index db");
    let cfg3 = write_config(t3.path(), &indexed_db, "http://127.0.0.1:1");
    let err = inkentry_cmd(&indexed_db, &cfg3)
        .arg("read-memory")
        .output()
        .unwrap();
    assert_hard_error("read-memory no store beside a present index", &err);
}

// ── embed ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn embed_exit_codes() {
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "capabilities": ["index.embed", "search.semantic"],
        })))
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/projects/.+/index/embed$"))
        .respond_with(IndexEmbedResponder)
        .mount(&mock)
        .await;

    let tmp = TempDir::new().unwrap();
    let cfg = tmp.path().join("config.toml");
    std::fs::write(&cfg, "mode = \"cloud_first\"\n").unwrap();
    write_project_server_config(tmp.path(), &mock.uri(), FIXTURE_PROJECT_ID);

    let ok = inkentry_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&cfg)
        .args(["plumbing", "embed"])
        .write_stdin("fn greet(name: &str) -> String\n")
        .output()
        .unwrap();
    assert_exit("embed results", &ok, 0);
    assert!(!ok.stdout.is_empty(), "exit 0 must emit a vector");

    // Documented exception: no reachable input is an empty *input*, not an
    // empty result set, so embed answers 0 with no rows rather than 1.
    let no_input = inkentry_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&cfg)
        .args(["plumbing", "embed"])
        .write_stdin("")
        .output()
        .unwrap();
    assert_exit("embed empty stdin", &no_input, 0);
    assert!(no_input.stdout.is_empty(), "no input means no vectors");

    // No embedding backend reachable is a hard error, not an empty set.
    let unreachable = TempDir::new().unwrap();
    let bare_cfg = unreachable.path().join("config.toml");
    std::fs::write(&bare_cfg, "llm_model = \"x\"\n").unwrap();
    let err = inkentry_bin()
        .current_dir(unreachable.path())
        .env("INKENTRY_NO_SERVER", "1")
        .env_remove("INKENTRY_SERVER_URL")
        .arg("--config")
        .arg(&bare_cfg)
        .args(["plumbing", "embed"])
        .write_stdin("some text\n")
        .output()
        .unwrap();
    assert_hard_error("embed no backend", &err);
}

// ── publish-notes ────────────────────────────────────────────────────────────

fn git_in(dir: &Path, args: &[&str]) {
    plumbing_helpers::isolate_git_config();
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

#[test]
fn publish_notes_exit_codes() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);

    // Nothing to publish is reported in the JSON payload, not the exit status:
    // this runs from a pre-push hook, where a non-zero exit aborts the user's
    // branch push. That coupling is why the skip path is exit 0 and not 1.
    let skipped = inkentry_bin()
        .current_dir(&repo)
        .env("INKENTRY_NO_SERVER", "1")
        .args(["plumbing", "publish-notes", "origin"])
        .output()
        .unwrap();
    assert_exit("publish-notes nothing to publish", &skipped, 0);
    assert!(
        !skipped.stdout.is_empty(),
        "the skip outcome is still reported as JSONL"
    );

    // A remote that resolves but cannot be pushed to is a real failure, and
    // without --best-effort it must surface as one.
    inkentry_bin()
        .current_dir(&repo)
        .env("INKENTRY_NO_SERVER", "1")
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "A publish failure is a hard error",
            "--body",
            "Unless the caller asked for best effort.",
        ])
        .assert()
        .success();
    let broken = tmp.path().join("not-a-repo");
    std::fs::create_dir_all(&broken).unwrap();
    git_in(
        &repo,
        &["remote", "add", "origin", broken.to_str().unwrap()],
    );

    let err = inkentry_bin()
        .current_dir(&repo)
        .env("INKENTRY_NO_SERVER", "1")
        .args(["plumbing", "publish-notes", "origin"])
        .output()
        .unwrap();
    assert_hard_error("publish-notes unpushable remote", &err);

    // The same failure under --best-effort is exit 0 with the error in the
    // payload, so an installed pre-push hook never blocks a code push.
    let tolerated = inkentry_bin()
        .current_dir(&repo)
        .env("INKENTRY_NO_SERVER", "1")
        .args(["plumbing", "publish-notes", "origin", "--best-effort"])
        .output()
        .unwrap();
    assert_exit("publish-notes best effort", &tolerated, 0);
    assert!(
        !tolerated.stdout.is_empty(),
        "--best-effort still reports the failure as JSONL"
    );
}

// ── push / pull (team-server transfer) ───────────────────────────────────────
//
// Unlike every read-only command above, push/pull are network-touching and
// their exit 1 (an empty delta) still emits the one report object — only exit 2
// leaves stdout empty. So these use bespoke assertions rather than
// `assert_empty` (which requires empty stdout on 1). Setup mirrors
// `memory_push_sync_total_failure.rs`: a mock team server plus a real seeded
// local project.

use plumbing_helpers::{
    init_local_project, inkentry_bin_in, mount_memory_batch, mount_memory_since, mount_team_health,
    seed_memory_note, write_team_config,
};
use wiremock::MockServer;

// Return the single report object push/pull emit, asserting there is exactly
// one. Their contract is one object per completed run.
fn sole_report(label: &str, out: &std::process::Output) -> serde_json::Value {
    let rows = plumbing_helpers::parse_jsonl(&out.stdout);
    assert_eq!(
        rows.len(),
        1,
        "{label}: expected exactly one report object, got stdout {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    rows.into_iter().next().unwrap()
}

// Echoes the batch request back as all-`created`, stamping cloud ids that match
// the entries' real uuids, so a follow-up push correctly sees them as already
// synced. A static body cannot do this: it never knows the seeded notes' uuids.
struct BatchEchoCreated;
impl wiremock::Respond for BatchEchoCreated {
    fn respond(&self, req: &wiremock::Request) -> wiremock::ResponseTemplate {
        #[derive(serde::Deserialize)]
        struct Item {
            external_id: String,
        }
        #[derive(serde::Deserialize)]
        struct Body {
            entries: Vec<Item>,
        }
        let body: Body = serde_json::from_slice(&req.body).unwrap_or(Body { entries: vec![] });
        let results: Vec<serde_json::Value> = body
            .entries
            .iter()
            .map(|e| {
                serde_json::json!({
                    "status": "created",
                    "external_id": e.external_id,
                    "id": format!("cloud-{}", e.external_id),
                })
            })
            .collect();
        wiremock::ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": results.len(), "skipped": 0, "failed": 0, "results": results,
        }))
    }
}

async fn mount_batch_echo(server: &MockServer) {
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path(
            "/v1/projects/acme-widget/memory/batch",
        ))
        .respond_with(BatchEchoCreated)
        .mount(server)
        .await;
}

// Push a batch with N created entries (using the echo responder), one report on
// stdout, exit 0.
#[tokio::test]
async fn plumbing_push_clean_push_is_exit_0_with_report() {
    let server = MockServer::start().await;
    mount_team_health(&server).await;
    mount_batch_echo(&server).await;

    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    init_local_project(proj.path());
    let cfg = write_team_config(proj.path(), &server.uri());
    seed_memory_note(home.path(), proj.path(), &cfg, "one");
    seed_memory_note(home.path(), proj.path(), &cfg, "two");

    let out = inkentry_bin_in(home.path())
        .current_dir(proj.path())
        .arg("--config")
        .arg(&cfg)
        .args(["plumbing", "push"])
        .output()
        .unwrap();

    assert_exit("plumbing push clean", &out, 0);
    let report = sole_report("plumbing push clean", &out);
    assert_eq!(report["attempted"], 2, "report: {report}");
    assert_eq!(report["created"], 2, "report: {report}");
    assert_eq!(report["failed"], 0, "report: {report}");
    assert_eq!(report["interrupted"], false, "report: {report}");
}

// A completed run that created nothing but had failures alongside real
// creations still exits 0 — at least one entry moved.
#[tokio::test]
async fn plumbing_push_partial_failure_with_a_creation_is_exit_0() {
    let server = MockServer::start().await;
    mount_team_health(&server).await;
    mount_memory_batch(
        &server,
        serde_json::json!({
            "created": 0, "skipped": 0, "failed": 0,
            "results": [
                {"status": "created", "external_id": "a", "id": "c1"},
                {"status": "failed", "external_id": "b"}
            ]
        }),
    )
    .await;

    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    init_local_project(proj.path());
    let cfg = write_team_config(proj.path(), &server.uri());
    seed_memory_note(home.path(), proj.path(), &cfg, "one");
    seed_memory_note(home.path(), proj.path(), &cfg, "two");

    let out = inkentry_bin_in(home.path())
        .current_dir(proj.path())
        .arg("--config")
        .arg(&cfg)
        .args(["plumbing", "push"])
        .output()
        .unwrap();

    assert_exit("plumbing push partial", &out, 0);
    let report = sole_report("plumbing push partial", &out);
    assert_eq!(report["created"], 1, "report: {report}");
    assert_eq!(report["failed"], 1, "report: {report}");
}

// Nothing local to push is an empty delta: exit 1, and the report is still
// emitted (attempted == 0), unlike the read-only commands' empty-stdout exit 1.
#[tokio::test]
async fn plumbing_push_nothing_to_push_is_exit_1_with_report() {
    let server = MockServer::start().await;
    mount_team_health(&server).await;

    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    init_local_project(proj.path());
    let cfg = write_team_config(proj.path(), &server.uri());

    let out = inkentry_bin_in(home.path())
        .current_dir(proj.path())
        .arg("--config")
        .arg(&cfg)
        .args(["plumbing", "push"])
        .output()
        .unwrap();

    assert_exit("plumbing push empty", &out, 1);
    let report = sole_report("plumbing push empty", &out);
    assert_eq!(report["attempted"], 0, "report: {report}");
    assert_eq!(report["created"], 0, "report: {report}");
}

// Re-pushing entries already on the server is also an empty delta (exit 1):
// after the first push stamps their remote ids, the second push has nothing
// live to send.
#[tokio::test]
async fn plumbing_push_repush_already_synced_is_exit_1() {
    let server = MockServer::start().await;
    mount_team_health(&server).await;
    mount_batch_echo(&server).await;

    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    init_local_project(proj.path());
    let cfg = write_team_config(proj.path(), &server.uri());
    seed_memory_note(home.path(), proj.path(), &cfg, "one");

    let first = inkentry_bin_in(home.path())
        .current_dir(proj.path())
        .arg("--config")
        .arg(&cfg)
        .args(["plumbing", "push"])
        .output()
        .unwrap();
    assert_exit("plumbing push first", &first, 0);

    let second = inkentry_bin_in(home.path())
        .current_dir(proj.path())
        .arg("--config")
        .arg(&cfg)
        .args(["plumbing", "push"])
        .output()
        .unwrap();
    assert_exit("plumbing push re-push", &second, 1);
    let report = sole_report("plumbing push re-push", &second);
    assert_eq!(report["created"], 0, "report: {report}");
    assert_eq!(report["attempted"], 0, "report: {report}");
    assert_eq!(report["already_synced"], 1, "report: {report}");
}

// A total failure — nothing durably landed — did not complete: exit 2, stdout
// empty, diagnostic on stderr.
#[tokio::test]
async fn plumbing_push_total_failure_is_exit_2_empty_stdout() {
    let server = MockServer::start().await;
    mount_team_health(&server).await;
    mount_memory_batch(
        &server,
        serde_json::json!({"created": 0, "skipped": 0, "failed": 1, "results": []}),
    )
    .await;

    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    init_local_project(proj.path());
    let cfg = write_team_config(proj.path(), &server.uri());
    seed_memory_note(home.path(), proj.path(), &cfg, "one");

    let out = inkentry_bin_in(home.path())
        .current_dir(proj.path())
        .arg("--config")
        .arg(&cfg)
        .args(["plumbing", "push"])
        .output()
        .unwrap();

    assert_hard_error("plumbing push total failure", &out);
}

// No explicit team server_url configured is a setup error: exit 2, empty
// stdout. (The loopback inference server must never satisfy push.)
#[tokio::test]
async fn plumbing_push_no_server_url_is_exit_2() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    init_local_project(proj.path());
    // A global config with a db_path but no server_url anywhere.
    let cfg = proj.path().join("config.toml");
    let db = proj.path().join(".inkentry").join("index.db");
    std::fs::write(&cfg, format!("db_path = {db:?}\n")).unwrap();

    let out = inkentry_bin_in(home.path())
        .current_dir(proj.path())
        .env("INKENTRY_NO_SERVER", "1")
        .env_remove("INKENTRY_SERVER_URL")
        .arg("--config")
        .arg(&cfg)
        .args(["plumbing", "push"])
        .output()
        .unwrap();

    assert_hard_error("plumbing push no server_url", &out);
}

// Pull that applies new remote entries: exit 0, report {applied > 0}.
#[tokio::test]
async fn plumbing_pull_applies_is_exit_0_with_report() {
    let server = MockServer::start().await;
    mount_team_health(&server).await;
    mount_memory_since(
        &server,
        serde_json::json!({"entries": [{
            "id": "01890000-0000-7000-8000-000000000abc",
            "kind": "decision",
            "title": "Teammate",
            "body": "from the server",
            "created_at": "2026-06-19T01:00:00Z"
        }]}),
    )
    .await;

    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    init_local_project(proj.path());
    let cfg = write_team_config(proj.path(), &server.uri());

    let out = inkentry_bin_in(home.path())
        .current_dir(proj.path())
        .arg("--config")
        .arg(&cfg)
        .args(["plumbing", "pull"])
        .output()
        .unwrap();

    assert_exit("plumbing pull applies", &out, 0);
    let report = sole_report("plumbing pull applies", &out);
    assert_eq!(report["applied"], 1, "report: {report}");
}

// Pull with nothing new is an empty delta: exit 1, report {applied: 0}. A second
// pull of the same entry dedups, so it too is exit 1 (idempotence).
#[tokio::test]
async fn plumbing_pull_empty_then_idempotent_is_exit_1() {
    let server = MockServer::start().await;
    mount_team_health(&server).await;
    mount_memory_since(
        &server,
        serde_json::json!({"entries": [{
            "id": "01890000-0000-7000-8000-000000000abc",
            "kind": "decision",
            "title": "Teammate",
            "body": "from the server",
            "created_at": "2026-06-19T01:00:00Z"
        }]}),
    )
    .await;

    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    init_local_project(proj.path());
    let cfg = write_team_config(proj.path(), &server.uri());

    // First pull applies the one entry (exit 0); the second re-fetches it but it
    // is already present, so nothing new applies.
    let first = inkentry_bin_in(home.path())
        .current_dir(proj.path())
        .arg("--config")
        .arg(&cfg)
        .args(["plumbing", "pull"])
        .output()
        .unwrap();
    assert_exit("plumbing pull first", &first, 0);

    let second = inkentry_bin_in(home.path())
        .current_dir(proj.path())
        .arg("--config")
        .arg(&cfg)
        .args(["plumbing", "pull"])
        .output()
        .unwrap();
    assert_exit("plumbing pull idempotent", &second, 1);
    let report = sole_report("plumbing pull idempotent", &second);
    assert_eq!(report["applied"], 0, "report: {report}");
}

// No explicit team server_url configured is a setup error for pull too: exit 2.
#[tokio::test]
async fn plumbing_pull_no_server_url_is_exit_2() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    init_local_project(proj.path());
    let cfg = proj.path().join("config.toml");
    let db = proj.path().join(".inkentry").join("index.db");
    std::fs::write(&cfg, format!("db_path = {db:?}\n")).unwrap();

    let out = inkentry_bin_in(home.path())
        .current_dir(proj.path())
        .env("INKENTRY_NO_SERVER", "1")
        .env_remove("INKENTRY_SERVER_URL")
        .arg("--config")
        .arg(&cfg)
        .args(["plumbing", "pull"])
        .output()
        .unwrap();

    assert_hard_error("plumbing pull no server_url", &out);
}
