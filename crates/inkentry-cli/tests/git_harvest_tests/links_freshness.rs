use crate::plumbing_helpers;
use plumbing_helpers::{inkentry_bin_in, register_sqlite_vec};

use assert_cmd::Command;
use predicates::prelude::*;
use rusqlite::Connection;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn registry_dir(home: &Path) -> PathBuf {
    home.join(".config").join("inkentry")
}

// Canonicalized as the product does, so registry `root_path` matches `current_dir()` (macOS `/var` vs `/private/var`).
fn canon(p: &Path) -> PathBuf {
    inkentry_core::utils::canonicalize(p)
}

struct TestRegistry {
    conn: Connection,
}

impl TestRegistry {
    fn new(home: &Path) -> Self {
        let config_dir = registry_dir(home);
        fs::create_dir_all(&config_dir).expect("create registry dir");
        let conn = Connection::open(config_dir.join("registry.db")).expect("open registry db");
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

fn create_index_db(root: &Path) -> PathBuf {
    let inkentry_dir = root.join(".inkentry");
    fs::create_dir_all(&inkentry_dir).expect("create .inkentry dir");
    inkentry_dir.join("index.db")
}

fn write_config(dir: &Path, index_db: &Path) -> PathBuf {
    let cfg = format!(
        "db_path = {:?}\napi_base_url = \"http://127.0.0.1:1\"\nllm_model = \"none\"\n",
        canon(index_db)
    );
    let config_path = dir.join("config.toml");
    fs::write(&config_path, cfg).expect("write config");
    config_path
}

// Stores the root-relative path with the real blake3 hash, as a fresh `inkentry index` would.
fn seed_indexed_file(index_db: &Path, root: &Path, rel: &str, content: &[u8]) {
    register_sqlite_vec();
    fs::write(root.join(rel), content).expect("write source file");
    let hash = format!("{}", blake3::hash(content));
    let db = inkentry_core::storage::Database::open(index_db).expect("open index db");
    db.upsert_file(rel, Some("rust"), &hash, 0)
        .expect("seed indexed file");
    drop(db);
}

struct Linked {
    _tmp: TempDir,
    home: PathBuf,
    primary_root: PathBuf,
    primary_config: PathBuf,
    dep_root: PathBuf,
    dep_index: PathBuf,
    dep_config: PathBuf,
}

fn setup() -> Linked {
    let tmp = TempDir::new().expect("create temp dir");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).expect("create home");

    let primary_root = canon(&{
        let p = tmp.path().join("primary");
        fs::create_dir_all(&p).unwrap();
        p
    });
    let primary_index = create_index_db(&primary_root);
    // A stub primary index.db is enough — links only opens the *dep* DB.
    drop(Connection::open(&primary_index).expect("stub primary index"));
    let primary_config = write_config(&primary_root, &primary_index);

    let dep_root = canon(&{
        let p = tmp.path().join("dep");
        fs::create_dir_all(&p).unwrap();
        p
    });
    let dep_index = create_index_db(&dep_root);
    seed_indexed_file(&dep_index, &dep_root, "shared.rs", b"pub fn shared() {}\n");
    let dep_config = write_config(&dep_root, &dep_index);

    let reg = TestRegistry::new(&home);
    let primary_id = reg.register(&primary_root, &primary_index);
    let dep_id = reg.register(&dep_root, &dep_index);
    reg.add_dep(primary_id, dep_id);

    Linked {
        _tmp: tmp,
        home,
        primary_root,
        primary_config,
        dep_root,
        dep_index,
        dep_config,
    }
}

fn cmd(env: &Linked, cwd: &Path, config: &Path) -> Command {
    let mut c = inkentry_bin_in(&env.home);
    c.env("HOME", &env.home)
        .env("INKENTRY_REGISTRY_DIR", registry_dir(&env.home))
        .env_remove("XDG_CONFIG_HOME")
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(cwd)
        .arg("--config")
        .arg(config);
    c
}

#[test]
fn links_check_reports_freshly_indexed_dep_as_fresh() {
    let env = setup();

    cmd(&env, &env.primary_root, &env.primary_config)
        .args(["links", "check"])
        .assert()
        .success()
        .stdout(predicate::str::contains("fresh"));
}

#[test]
fn links_list_shows_freshly_indexed_dep_not_stale() {
    let env = setup();

    let out = cmd(&env, &env.primary_root, &env.primary_config)
        .args(["links", "list", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let infos: Vec<serde_json::Value> = serde_json::from_slice(&out).expect("valid JSON");
    let dep = infos
        .iter()
        .find(|i| i["name"] == "dep")
        .expect("dep entry present");
    assert_eq!(
        dep["status"], "fresh",
        "freshly-indexed dep must be fresh, got: {infos:?}"
    );
}

// Guards against over-correcting into never-stale.
#[test]
fn links_check_reports_modified_dep_as_stale() {
    let env = setup();

    fs::write(
        env.dep_root.join("shared.rs"),
        b"pub fn shared() { changed }\n",
    )
    .expect("modify dep file");

    cmd(&env, &env.primary_root, &env.primary_config)
        .args(["links", "check"])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("stale"));
}

#[test]
fn links_check_and_in_project_freshness_agree_on_fresh_dep() {
    let env = setup();

    // A fresh tree emits no rows and exits 1 (no results), the inverse polarity of a success check.
    cmd(&env, &env.dep_root, &env.dep_config)
        .args(["plumbing", "ls-files", "--stale"])
        .arg("--db")
        .arg(&env.dep_index)
        .arg("--root")
        .arg(&env.dep_root)
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty());

    cmd(&env, &env.primary_root, &env.primary_config)
        .args(["links", "check"])
        .assert()
        .success();
}
