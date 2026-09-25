mod plumbing_helpers;
use plumbing_helpers::inkentry_bin_in;

use std::path::Path;
use tempfile::TempDir;

fn write_project(dir: &Path) {
    std::fs::create_dir_all(dir.join("src")).expect("create src dir");
    std::fs::write(
        dir.join("src").join("lib.rs"),
        "pub fn greet() -> &'static str { \"hi\" }\n",
    )
    .expect("write lib.rs");
}

struct EventRow {
    command: String,
    surface: String,
    trigger: String,
    actor_kind: String,
    session_ref: Option<String>,
}

fn last_event(mem_db: &Path) -> EventRow {
    let conn = rusqlite::Connection::open(mem_db).expect("open memory db");
    conn.query_row(
        "SELECT command, surface, trigger, actor_kind, session_ref \
         FROM events ORDER BY rowid DESC LIMIT 1",
        [],
        |r| {
            Ok(EventRow {
                command: r.get(0)?,
                surface: r.get(1)?,
                trigger: r.get(2)?,
                actor_kind: r.get(3)?,
                session_ref: r.get(4)?,
            })
        },
    )
    .expect("at least one event row")
}

#[test]
fn a_declared_trigger_and_actor_are_recorded_verbatim_and_the_session_ref_is_hashed() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    write_project(proj.path());

    inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(proj.path())
        .args(["index", "."])
        .assert()
        .success();

    inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .env("INKENTRY_TRIGGER", "hook")
        .env("INKENTRY_ACTOR", "agent")
        .env("INKENTRY_SESSION_REF", "a-very-identifying-session-token")
        .current_dir(proj.path())
        .args([
            "memory", "add", "--kind", "note", "--title", "t", "--body", "b",
        ])
        .assert()
        .success();

    let mem_db = proj.path().join(".inkentry").join("memory.db");
    let row = last_event(&mem_db);
    assert_eq!(row.command, "memory.add");
    assert_eq!(row.surface, "cli");
    assert_eq!(row.trigger, "hook");
    assert_eq!(row.actor_kind, "agent");
    let session_ref = row.session_ref.expect("session_ref was declared");
    assert_ne!(
        session_ref, "a-very-identifying-session-token",
        "the raw session ref must never reach storage"
    );
    assert!(session_ref.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn an_undeclared_caller_records_unknown_trigger_and_actor_with_no_session_ref() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    write_project(proj.path());

    inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(proj.path())
        .args(["index", "."])
        .assert()
        .success();

    inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .env_remove("INKENTRY_TRIGGER")
        .env_remove("INKENTRY_ACTOR")
        .env_remove("INKENTRY_SESSION_REF")
        .current_dir(proj.path())
        .args([
            "memory", "add", "--kind", "note", "--title", "t2", "--body", "b2",
        ])
        .assert()
        .success();

    let mem_db = proj.path().join(".inkentry").join("memory.db");
    let row = last_event(&mem_db);
    assert_eq!(row.trigger, "unknown");
    assert_eq!(row.actor_kind, "unknown");
    assert_eq!(row.session_ref, None);
}

#[test]
fn memory_list_json_exposes_the_declared_origin_and_omits_it_when_undeclared() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    write_project(proj.path());

    inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(proj.path())
        .args(["index", "."])
        .assert()
        .success();

    inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .env("INKENTRY_ACTOR", "human")
        .env("INKENTRY_TOOL", "cli")
        .current_dir(proj.path())
        .args([
            "memory",
            "add",
            "--kind",
            "note",
            "--title",
            "with origin",
            "--body",
            "b",
        ])
        .assert()
        .success();

    inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .env_remove("INKENTRY_ACTOR")
        .env_remove("INKENTRY_TOOL")
        .current_dir(proj.path())
        .args([
            "memory",
            "add",
            "--kind",
            "note",
            "--title",
            "without origin",
            "--body",
            "b",
        ])
        .assert()
        .success();

    let out = inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(proj.path())
        .args(["memory", "list", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let notes: Vec<serde_json::Value> = serde_json::from_slice(&out).expect("valid JSON array");

    let with_origin = notes
        .iter()
        .find(|n| n["title"] == "with origin")
        .expect("entry present");
    assert_eq!(with_origin["origin"]["actor_kind"], "human");
    assert_eq!(with_origin["origin"]["tool"], "cli");

    let without_origin = notes
        .iter()
        .find(|n| n["title"] == "without origin")
        .expect("entry present");
    assert!(
        without_origin.get("origin").is_none(),
        "an undeclared origin must be omitted, not a null or fabricated object: {without_origin}"
    );
}

#[test]
fn search_with_no_local_memory_db_records_nothing_and_creates_no_file() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    write_project(proj.path());

    inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(proj.path())
        .args(["index", "."])
        .assert()
        .success();

    let mem_db = proj.path().join(".inkentry").join("memory.db");
    assert!(
        !mem_db.exists(),
        "indexing code alone must not create memory.db"
    );

    inkentry_bin_in(home.path())
        .env("INKENTRY_NO_SERVER", "1")
        .env("INKENTRY_TRIGGER", "explicit")
        .env("INKENTRY_ACTOR", "human")
        .current_dir(proj.path())
        .args(["search", "greet", "--only-code", "--format", "json"])
        .assert()
        .success();

    assert!(
        !mem_db.exists(),
        "a search with no local memory.db must not create one just to record an event"
    );
}
