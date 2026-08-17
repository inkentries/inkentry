// The CLI's memory commands against a real self-hosted team server.
//
// Every other CLI-to-team-server test in this suite runs against a mock, and a
// mock is written from the CLI's own expectations — by construction it agrees
// with them, so it cannot detect the CLI and the real server disagreeing about
// the wire. That is how the server kept exposing integer note ids while
// `NoteId` moved to strings: one test noticed, and only because its mock had
// been hand-updated.
//
// This test has no mock. `inkentry-server`'s production router serves a real
// `ServerDb` over a real loopback socket, and the real `inkentry` binary drives
// it in `cloud_first` — the mode that makes the server the store of record, so
// every assertion below is about data that only ever existed server-side.

use crate::plumbing_helpers;
use plumbing_helpers::inkentry_bin_in;

use std::path::Path;
use std::sync::Arc;

use assert_cmd::Command;

use serde_json::Value;
use tempfile::TempDir;

const PROJECT: &str = "acme/widget";

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

// A real `inkentry-server` on an ephemeral loopback port. The database is a
// file rather than `:memory:` so it behaves as a deployed server does, migrations
// and all.
async fn spawn_real_server(db_path: &Path) -> String {
    register_sqlite_vec();
    let db = inkentry_server::db::ServerDb::open(db_path, 4, "test-model").expect("open server db");
    let instance_id = db.get_or_create_instance_id().expect("instance_id");
    let state = inkentry_server::AppState {
        db: Arc::new(tokio::sync::Mutex::new(db)),
        auth: Arc::new(inkentry_server::auth::ApiKeyAuth::new(None)),
        conflict_threshold: inkentry_server::default_conflict_threshold(),
        embedder: inkentry_server::EmbedderSlot::disabled(),
        embed_admission: inkentry_server::EmbedAdmission::new(
            inkentry_server::EMBED_QUEUE_CAPACITY,
            inkentry_server::EMBED_BUSY_RETRY_AFTER_SECS,
        ),
        embed_threads: 4,
        llm: None,
        max_tokens_ceiling: 8192,
        rate_limiter: Arc::new(inkentry_server::rate_limiter::RateLimiter::new(1000, 60)),
        instance_id,
        started_by: None,
        trusted_proxies: Default::default(),
        relay: inkentry_server::relay::RelayRegistry::disabled(),
    };
    let app = inkentry_server::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral loopback port");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await;
    });
    format!("http://{addr}")
}

// A project configured the way a team member's would be: `cloud_first`, so the
// server is the store of record and nothing falls back to a local `memory.db`.
//
// The split is the CLI's own: `server_url` and `project_id` are read only from
// the project-level `.inkentry/config.toml` (a team server is a project-wide
// choice), `mode` only from the global config.
fn make_project(home: &Path, base_url: &str) {
    let project = home.join(".inkentry");
    std::fs::create_dir_all(&project).expect("create project dir");
    std::fs::write(
        project.join("config.toml"),
        format!("project_id = \"{PROJECT}\"\nserver_url = \"{base_url}\"\n"),
    )
    .expect("write project config");

    let global = home.join(".config").join("inkentry");
    std::fs::create_dir_all(&global).expect("create global config dir");
    // `store_in_git_notes` off: the child would otherwise write a git note per
    // entry into whatever repository its working directory sits in.
    std::fs::write(
        global.join("config.toml"),
        "mode = \"cloud_first\"\nstore_in_git_notes = false\n",
    )
    .expect("write global config");
}

// `current_dir` is the throwaway home rather than the crate directory, so this
// repo's own committed `.inkentry/config.toml` never reaches the child, and
// `INKENTRY_STATE_DIR` points at the same throwaway so loopback auto-discovery
// cannot find a developer's running daemon. (`INKENTRY_NO_SERVER` would be the
// blunter tool and is the wrong one: it forces `offline`, which routes memory
// straight back to a local store and makes the whole test vacuous.)
fn cli(home: &Path, _base_url: &str) -> Command {
    let mut cmd = inkentry_bin_in(home);
    cmd.current_dir(home)
        .env("INKENTRY_STATE_DIR", home.join("state"));
    cmd
}

fn run(home: &Path, base_url: &str, args: &[&str]) -> (bool, String, String) {
    let out = cli(home, base_url).args(args).output().expect("spawn cli");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn run_ok(home: &Path, base_url: &str, args: &[&str]) -> String {
    let (ok, stdout, stderr) = run(home, base_url, args);
    assert!(ok, "`inkentry {}` failed:\n{stderr}", args.join(" "));
    stdout
}

fn add(home: &Path, base_url: &str, title: &str) -> String {
    run_ok(
        home,
        base_url,
        &[
            "memory", "add", "--kind", "decision", "--title", title, "--body", "why",
        ],
    )
}

fn list_entries(home: &Path, base_url: &str) -> Vec<Value> {
    let stdout = run_ok(
        home,
        base_url,
        &["memory", "list", "--format", "json", "--limit", "50"],
    );
    let parsed: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("`memory list --format json` is not JSON ({e}):\n{stdout}"));
    parsed
        .get("entries")
        .or(Some(&parsed))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_else(|| panic!("no entries array in:\n{stdout}"))
}

fn id_of(entries: &[Value], title: &str) -> String {
    entries
        .iter()
        .find(|e| e["title"] == title)
        .unwrap_or_else(|| panic!("no entry titled {title:?} in {entries:#?}"))["id"]
        .as_str()
        .unwrap_or_else(|| panic!("entry {title:?} has a non-string id in {entries:#?}"))
        .to_string()
}

fn assert_uuid_v7(id: &str, what: &str) {
    let parsed =
        uuid::Uuid::parse_str(id).unwrap_or_else(|e| panic!("{what} is not a UUID: {id} ({e})"));
    assert_eq!(parsed.get_version_num(), 7, "{what} must be a UUIDv7: {id}");
}

// The round trip the mocks cannot cover: entries written through the real CLI
// to the real server come back through the real CLI, addressed by the ids the
// server actually minted.
//
// `multi_thread` because the test blocks its own thread on a synchronous
// `Command::output()` while the in-process server must keep serving it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_memory_round_trips_against_a_real_team_server() {
    let db_dir = TempDir::new().unwrap();
    let base_url = spawn_real_server(&db_dir.path().join("server.db")).await;
    let home = TempDir::new().unwrap();
    let home = home.path();
    make_project(home, &base_url);

    add(home, &base_url, "First decision");
    add(home, &base_url, "Second decision");

    let entries = list_entries(home, &base_url);
    assert_eq!(entries.len(), 2, "both writes must come back: {entries:#?}");

    for entry in &entries {
        let id = entry["id"]
            .as_str()
            .unwrap_or_else(|| panic!("id must be a JSON string, got {}", entry["id"]));
        assert_uuid_v7(id, "a listed entry's id");
    }

    // `memory show <id>` proves the id the list handed out actually addresses
    // the entry on the server — a shape the CLI merely echoed would not.
    let first = id_of(&entries, "First decision");
    let shown = run_ok(home, &base_url, &["memory", "show", &first]);
    assert!(
        shown.contains("First decision"),
        "`memory show {first}` did not return the entry:\n{shown}"
    );
}

// Supersede is the route that carries an id in the path *and* in the body, so
// it is the one where a shape disagreement can hide on either side.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supersede_against_a_real_team_server_links_the_pair_by_uuid() {
    let db_dir = TempDir::new().unwrap();
    let base_url = spawn_real_server(&db_dir.path().join("server.db")).await;
    let home = TempDir::new().unwrap();
    let home = home.path();
    make_project(home, &base_url);

    add(home, &base_url, "Old decision");
    add(home, &base_url, "New decision");
    let entries = list_entries(home, &base_url);
    let old = id_of(&entries, "Old decision");
    let new = id_of(&entries, "New decision");

    run_ok(home, &base_url, &["memory", "supersede", &old, &new]);

    // The superseded entry is archived, so it only reappears with --archived.
    let stdout = run_ok(
        home,
        &base_url,
        &["memory", "list", "--format", "json", "--archived"],
    );
    let parsed: Value = serde_json::from_str(&stdout).expect("archived list is JSON");
    let archived = parsed
        .get("entries")
        .or(Some(&parsed))
        .and_then(Value::as_array)
        .expect("entries array")
        .clone();
    let old_entry = archived
        .iter()
        .find(|e| e["id"] == Value::String(old.clone()))
        .unwrap_or_else(|| panic!("superseded entry missing from archived list: {archived:#?}"));
    assert_eq!(
        old_entry["superseded_by"],
        Value::String(new.clone()),
        "superseded_by must carry the successor's UUID, not a rowid: {old_entry:#?}"
    );
    assert_uuid_v7(&new, "the successor id");
}

// A numeric id is what a user has in shell history from before the crossing.
// It must miss cleanly rather than address whatever row happens to hold that
// rowid on the server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_numeric_id_does_not_address_an_entry_on_a_real_team_server() {
    let db_dir = TempDir::new().unwrap();
    let base_url = spawn_real_server(&db_dir.path().join("server.db")).await;
    let home = TempDir::new().unwrap();
    let home = home.path();
    make_project(home, &base_url);

    add(home, &base_url, "Only decision");
    let entries = list_entries(home, &base_url);
    assert_eq!(entries.len(), 1);

    let (ok, stdout, _stderr) = run(home, &base_url, &["memory", "show", "1"]);
    assert!(
        !ok || !stdout.contains("Only decision"),
        "the first row's rowid must not address it: {stdout}"
    );
}
