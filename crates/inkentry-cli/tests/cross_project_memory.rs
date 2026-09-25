// Linked projects' `locked`/`cross-project` decisions and requirements surface
// in `memory list`, `search` and `context`; nothing else from them does.

mod plumbing_helpers;
use plumbing_helpers::{inkentry_bin_in, register_sqlite_vec};

use assert_cmd::Command;
use rusqlite::Connection;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

// Explicit `INKENTRY_REGISTRY_DIR` override: `dirs::config_dir()` is not
// `HOME`-redirectable on Windows (`%APPDATA%`).
fn registry_dir(home: &Path) -> PathBuf {
    home.join(".config").join("inkentry")
}

// Canonicalize the way the product does (de-UNCs the Windows verbatim prefix);
// otherwise the cross-project dep lookup finds nothing on Windows.
fn canon(p: &Path) -> PathBuf {
    inkentry_core::utils::canonicalize(p)
}

struct TestRegistry {
    conn: Connection,
}

impl TestRegistry {
    fn new(home_dir: &Path) -> Self {
        let config_dir = registry_dir(home_dir);
        fs::create_dir_all(&config_dir).expect("create registry dir");
        let db_path = config_dir.join("registry.db");
        let conn = Connection::open(&db_path).expect("open registry db");
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS projects (
                 id            INTEGER PRIMARY KEY AUTOINCREMENT,
                 root_path     TEXT    NOT NULL UNIQUE,
                 db_path       TEXT    NOT NULL,
                 registered_at INTEGER NOT NULL DEFAULT (unixepoch())
             );
             CREATE TABLE IF NOT EXISTS project_deps (
                 project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                 dep_id     INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                 PRIMARY KEY (project_id, dep_id)
             );",
        )
        .expect("init registry schema");
        Self { conn }
    }

    // Canonicalized to resolve macOS symlinks (`/var/folders` vs
    // `/private/var/folders`) that would mismatch the CLI's `current_dir()`.
    fn register(&self, root: &Path, db: &Path) -> i64 {
        let root_c = canon(root);
        let db_c = canon(db);
        self.conn
            .execute(
                "INSERT INTO projects (root_path, db_path)
                 VALUES (?1, ?2)
                 ON CONFLICT(root_path) DO UPDATE SET db_path = excluded.db_path",
                rusqlite::params![root_c.to_string_lossy(), db_c.to_string_lossy()],
            )
            .expect("register project");
        self.conn
            .query_row(
                "SELECT id FROM projects WHERE root_path = ?1",
                rusqlite::params![root_c.to_string_lossy()],
                |r| r.get(0),
            )
            .expect("fetch project id")
    }

    fn add_dep(&self, project_id: i64, dep_id: i64) {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO project_deps (project_id, dep_id) VALUES (?1, ?2)",
                rusqlite::params![project_id, dep_id],
            )
            .expect("add dep edge");
    }
}

fn open_memory_db(path: &Path) -> Connection {
    register_sqlite_vec();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create memory db parent");
    }
    // Created through the store rather than by replaying the schema and
    // stamping a literal: a wrong hand-written stamp makes the store read as an
    // older product's and be refused.
    drop(inkentry_core::storage::MemoryStore::open(path).expect("create memory db"));
    let conn = Connection::open(path).expect("open memory db");
    conn.execute_batch("PRAGMA foreign_keys = ON")
        .expect("foreign keys");
    conn
}

// `tags` land in `note_tags`; every tag literal in this file is already
// normalised, so they are not re-normalised here.
fn seed_note(
    conn: &Connection,
    kind: &str,
    title: &str,
    body: &str,
    tags: &[&str],
    status: &str,
) -> String {
    let uuid = inkentry_core::storage::uuid_v7_at(1_700_000_000);
    conn.execute(
        "INSERT INTO notes (uuid, kind, title, body, status, entity_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            uuid,
            kind,
            title,
            body,
            status,
            inkentry_core::storage::entity_id(kind, title, body)
        ],
    )
    .expect("seed note");
    for tag in tags {
        conn.execute(
            "INSERT INTO note_tags (note_uuid, tag) VALUES (?1, ?2)",
            rusqlite::params![uuid, tag],
        )
        .expect("seed tag");
    }
    uuid
}

fn write_config(dir: &Path, index_db: &Path) -> PathBuf {
    let index_db_c = canon(index_db);
    let cfg = format!(
        concat!(
            "db_path = {:?}\n",
            "api_base_url = \"http://127.0.0.1:1\"\n",
            "llm_model = \"none\"\n",
        ),
        index_db_c
    );
    let config_path = dir.join("config.toml");
    fs::write(&config_path, cfg).expect("write config");
    config_path
}

// An empty SQLite database suffices: the dep pass never opens the index DB.
fn create_inkentry_dir(project_root: &Path) -> PathBuf {
    let inkentry_dir = project_root.join(".inkentry");
    fs::create_dir_all(&inkentry_dir).expect("create .inkentry dir");
    let index_db = inkentry_dir.join("index.db");
    let _ = Connection::open(&index_db).expect("create stub index.db");
    index_db
}

// All returned paths are canonical so the subprocess `current_dir` matches
// registry `root_path` entries (macOS `/var` vs `/private/var`). The caller must
// keep the `TempDir` alive.
#[allow(clippy::type_complexity)]
fn setup_linked_projects() -> (
    TempDir, // keep alive
    PathBuf, // home (use as HOME env)
    PathBuf, // primary project root (canonical)
    PathBuf, // primary index.db path (canonical)
    PathBuf, // primary config.toml
    PathBuf, // dep project root (canonical)
    PathBuf, // dep memory.db (canonical, caller seeds this)
) {
    let tmp = TempDir::new().expect("create temp dir");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).expect("create home dir");

    let primary_root_raw = tmp.path().join("primary");
    fs::create_dir_all(&primary_root_raw).expect("create primary dir");
    let primary_index_raw = create_inkentry_dir(&primary_root_raw);

    let dep_root_raw = tmp.path().join("dep");
    fs::create_dir_all(&dep_root_raw).expect("create dep dir");
    let dep_index_raw = create_inkentry_dir(&dep_root_raw);

    // Canonicalize after the directories exist so symlink resolution succeeds.
    let primary_root = canon(&primary_root_raw);
    let primary_index = canon(&primary_index_raw);
    let dep_root = canon(&dep_root_raw);
    let dep_index = canon(&dep_index_raw);

    let primary_config = write_config(&primary_root, &primary_index);
    let dep_mem = dep_index.with_file_name("memory.db");

    let reg = TestRegistry::new(&home);
    let primary_id = reg.register(&primary_root, &primary_index);
    let dep_id = reg.register(&dep_root, &dep_index);
    reg.add_dep(primary_id, dep_id);

    (
        tmp,
        home,
        primary_root,
        primary_index,
        primary_config,
        dep_root,
        dep_mem,
    )
}

fn memory_cmd(home: &Path, primary_root: &Path, config: &Path, primary_mem: &Path) -> Command {
    let mut cmd = inkentry_bin_in(home);
    cmd.env("HOME", home)
        .env("INKENTRY_REGISTRY_DIR", registry_dir(home))
        // Unset XDG_CONFIG_HOME so dirs::config_dir() uses $HOME/.config on Linux,
        // matching what TestRegistry::new() writes to home_dir.join(".config").
        .env_remove("XDG_CONFIG_HOME")
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(primary_root)
        .arg("--config")
        .arg(config)
        .arg("memory")
        .arg("--db")
        .arg(primary_mem);
    cmd
}

// Unified search derives the memory store from the resolved index.db's
// sibling, so cwd + `--config` resolve both without `--db`.
fn search_cmd(home: &Path, primary_root: &Path, config: &Path) -> Command {
    let mut cmd = inkentry_bin_in(home);
    cmd.env("HOME", home)
        .env("INKENTRY_REGISTRY_DIR", registry_dir(home))
        .env_remove("XDG_CONFIG_HOME")
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(primary_root)
        .arg("--config")
        .arg(config)
        .arg("search");
    cmd
}

fn context_cmd(
    home: &Path,
    primary_root: &Path,
    config: &Path,
    primary_mem: &Path,
    primary_index: &Path,
) -> Command {
    let mut cmd = inkentry_bin_in(home);
    cmd.env("HOME", home)
        .env("INKENTRY_REGISTRY_DIR", registry_dir(home))
        .env_remove("XDG_CONFIG_HOME")
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(primary_root)
        .arg("--config")
        .arg(config)
        .arg("context")
        .arg("--db")
        .arg(primary_mem)
        .arg("--index-db")
        .arg(primary_index)
        .arg("--no-conventions");
    cmd
}

#[test]
fn memory_list_includes_locked_decision_from_linked_dep() {
    let (_tmp, home, primary_root, primary_index, primary_config, _dep_root, dep_mem) =
        setup_linked_projects();

    let primary_mem = primary_index.with_file_name("memory.db");
    let primary_conn = open_memory_db(&primary_mem);
    seed_note(
        &primary_conn,
        "decision",
        "Local decision",
        "local body",
        &[],
        "active",
    );

    let dep_conn = open_memory_db(&dep_mem);
    seed_note(
        &dep_conn,
        "decision",
        "SSE is Cloud-only",
        "SSE memory stream is Cloud-only per decision #134.",
        &["locked", "v1"],
        "active",
    );

    let output = memory_cmd(&home, &primary_root, &primary_config, &primary_mem)
        .args(["list", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let notes: Vec<serde_json::Value> = serde_json::from_slice(&output).expect("valid JSON");
    let titles: Vec<&str> = notes.iter().filter_map(|n| n["title"].as_str()).collect();

    assert!(
        titles.contains(&"Local decision"),
        "local note must appear; got: {titles:?}"
    );
    assert!(
        titles.contains(&"SSE is Cloud-only"),
        "locked dep decision must be surfaced; got: {titles:?}"
    );
}

#[test]
fn dep_note_carries_source_project_tag_in_json() {
    let (_tmp, home, primary_root, primary_index, primary_config, _dep_root, dep_mem) =
        setup_linked_projects();
    let primary_mem = primary_index.with_file_name("memory.db");

    let dep_conn = open_memory_db(&dep_mem);
    seed_note(
        &dep_conn,
        "decision",
        "Cross-project tagged decision",
        "Must carry source_project in JSON.",
        &["locked"],
        "active",
    );

    let output = memory_cmd(&home, &primary_root, &primary_config, &primary_mem)
        .args(["list", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let notes: Vec<serde_json::Value> = serde_json::from_slice(&output).expect("valid JSON");
    let dep_note = notes
        .iter()
        .find(|n| n["title"].as_str() == Some("Cross-project tagged decision"))
        .expect("dep note must appear in list");

    assert_eq!(
        dep_note["source_project"].as_str(),
        Some("dep"),
        "source_project must be 'dep'; got: {dep_note}"
    );
    assert!(
        dep_note["source_project_path"].as_str().is_some(),
        "source_project_path must be set; got: {dep_note}"
    );
}

#[test]
fn local_note_has_no_source_project_field() {
    let (_tmp, home, primary_root, primary_index, primary_config, _dep_root, _dep_mem) =
        setup_linked_projects();
    let primary_mem = primary_index.with_file_name("memory.db");

    let primary_conn = open_memory_db(&primary_mem);
    seed_note(
        &primary_conn,
        "decision",
        "Local-only decision",
        "body",
        &[],
        "active",
    );

    let output = memory_cmd(&home, &primary_root, &primary_config, &primary_mem)
        .args(["list", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let notes: Vec<serde_json::Value> = serde_json::from_slice(&output).expect("valid JSON");
    let local = notes
        .iter()
        .find(|n| n["title"].as_str() == Some("Local-only decision"))
        .expect("local note must appear");

    assert!(
        local.get("source_project").is_none() || local["source_project"].is_null(),
        "local note must not have source_project; got: {local}"
    );
}

// Text mode needs no embedding server. The dep pass appends ALL cross-cutting
// entries regardless of the FTS query.
#[test]
fn search_only_memory_text_appends_locked_dep_decisions() {
    let (_tmp, home, primary_root, _primary_index, primary_config, _dep_root, dep_mem) =
        setup_linked_projects();

    let dep_conn = open_memory_db(&dep_mem);
    seed_note(
        &dep_conn,
        "decision",
        "Auth uses JWT tokens",
        "All authentication must use signed JWT tokens.",
        &["locked", "security"],
        "active",
    );

    let output = search_cmd(&home, &primary_root, &primary_config)
        .args([
            "--only-memory",
            "--only-text",
            "--format",
            "json",
            "anything",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    // Unified envelope: each memory result nests the Note under `memory`.
    let results: Vec<serde_json::Value> = serde_json::from_slice(&output).expect("valid JSON");
    let titles: Vec<&str> = results
        .iter()
        .filter_map(|r| r["memory"]["title"].as_str())
        .collect();
    assert!(
        titles.contains(&"Auth uses JWT tokens"),
        "locked dep decision must be appended by dep pass; got: {titles:?}"
    );
}

#[test]
fn untagged_dep_decision_is_not_surfaced() {
    let (_tmp, home, primary_root, primary_index, primary_config, _dep_root, dep_mem) =
        setup_linked_projects();
    let primary_mem = primary_index.with_file_name("memory.db");

    let dep_conn = open_memory_db(&dep_mem);
    seed_note(
        &dep_conn,
        "decision",
        "Internal dep naming convention",
        "We use camelCase for internal variables.",
        &["style"], // not locked, not cross-project
        "active",
    );

    let raw = inkentry_bin_in(&home)
        .env("HOME", &home)
        .env("INKENTRY_REGISTRY_DIR", registry_dir(&home))
        .env_remove("XDG_CONFIG_HOME")
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&primary_root)
        .arg("--config")
        .arg(&primary_config)
        .arg("memory")
        .arg("--db")
        .arg(&primary_mem)
        .args(["list", "--format", "json"])
        .output()
        .expect("run inkentry");

    let text = String::from_utf8_lossy(&raw.stdout);
    if text.trim().starts_with('[') {
        let notes: Vec<serde_json::Value> = serde_json::from_str(text.trim()).expect("valid JSON");
        let titles: Vec<&str> = notes.iter().filter_map(|n| n["title"].as_str()).collect();
        assert!(
            !titles.contains(&"Internal dep naming convention"),
            "untagged dep decision must NOT be surfaced; got: {titles:?}"
        );
    }
    // Non-JSON output ("No memory entries found.") also means it was not surfaced.
}

#[test]
fn dep_note_kind_is_not_surfaced_even_if_locked() {
    let (_tmp, home, primary_root, primary_index, primary_config, _dep_root, dep_mem) =
        setup_linked_projects();
    let primary_mem = primary_index.with_file_name("memory.db");

    let dep_conn = open_memory_db(&dep_mem);
    seed_note(
        &dep_conn,
        "note", // wrong kind — notes are always local
        "Surprising fact: locked",
        "A note tagged locked should remain private to its project.",
        &["locked"],
        "active",
    );

    let raw = inkentry_bin_in(&home)
        .env("HOME", &home)
        .env("INKENTRY_REGISTRY_DIR", registry_dir(&home))
        .env_remove("XDG_CONFIG_HOME")
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&primary_root)
        .arg("--config")
        .arg(&primary_config)
        .arg("memory")
        .arg("--db")
        .arg(&primary_mem)
        .args(["list", "--format", "json"])
        .output()
        .expect("run inkentry");

    let text = String::from_utf8_lossy(&raw.stdout);
    if text.trim().starts_with('[') {
        let notes: Vec<serde_json::Value> = serde_json::from_str(text.trim()).expect("valid JSON");
        let titles: Vec<&str> = notes.iter().filter_map(|n| n["title"].as_str()).collect();
        assert!(
            !titles.contains(&"Surprising fact: locked"),
            "dep note (kind=note) must not cross boundary; got: {titles:?}"
        );
    }
}

#[test]
fn dep_requirement_with_cross_project_tag_is_surfaced() {
    let (_tmp, home, primary_root, primary_index, primary_config, _dep_root, dep_mem) =
        setup_linked_projects();
    let primary_mem = primary_index.with_file_name("memory.db");

    let dep_conn = open_memory_db(&dep_mem);
    seed_note(
        &dep_conn,
        "requirement",
        "All APIs must be TLS 1.3",
        "Security requirement applying to all linked projects.",
        &["cross-project", "security"],
        "active",
    );

    let output = memory_cmd(&home, &primary_root, &primary_config, &primary_mem)
        .args(["list", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let notes: Vec<serde_json::Value> = serde_json::from_slice(&output).expect("valid JSON");
    let titles: Vec<&str> = notes.iter().filter_map(|n| n["title"].as_str()).collect();
    assert!(
        titles.contains(&"All APIs must be TLS 1.3"),
        "dep requirement with cross-project tag must be surfaced; got: {titles:?}"
    );
}

#[test]
fn single_project_no_deps_works_unchanged() {
    let tmp = TempDir::new().expect("create temp dir");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).expect("create home dir");

    let project_root_raw = tmp.path().join("proj");
    fs::create_dir_all(&project_root_raw).expect("create project dir");
    let index_db_raw = create_inkentry_dir(&project_root_raw);
    let project_root = canon(&project_root_raw);
    let index_db = canon(&index_db_raw);
    let config = write_config(&project_root, &index_db);
    let mem = index_db.with_file_name("memory.db");

    let reg = TestRegistry::new(&home);
    reg.register(&project_root, &index_db);

    let conn = open_memory_db(&mem);
    seed_note(
        &conn,
        "decision",
        "Local-only note",
        "no deps anywhere",
        &[],
        "active",
    );

    let output = inkentry_bin_in(&home)
        .env("HOME", &home)
        .env("INKENTRY_REGISTRY_DIR", registry_dir(&home))
        .env_remove("XDG_CONFIG_HOME")
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&project_root)
        .arg("--config")
        .arg(&config)
        .arg("memory")
        .arg("--db")
        .arg(&mem)
        .args(["list", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let notes: Vec<serde_json::Value> = serde_json::from_slice(&output).expect("valid JSON");
    assert_eq!(
        notes.len(),
        1,
        "exactly one local note, no phantom dep notes"
    );
    assert_eq!(notes[0]["title"].as_str(), Some("Local-only note"));
}

#[test]
fn context_includes_locked_dep_decision_with_source_badge() {
    let (_tmp, home, primary_root, primary_index, primary_config, _dep_root, dep_mem) =
        setup_linked_projects();
    let primary_mem = primary_index.with_file_name("memory.db");

    let dep_conn = open_memory_db(&dep_mem);
    seed_note(
        &dep_conn,
        "decision",
        "SSE endpoint is Cloud-only",
        "The /v1/memory/sse endpoint must never be in OSS.",
        &["locked"],
        "active",
    );

    let stdout = context_cmd(
        &home,
        &primary_root,
        &primary_config,
        &primary_mem,
        &primary_index,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();

    let text = String::from_utf8_lossy(&stdout);
    assert!(
        text.contains("SSE endpoint is Cloud-only"),
        "dep decision must appear in context; got:\n{text}"
    );
    assert!(
        text.contains("[from: dep]"),
        "source badge must appear for dep decision; got:\n{text}"
    );
}

#[test]
fn context_json_includes_dep_decision_with_source_project() {
    let (_tmp, home, primary_root, primary_index, primary_config, _dep_root, dep_mem) =
        setup_linked_projects();
    let primary_mem = primary_index.with_file_name("memory.db");

    let dep_conn = open_memory_db(&dep_mem);
    seed_note(
        &dep_conn,
        "decision",
        "Dep locked decision for context JSON",
        "Must appear in context JSON with source_project.",
        &["locked"],
        "active",
    );

    let output = context_cmd(
        &home,
        &primary_root,
        &primary_config,
        &primary_mem,
        &primary_index,
    )
    .args(["--format", "json"])
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();

    let obj: serde_json::Value = serde_json::from_slice(&output).expect("valid JSON");
    let sections = obj["sections"].as_array().expect("sections array");
    let decision_section = sections
        .iter()
        .find(|s| s[0].as_str() == Some("decision"))
        .expect("decision section");
    let decision_notes = decision_section[1].as_array().expect("notes array");

    let dep_note = decision_notes
        .iter()
        .find(|n| n["title"].as_str() == Some("Dep locked decision for context JSON"))
        .expect("dep decision must appear in context JSON");

    assert_eq!(
        dep_note["source_project"].as_str(),
        Some("dep"),
        "source_project must be 'dep' in context JSON; got: {dep_note}"
    );
}

#[test]
fn context_dep_requirement_appears_in_requirement_section() {
    let (_tmp, home, primary_root, primary_index, primary_config, _dep_root, dep_mem) =
        setup_linked_projects();
    let primary_mem = primary_index.with_file_name("memory.db");

    let dep_conn = open_memory_db(&dep_mem);
    seed_note(
        &dep_conn,
        "requirement",
        "Dep TLS requirement",
        "All endpoints must use TLS.",
        &["locked"],
        "active",
    );

    let output = context_cmd(
        &home,
        &primary_root,
        &primary_config,
        &primary_mem,
        &primary_index,
    )
    .args(["--format", "json"])
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();

    let obj: serde_json::Value = serde_json::from_slice(&output).expect("valid JSON");
    let sections = obj["sections"].as_array().expect("sections array");

    let req_section = sections
        .iter()
        .find(|s| s[0].as_str() == Some("requirement"))
        .expect("requirement section");
    let req_notes = req_section[1].as_array().expect("requirement notes");
    let found_in_req = req_notes
        .iter()
        .any(|n| n["title"].as_str() == Some("Dep TLS requirement"));
    assert!(
        found_in_req,
        "dep requirement must appear in requirement section of context"
    );

    let dec_section = sections
        .iter()
        .find(|s| s[0].as_str() == Some("decision"))
        .expect("decision section");
    let dec_notes = dec_section[1].as_array().expect("decision notes");
    let found_in_dec = dec_notes
        .iter()
        .any(|n| n["title"].as_str() == Some("Dep TLS requirement"));
    assert!(
        !found_in_dec,
        "dep requirement must NOT appear in decision section"
    );
}

#[test]
fn memory_list_local_only_suppresses_dep_results() {
    let (_tmp, home, primary_root, primary_index, primary_config, _dep_root, dep_mem) =
        setup_linked_projects();
    let primary_mem = primary_index.with_file_name("memory.db");

    let dep_conn = open_memory_db(&dep_mem);
    seed_note(
        &dep_conn,
        "decision",
        "Would appear without local-only",
        "body",
        &["locked"],
        "active",
    );

    let stdout = memory_cmd(&home, &primary_root, &primary_config, &primary_mem)
        .args(["list", "--local-only"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = String::from_utf8_lossy(&stdout);
    assert!(
        !text.contains("Would appear without local-only"),
        "--local-only must suppress dep results; got:\n{text}"
    );
}

#[test]
fn search_local_only_suppresses_dep_memory_results() {
    let (_tmp, home, primary_root, _primary_index, primary_config, _dep_root, dep_mem) =
        setup_linked_projects();

    let dep_conn = open_memory_db(&dep_mem);
    seed_note(
        &dep_conn,
        "decision",
        "Search-suppressed dep decision",
        "Normally appended by dep pass.",
        &["locked"],
        "active",
    );

    let stdout = search_cmd(&home, &primary_root, &primary_config)
        .args([
            "--only-memory",
            "--only-text",
            "--local-only",
            "dep decision",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = String::from_utf8_lossy(&stdout);
    assert!(
        !text.contains("Search-suppressed dep decision"),
        "search --local-only must suppress dep results; got:\n{text}"
    );
}

#[test]
fn context_local_only_suppresses_dep_results() {
    let (_tmp, home, primary_root, primary_index, primary_config, _dep_root, dep_mem) =
        setup_linked_projects();
    let primary_mem = primary_index.with_file_name("memory.db");

    let dep_conn = open_memory_db(&dep_mem);
    seed_note(
        &dep_conn,
        "decision",
        "Context-suppressed dep decision",
        "body",
        &["locked"],
        "active",
    );

    let stdout = context_cmd(
        &home,
        &primary_root,
        &primary_config,
        &primary_mem,
        &primary_index,
    )
    .arg("--local-only")
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();

    let text = String::from_utf8_lossy(&stdout);
    assert!(
        !text.contains("Context-suppressed dep decision"),
        "context --local-only must suppress dep results; got:\n{text}"
    );
}

#[test]
fn archived_dep_decision_is_not_surfaced() {
    let (_tmp, home, primary_root, primary_index, primary_config, _dep_root, dep_mem) =
        setup_linked_projects();
    let primary_mem = primary_index.with_file_name("memory.db");

    let dep_conn = open_memory_db(&dep_mem);
    seed_note(
        &dep_conn,
        "decision",
        "Old locked decision now archived",
        "Superseded and archived — must not propagate.",
        &["locked"],
        "archived",
    );

    let raw = inkentry_bin_in(&home)
        .env("HOME", &home)
        .env("INKENTRY_REGISTRY_DIR", registry_dir(&home))
        .env_remove("XDG_CONFIG_HOME")
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&primary_root)
        .arg("--config")
        .arg(&primary_config)
        .arg("memory")
        .arg("--db")
        .arg(&primary_mem)
        .args(["list", "--format", "json"])
        .output()
        .expect("run inkentry");

    let text = String::from_utf8_lossy(&raw.stdout);
    if text.trim().starts_with('[') {
        let notes: Vec<serde_json::Value> = serde_json::from_str(text.trim()).expect("valid JSON");
        let titles: Vec<&str> = notes.iter().filter_map(|n| n["title"].as_str()).collect();
        assert!(
            !titles.contains(&"Old locked decision now archived"),
            "archived dep note must not be surfaced; got: {titles:?}"
        );
    }
    // Non-JSON output ("No memory entries found.") also means it was suppressed.
}

#[test]
fn context_never_pulls_dep_handoffs() {
    let (_tmp, home, primary_root, primary_index, primary_config, _dep_root, dep_mem) =
        setup_linked_projects();
    let primary_mem = primary_index.with_file_name("memory.db");

    let dep_conn = open_memory_db(&dep_mem);
    seed_note(
        &dep_conn,
        "handoff",
        "Dep handoff: session ended",
        "This handoff must remain local to the dep project.",
        &["locked"], // even locked tag cannot make a handoff cross-project
        "active",
    );

    let stdout = context_cmd(
        &home,
        &primary_root,
        &primary_config,
        &primary_mem,
        &primary_index,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();

    let text = String::from_utf8_lossy(&stdout);
    assert!(
        !text.contains("Dep handoff: session ended"),
        "dep handoff must never cross project boundaries; got:\n{text}"
    );
}

// The intent roster is session/project-scoped, like handoff/question, so it
// never crosses projects.
#[test]
fn context_never_pulls_dep_intents() {
    let (_tmp, home, primary_root, primary_index, primary_config, _dep_root, dep_mem) =
        setup_linked_projects();
    let primary_mem = primary_index.with_file_name("memory.db");

    let dep_conn = open_memory_db(&dep_mem);
    seed_note(
        &dep_conn,
        "intent",
        "Dep intent: working on shared util",
        "Only relevant within the dep project.",
        &["locked", "cross-project"], // neither tag makes an intent cross-project
        "active",
    );

    let stdout = context_cmd(
        &home,
        &primary_root,
        &primary_config,
        &primary_mem,
        &primary_index,
    )
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();

    let text = String::from_utf8_lossy(&stdout);
    assert!(
        !text.contains("Dep intent: working on shared util"),
        "dep intent must never cross project boundaries; got:\n{text}"
    );
}

#[test]
fn dep_question_is_never_surfaced_cross_project() {
    let (_tmp, home, primary_root, primary_index, primary_config, _dep_root, dep_mem) =
        setup_linked_projects();
    let primary_mem = primary_index.with_file_name("memory.db");

    let dep_conn = open_memory_db(&dep_mem);
    seed_note(
        &dep_conn,
        "question",
        "Dep question: should we use SSE?",
        "Only relevant within the dep project.",
        &["locked"],
        "active",
    );

    let raw = inkentry_bin_in(&home)
        .env("HOME", &home)
        .env("INKENTRY_REGISTRY_DIR", registry_dir(&home))
        .env_remove("XDG_CONFIG_HOME")
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&primary_root)
        .arg("--config")
        .arg(&primary_config)
        .arg("memory")
        .arg("--db")
        .arg(&primary_mem)
        .args(["list", "--format", "json"])
        .output()
        .expect("run inkentry");

    let text = String::from_utf8_lossy(&raw.stdout);
    if text.trim().starts_with('[') {
        let notes: Vec<serde_json::Value> = serde_json::from_str(text.trim()).expect("valid JSON");
        let titles: Vec<&str> = notes.iter().filter_map(|n| n["title"].as_str()).collect();
        assert!(
            !titles.contains(&"Dep question: should we use SSE?"),
            "dep question must not cross boundary; got: {titles:?}"
        );
    }
}

// The dep pass deduplicates diamond-shaped dependency graphs; this covers the
// simpler case of two direct deps with a unique entry each, to confirm no
// cross-dep pollution.
#[test]
fn multiple_deps_results_are_aggregated_not_duplicated() {
    let tmp = TempDir::new().expect("create temp dir");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).expect("create home dir");

    let primary_root_raw = tmp.path().join("primary");
    fs::create_dir_all(&primary_root_raw).expect("primary dir");
    let primary_index_raw = create_inkentry_dir(&primary_root_raw);
    let primary_root = canon(&primary_root_raw);
    let primary_index = canon(&primary_index_raw);
    let primary_config = write_config(&primary_root, &primary_index);
    let primary_mem = primary_index.with_file_name("memory.db");

    let dep_a_root_raw = tmp.path().join("dep-a");
    fs::create_dir_all(&dep_a_root_raw).expect("dep-a dir");
    let dep_a_index_raw = create_inkentry_dir(&dep_a_root_raw);
    let dep_a_root = canon(&dep_a_root_raw);
    let dep_a_index = canon(&dep_a_index_raw);
    let dep_a_mem = dep_a_index.with_file_name("memory.db");

    let dep_b_root_raw = tmp.path().join("dep-b");
    fs::create_dir_all(&dep_b_root_raw).expect("dep-b dir");
    let dep_b_index_raw = create_inkentry_dir(&dep_b_root_raw);
    let dep_b_root = canon(&dep_b_root_raw);
    let dep_b_index = canon(&dep_b_index_raw);
    let dep_b_mem = dep_b_index.with_file_name("memory.db");

    let conn_a = open_memory_db(&dep_a_mem);
    seed_note(
        &conn_a,
        "decision",
        "Dep-A policy",
        "body",
        &["locked"],
        "active",
    );
    let conn_b = open_memory_db(&dep_b_mem);
    seed_note(
        &conn_b,
        "decision",
        "Dep-B policy",
        "body",
        &["locked"],
        "active",
    );

    let reg = TestRegistry::new(&home);
    let primary_id = reg.register(&primary_root, &primary_index);
    let dep_a_id = reg.register(&dep_a_root, &dep_a_index);
    let dep_b_id = reg.register(&dep_b_root, &dep_b_index);
    reg.add_dep(primary_id, dep_a_id);
    reg.add_dep(primary_id, dep_b_id);

    let output = inkentry_bin_in(&home)
        .env("HOME", &home)
        .env("INKENTRY_REGISTRY_DIR", registry_dir(&home))
        .env_remove("XDG_CONFIG_HOME")
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&primary_root)
        .arg("--config")
        .arg(&primary_config)
        .arg("memory")
        .arg("--db")
        .arg(&primary_mem)
        .args(["list", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let notes: Vec<serde_json::Value> = serde_json::from_slice(&output).expect("valid JSON");
    let titles: Vec<&str> = notes.iter().filter_map(|n| n["title"].as_str()).collect();

    assert_eq!(
        titles.iter().filter(|&&t| t == "Dep-A policy").count(),
        1,
        "Dep-A policy must appear exactly once; got: {titles:?}"
    );
    assert_eq!(
        titles.iter().filter(|&&t| t == "Dep-B policy").count(),
        1,
        "Dep-B policy must appear exactly once; got: {titles:?}"
    );
}

#[test]
fn missing_dep_memory_db_is_skipped_silently() {
    let tmp = TempDir::new().expect("create temp dir");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).expect("create home dir");

    let primary_root_raw = tmp.path().join("primary");
    fs::create_dir_all(&primary_root_raw).expect("primary dir");
    let primary_index_raw = create_inkentry_dir(&primary_root_raw);
    let primary_root = canon(&primary_root_raw);
    let primary_index = canon(&primary_index_raw);
    let primary_config = write_config(&primary_root, &primary_index);
    let primary_mem = primary_index.with_file_name("memory.db");

    let dep_a_root_raw = tmp.path().join("dep-a");
    fs::create_dir_all(&dep_a_root_raw).expect("dep-a dir");
    let dep_a_index_raw = create_inkentry_dir(&dep_a_root_raw);
    let dep_a_root = canon(&dep_a_root_raw);
    let dep_a_index = canon(&dep_a_index_raw);
    let dep_a_mem = dep_a_index.with_file_name("memory.db");
    let conn_a = open_memory_db(&dep_a_mem);
    seed_note(
        &conn_a,
        "decision",
        "Dep-a locked decision",
        "body",
        &["locked"],
        "active",
    );

    let dep_b_root_raw = tmp.path().join("dep-b");
    fs::create_dir_all(&dep_b_root_raw).expect("dep-b dir");
    let dep_b_index_raw = create_inkentry_dir(&dep_b_root_raw);
    let dep_b_root = canon(&dep_b_root_raw);
    let dep_b_index = canon(&dep_b_index_raw);
    // Deliberately do NOT create dep_b_index.with_file_name("memory.db").

    let reg = TestRegistry::new(&home);
    let primary_id = reg.register(&primary_root, &primary_index);
    let dep_a_id = reg.register(&dep_a_root, &dep_a_index);
    let dep_b_id = reg.register(&dep_b_root, &dep_b_index);
    reg.add_dep(primary_id, dep_a_id);
    reg.add_dep(primary_id, dep_b_id);

    let output = inkentry_bin_in(&home)
        .env("HOME", &home)
        .env("INKENTRY_REGISTRY_DIR", registry_dir(&home))
        .env_remove("XDG_CONFIG_HOME")
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&primary_root)
        .arg("--config")
        .arg(&primary_config)
        .arg("memory")
        .arg("--db")
        .arg(&primary_mem)
        .args(["list", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let notes: Vec<serde_json::Value> = serde_json::from_slice(&output).expect("valid JSON");
    let titles: Vec<&str> = notes.iter().filter_map(|n| n["title"].as_str()).collect();
    assert!(
        titles.contains(&"Dep-a locked decision"),
        "dep-a result must appear despite dep-b having no memory.db; got: {titles:?}"
    );
}

#[test]
fn sql_injection_in_dep_note_is_inert() {
    let (_tmp, home, primary_root, primary_index, primary_config, _dep_root, dep_mem) =
        setup_linked_projects();
    let primary_mem = primary_index.with_file_name("memory.db");

    let dep_conn = open_memory_db(&dep_mem);
    seed_note(
        &dep_conn,
        "decision",
        "'; DROP TABLE notes; --",
        "body: \" OR 1=1; --",
        &["locked"],
        "active",
    );
    seed_note(
        &dep_conn,
        "decision",
        "Benign note after injection payload",
        "This note must still exist.",
        &["locked"],
        "active",
    );

    let output = memory_cmd(&home, &primary_root, &primary_config, &primary_mem)
        .args(["list", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let notes: Vec<serde_json::Value> =
        serde_json::from_slice(&output).expect("valid JSON after injection payload");
    let titles: Vec<&str> = notes.iter().filter_map(|n| n["title"].as_str()).collect();
    assert!(
        titles.contains(&"Benign note after injection payload"),
        "notes table must survive injection payload; got: {titles:?}"
    );
}

#[test]
fn memory_store_list_excludes_archived_by_default() {
    register_sqlite_vec();

    use inkentry_core::storage::memory::MemoryStore;
    let store = MemoryStore::open(std::path::Path::new(":memory:")).expect("in-memory MemoryStore");

    store
        .add_note("decision", "Active note", "body", &[], &[], None, None)
        .expect("add active note");
    let (archived_id, _) = store
        .add_note("decision", "Archived note", "body", &[], &[], None, None)
        .expect("add to-be-archived note");
    store.archive(&archived_id).expect("archive note");

    let notes = store
        .list(Some("decision"), 100, false)
        .expect("list notes");
    let titles: Vec<&str> = notes.iter().map(|n| n.title.as_str()).collect();
    assert!(titles.contains(&"Active note"), "active note must appear");
    assert!(
        !titles.contains(&"Archived note"),
        "archived note must be excluded; got: {titles:?}"
    );
}

#[test]
fn memory_store_notes_have_no_source_project_by_default() {
    register_sqlite_vec();

    use inkentry_core::storage::memory::MemoryStore;
    let store = MemoryStore::open(std::path::Path::new(":memory:")).expect("in-memory MemoryStore");

    store
        .add_note(
            "decision",
            "Some decision",
            "body",
            &["locked"],
            &[],
            None,
            None,
        )
        .expect("add note");

    let notes = store.list(Some("decision"), 100, false).expect("list");
    assert_eq!(notes.len(), 1);
    assert!(
        notes[0].source_project.is_none(),
        "MemoryStore must not set source_project — CLI dep-pass sets it"
    );
    assert!(
        notes[0].source_project_path.is_none(),
        "MemoryStore must not set source_project_path — CLI dep-pass sets it"
    );
}
