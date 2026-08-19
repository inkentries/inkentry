// Which memory store the memory-targeting plumbing commands act on.
//
// The marker for a local project is the `.inkentry/` directory, not a present
// `index.db`. A directory configured by `inkentry init --no-index` (or simply
// never indexed) has `.inkentry/config.toml` and no `index.db`, and deriving
// the memory path from the index walk sends `plumbing push` to the
// machine-global store while `memory add`/`list`/`sync` in that same directory
// stay on the project store. Two commands, one directory, two answers, and
// push writes.
//
// Outside any project the global store is still the honest answer, so these
// also pin that the store actually used is named on stderr rather than left to
// be misread as an empty local delta.

use crate::plumbing_helpers;
use plumbing_helpers::{init_git_repo, inkentry_bin_in, register_sqlite_vec};

use std::path::{Path, PathBuf};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use inkentry_core::storage::MemoryStore;
use inkentry_core::test_support::git_command;

// No characters `encode_project_id` would percent-encode, so the mocked route
// path can be matched literally.
const PROJECT_SLUG: &str = "acme-widget";

async fn mount_health(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "capabilities": ["memory"],
        })))
        .mount(server)
        .await;
}

async fn mount_batch_ok(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path(format!("/v1/projects/{PROJECT_SLUG}/memory/batch")))
        .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
            "created": 1, "skipped": 0, "failed": 0, "results": []
        })))
        .mount(server)
        .await;
}

fn seed_store(mem_path: &Path, title: &str) {
    register_sqlite_vec();
    std::fs::create_dir_all(mem_path.parent().unwrap()).expect("create store dir");
    let store = MemoryStore::open(mem_path).expect("open memory.db");
    store
        .add_note("note", title, "body", &[], &[], None, None)
        .expect("seed note");
}

// A `--config` file whose `db_path` is the machine-global index, well outside
// any project. Its sibling `memory.db` is the store the index walk reaches.
fn write_global_config(home: &Path) -> (PathBuf, PathBuf) {
    let global_db = home.join("global").join("index.db");
    std::fs::create_dir_all(global_db.parent().unwrap()).expect("create global dir");
    let config_path = home.join("config.toml");
    std::fs::write(
        &config_path,
        format!("db_path = {global_db:?}\nllm_model = \"test-chat\"\n"),
    )
    .expect("write config.toml");
    (config_path, global_db.with_file_name("memory.db"))
}

// `.inkentry/config.toml` with no `index.db` beside it: a configured project
// that has never been indexed.
fn make_unindexed_project(proj: &Path, server_url: &str) {
    plumbing_helpers::write_project_server_config(proj, server_url, PROJECT_SLUG);
    assert!(
        !proj.join(".inkentry").join("index.db").exists(),
        "the fixture is only meaningful without an index.db"
    );
}

fn report(stdout: &[u8]) -> serde_json::Value {
    serde_json::from_slice(stdout).unwrap_or_else(|e| {
        panic!(
            "a plumbing report must be one JSON object on stdout: {e}; stdout={:?}",
            String::from_utf8_lossy(stdout)
        )
    })
}

// Titles of every note the server was asked to accept, across all batches.
async fn pushed_titles(server: &MockServer) -> Vec<String> {
    let mut titles = Vec::new();
    for req in server.received_requests().await.unwrap_or_default() {
        if !req.url.path().ends_with("/memory/batch") {
            continue;
        }
        let body: serde_json::Value = match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(entries) = body.get("entries").and_then(|e| e.as_array()) else {
            continue;
        };
        for entry in entries {
            if let Some(t) = entry.get("title").and_then(|t| t.as_str()) {
                titles.push(t.to_string());
            }
        }
    }
    titles
}

async fn mount_since_one_entry(server: &MockServer, title: &str) {
    Mock::given(method("GET"))
        .and(path(format!("/v1/projects/{PROJECT_SLUG}/memory/since")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "entries": [{
                "id": "01890000-0000-7000-8000-0000000000aa",
                "kind": "decision",
                "title": title,
                "body": "written by a teammate",
                "created_at": "2026-06-19T01:00:00Z",
            }]
        })))
        .mount(server)
        .await;
}

// Sorted so the assertion does not depend on the order rows come back in.
fn titles_in(mem_path: &Path) -> Vec<String> {
    register_sqlite_vec();
    let store = MemoryStore::open(mem_path).expect("open memory.db");
    let mut titles: Vec<String> = store
        .list(None, 100, true)
        .expect("list notes")
        .into_iter()
        .map(|n| n.title)
        .collect();
    titles.sort();
    titles
}

#[tokio::test]
async fn push_uses_the_project_store_of_a_configured_but_unindexed_project() {
    let server = MockServer::start().await;
    mount_health(&server).await;
    mount_batch_ok(&server).await;

    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    let (config_path, global_mem) = write_global_config(home.path());
    make_unindexed_project(proj.path(), &server.uri());
    seed_store(
        &proj.path().join(".inkentry").join("memory.db"),
        "project store entry",
    );
    seed_store(&global_mem, "global store entry");

    let out = inkentry_bin_in(home.path())
        .current_dir(proj.path())
        .arg("--config")
        .arg(&config_path)
        .args(["plumbing", "push"])
        .output()
        .expect("run plumbing push");

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let report = report(&out.stdout);
    assert_eq!(
        pushed_titles(&server).await,
        vec!["project store entry".to_string()],
        "push must act on the project store; stderr={stderr}"
    );
    assert_eq!(report["attempted"], 1, "report={report}; stderr={stderr}");
    assert_eq!(report["created"], 1, "report={report}; stderr={stderr}");
    assert_eq!(
        out.status.code(),
        Some(0),
        "an entry moved, so this is not an empty delta; stderr={stderr}"
    );
}

#[tokio::test]
async fn memory_list_and_push_agree_on_the_store_without_an_index() {
    let server = MockServer::start().await;
    mount_health(&server).await;
    mount_batch_ok(&server).await;

    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    let (config_path, global_mem) = write_global_config(home.path());
    init_git_repo(proj.path());
    make_unindexed_project(proj.path(), &server.uri());
    seed_store(&global_mem, "global store entry");

    let bin = |home: &Path| {
        let mut c = inkentry_bin_in(home);
        c.current_dir(proj.path()).arg("--config").arg(&config_path);
        c
    };

    let added = bin(home.path())
        .args([
            "memory",
            "add",
            "-k",
            "note",
            "-t",
            "Agreement probe",
            "-b",
            "written by memory add",
        ])
        .output()
        .expect("run memory add");
    assert!(
        added.status.success(),
        "memory add failed: {}",
        String::from_utf8_lossy(&added.stderr)
    );

    let listed = bin(home.path())
        .args(["memory", "list"])
        .output()
        .expect("run memory list");
    let listed_stdout = String::from_utf8_lossy(&listed.stdout).into_owned();

    let pushed = bin(home.path())
        .args(["plumbing", "push"])
        .output()
        .expect("run plumbing push");
    let push_report = report(&pushed.stdout);

    assert!(
        listed_stdout.contains("Agreement probe"),
        "memory list must see the entry it wrote; stdout={listed_stdout}"
    );
    assert_eq!(
        pushed_titles(&server).await,
        vec!["Agreement probe".to_string()],
        "push must mean the same store memory list does; report={push_report}, \
         list stdout={listed_stdout}, push stderr={}",
        String::from_utf8_lossy(&pushed.stderr)
    );
    assert_eq!(push_report["attempted"], 1, "report={push_report}");
}

#[tokio::test]
async fn push_from_a_linked_worktree_uses_the_main_worktree_store() {
    let server = MockServer::start().await;
    mount_health(&server).await;
    mount_batch_ok(&server).await;

    let home = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let (config_path, global_mem) = write_global_config(home.path());

    let main_root = tmp.path().join("main");
    std::fs::create_dir_all(&main_root).expect("create main worktree");
    init_git_repo(&main_root);
    make_unindexed_project(&main_root, &server.uri());
    seed_store(
        &main_root.join(".inkentry").join("memory.db"),
        "main worktree entry",
    );
    seed_store(&global_mem, "global store entry");

    let wt_root = tmp.path().join("linked");
    let status = git_command(&main_root)
        .args(["worktree", "add", "-b", "feat"])
        .arg(&wt_root)
        .status()
        .expect("run git worktree add");
    assert!(status.success(), "git worktree add failed");
    assert!(
        !wt_root.join(".inkentry").exists(),
        "the linked worktree must have no .inkentry/ of its own"
    );

    // The linked worktree has no `.inkentry/config.toml` to discover, so the
    // team-server settings come from the environment instead.
    let out = inkentry_bin_in(home.path())
        .current_dir(&wt_root)
        .env("INKENTRY_SERVER_URL", server.uri())
        .env("INKENTRY_PROJECT_ID", PROJECT_SLUG)
        .arg("--config")
        .arg(&config_path)
        .args(["plumbing", "push"])
        .output()
        .expect("run plumbing push");

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(
        pushed_titles(&server).await,
        vec!["main worktree entry".to_string()],
        "a linked worktree shares the main worktree's store; stderr={stderr}"
    );
    assert_eq!(report(&out.stdout)["attempted"], 1, "stderr={stderr}");
}

#[tokio::test]
async fn push_outside_any_project_names_the_global_store_it_acts_on() {
    let server = MockServer::start().await;
    mount_health(&server).await;
    mount_batch_ok(&server).await;

    let home = TempDir::new().unwrap();
    let elsewhere = TempDir::new().unwrap();
    let (config_path, global_mem) = write_global_config(home.path());
    seed_store(&global_mem, "global store entry");
    assert!(
        !elsewhere.path().join(".inkentry").exists(),
        "this directory must not be a project"
    );

    let out = inkentry_bin_in(home.path())
        .current_dir(elsewhere.path())
        .env("INKENTRY_SERVER_URL", server.uri())
        .env("INKENTRY_PROJECT_ID", PROJECT_SLUG)
        .arg("--config")
        .arg(&config_path)
        .args(["plumbing", "push"])
        .output()
        .expect("run plumbing push");

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(
        pushed_titles(&server).await,
        vec!["global store entry".to_string()],
        "outside a project the global store is the honest answer; stderr={stderr}"
    );
    assert!(
        stderr.contains(&global_mem.display().to_string()),
        "the store actually acted on must be named: stderr={stderr}"
    );
}

// `read-memory` derives its store the same way `push` does, so it reached the
// global store in the same directory. Unlike push it never writes, but a
// silently-wrong read is what a caller then acts on.
#[tokio::test]
async fn read_memory_uses_the_project_store_of_a_configured_but_unindexed_project() {
    let server = MockServer::start().await;
    mount_health(&server).await;

    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    let (config_path, global_mem) = write_global_config(home.path());
    make_unindexed_project(proj.path(), &server.uri());
    seed_store(
        &proj.path().join(".inkentry").join("memory.db"),
        "project store entry",
    );
    seed_store(&global_mem, "global store entry");

    let out = inkentry_bin_in(home.path())
        .current_dir(proj.path())
        .arg("--config")
        .arg(&config_path)
        .args(["plumbing", "read-memory"])
        .output()
        .expect("run plumbing read-memory");

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        stdout.contains("project store entry") && !stdout.contains("global store entry"),
        "read-memory must read the project store; stdout={stdout}, stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// Pull is the write direction for other people's entries, so a wrong store
// here deposits team memory where this project's own commands never look.
#[tokio::test]
async fn pull_writes_into_the_project_store_of_a_configured_but_unindexed_project() {
    let server = MockServer::start().await;
    mount_health(&server).await;
    mount_since_one_entry(&server, "teammate entry").await;

    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    let (config_path, global_mem) = write_global_config(home.path());
    make_unindexed_project(proj.path(), &server.uri());
    let project_mem = proj.path().join(".inkentry").join("memory.db");
    seed_store(&project_mem, "project store entry");
    seed_store(&global_mem, "global store entry");

    let out = inkentry_bin_in(home.path())
        .current_dir(proj.path())
        .arg("--config")
        .arg(&config_path)
        .args(["plumbing", "pull"])
        .output()
        .expect("run plumbing pull");

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let report = report(&out.stdout);
    assert_eq!(
        titles_in(&project_mem),
        vec![
            "project store entry".to_string(),
            "teammate entry".to_string(),
        ],
        "the pulled entry belongs in the project store; report={report}, stderr={stderr}"
    );
    assert_eq!(
        titles_in(&global_mem),
        vec!["global store entry".to_string()],
        "the global store must not receive team memory; report={report}, stderr={stderr}"
    );
    assert_eq!(report["applied"], 1, "report={report}; stderr={stderr}");
    assert_eq!(
        out.status.code(),
        Some(0),
        "an entry was applied, so this is not an empty delta; stderr={stderr}"
    );
}
