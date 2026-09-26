use crate::plumbing_helpers;
use plumbing_helpers::inkentry_bin;

use assert_cmd::Command;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tempfile::TempDir;

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

fn write_config(dir: &Path) -> (PathBuf, PathBuf) {
    let inkentry_dir = dir.join(".inkentry");
    std::fs::create_dir_all(&inkentry_dir).expect("create .inkentry");
    let index_db = inkentry_dir.join("index.db");
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "db_path = {:?}\nllm_model = \"test-model\"\n",
            index_db.display().to_string()
        ),
    )
    .expect("write config");
    let mem_path = index_db.with_file_name("memory.db");
    (config_path, mem_path)
}

// Two rows sharing `{kind, title, body}`. The schema's UNIQUE `idx_notes_entity_id` forbids that, so the seed
// drops it to mimic a hand-edited database, the only way the condition `memory dedupe` handles can arise.
fn seed_duplicate_group(mem_path: &Path) {
    ensure_sqlite_vec();
    std::fs::create_dir_all(mem_path.parent().unwrap()).expect("create .inkentry dir");
    drop(inkentry_core::storage::MemoryStore::open(mem_path).expect("create memory.db"));
    let conn = Connection::open(mem_path).expect("open memory.db");
    conn.execute_batch("DROP INDEX idx_notes_entity_id;")
        .expect("drop the uniqueness the seed violates");

    for (i, created_at) in [1_700_000_001_i64, 1_700_000_002_i64]
        .into_iter()
        .enumerate()
    {
        conn.execute(
            "INSERT INTO notes (uuid, kind, title, body, created_at, entity_id) \
             VALUES (?1, 'decision', 'dup', 'body', ?2, 'duplicate-entity-id')",
            rusqlite::params![
                format!("0199a0f1-4d3c-7c2a-9b1e-00000000000{i}"),
                created_at
            ],
        )
        .expect("seed duplicate row");
    }
}

fn count_memory_notes(mem_path: &Path) -> i64 {
    ensure_sqlite_vec();
    let conn = Connection::open(mem_path).expect("open memory.db");
    conn.query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))
        .unwrap_or(0)
}

fn dedupe_cmd(config_path: &Path) -> Command {
    let tmp_dir = config_path
        .parent()
        .expect("config_path must have a parent");
    let mut cmd = inkentry_bin();
    cmd.current_dir(tmp_dir)
        .env("INKENTRY_NO_SERVER", "1")
        .arg("--config")
        .arg(config_path)
        .arg("memory")
        .arg("dedupe");
    cmd
}

#[test]
fn dry_run_reports_counts_and_writes_nothing() {
    let tmp = TempDir::new().unwrap();
    let (config_path, mem_path) = write_config(tmp.path());
    seed_duplicate_group(&mem_path);
    let before = count_memory_notes(&mem_path);
    assert_eq!(before, 2, "precondition: two duplicate rows seeded");

    let output = dedupe_cmd(&config_path)
        .arg("--dry-run")
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = String::from_utf8(output).unwrap();
    let value: serde_json::Value =
        serde_json::from_str(text.trim()).expect("stdout should be valid JSON");
    assert_eq!(value["duplicate_groups"].as_i64(), Some(1));
    assert_eq!(value["rows_collapsed"].as_i64(), Some(1));

    assert_eq!(
        count_memory_notes(&mem_path),
        before,
        "dry-run must write nothing"
    );
}

#[test]
fn real_run_collapses_and_row_count_drops_by_rows_collapsed() {
    let tmp = TempDir::new().unwrap();
    let (config_path, mem_path) = write_config(tmp.path());
    seed_duplicate_group(&mem_path);
    let before = count_memory_notes(&mem_path);

    let output = dedupe_cmd(&config_path)
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = String::from_utf8(output).unwrap();
    let value: serde_json::Value =
        serde_json::from_str(text.trim()).expect("stdout should be valid JSON");
    let rows_collapsed = value["rows_collapsed"].as_i64().expect("rows_collapsed");
    assert_eq!(rows_collapsed, 1);

    assert_eq!(count_memory_notes(&mem_path), before - rows_collapsed);
}

#[test]
fn zero_duplicates_reports_all_zero_and_writes_nothing() {
    let tmp = TempDir::new().unwrap();
    let (config_path, mem_path) = write_config(tmp.path());

    // A single `memory add` on an empty store promotes the index, so no duplicates can exist.
    inkentry_bin()
        .current_dir(tmp.path())
        .env("INKENTRY_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("add")
        .arg("--title")
        .arg("solo")
        .arg("--body")
        .arg("only entry")
        .assert()
        .success();

    let before = count_memory_notes(&mem_path);
    assert_eq!(before, 1);

    let output = dedupe_cmd(&config_path)
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).unwrap();
    let value: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(value["duplicate_groups"].as_i64(), Some(0));
    assert_eq!(value["rows_collapsed"].as_i64(), Some(0));
    assert_eq!(value["tags_merged"].as_i64(), Some(0));
    assert_eq!(value["linked_files_merged"].as_i64(), Some(0));
    assert_eq!(value["supersede_edges_repointed"].as_i64(), Some(0));
    assert_eq!(value["supersede_self_edges_dropped"].as_i64(), Some(0));

    assert_eq!(
        count_memory_notes(&mem_path),
        before,
        "no writes on zero groups"
    );
}

#[test]
fn default_format_emits_human_readable_line() {
    let tmp = TempDir::new().unwrap();
    let (config_path, mem_path) = write_config(tmp.path());
    seed_duplicate_group(&mem_path);

    let assert = dedupe_cmd(&config_path).arg("--dry-run").assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.trim().is_empty(),
        "text-format summary goes to stderr, not stdout: {stdout}"
    );
    assert!(
        stderr.contains("dedupe") && stderr.contains("duplicate_groups=1"),
        "expected a human-readable dedupe summary line, got: {stderr}"
    );
}
