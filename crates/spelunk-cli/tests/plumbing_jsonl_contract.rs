// Conformance tests for the plumbing JSONL stability contract (docs/stability.md).
//
// Each test runs a real plumbing command and checks its emitted JSONL against
// the committed golden schema. Field presence and types only: a removal,
// rename, or retype fails here; an added field passes.
//
// The checker's own accept/reject behaviour is pinned separately, in
// `schema_contract_checker.rs`.

mod plumbing_helpers;
mod schema_contract;

use plumbing_helpers::{
    FIXTURE_PROJECT_ID, IndexEmbedResponder, index_fixture_project, parse_jsonl, spelunk_bin,
    spelunk_cmd, write_config, write_project_server_config,
};
use schema_contract::{CommandSchema, assert_conforms, load_golden};

use std::path::Path;
use tempfile::TempDir;

// The dimension the mock embedder in `plumbing_helpers` returns, and therefore
// the width of every vector in a fixture-backed index.
const FIXTURE_EMBEDDING_DIM: usize = 896;

fn schema_for(command: &str) -> CommandSchema {
    load_golden()
        .remove(command)
        .unwrap_or_else(|| panic!("golden schema has no entry for `{command}`"))
}

fn check(command: &str, stdout: &[u8]) {
    assert_conforms(command, &schema_for(command), &parse_jsonl(stdout));
}

// ── the contract covers every command that exists ────────────────────────────

// A new plumbing command that ships without a golden entry is an unguarded
// stable surface, which is the failure this whole suite exists to prevent. The
// command list comes from clap's own help rather than a second hand-maintained
// list, so it cannot drift from the binary.
#[test]
fn golden_schema_covers_every_plumbing_subcommand() {
    let help = spelunk_bin()
        .args(["plumbing", "--help"])
        .output()
        .expect("run plumbing --help");
    let help = String::from_utf8(help.stdout).expect("help is utf-8");

    let commands_section = help
        .split_once("Commands:")
        .expect("plumbing --help lists a Commands: section")
        .1;

    let mut shipped: Vec<String> = Vec::new();
    for line in commands_section.lines() {
        // Subcommand rows are indented and start with the command name. The
        // section ends at the blank line before `Options:`, but it also *opens*
        // with one, so an empty line only terminates once a row has been seen.
        if line.trim().is_empty() {
            if shipped.is_empty() {
                continue;
            }
            break;
        }
        let Some(name) = line.split_whitespace().next() else {
            continue;
        };
        if name == "help" {
            continue;
        }
        shipped.push(name.to_string());
    }
    shipped.sort();
    assert!(
        !shipped.is_empty(),
        "parsed no subcommands out of `plumbing --help`; the parser needs updating"
    );

    let mut declared: Vec<String> = load_golden().keys().cloned().collect();
    declared.sort();

    assert_eq!(
        shipped,
        declared,
        "every plumbing command must declare a JSONL schema in {}",
        schema_contract::GOLDEN_RELATIVE_PATH
    );
}

// ── index-backed commands ────────────────────────────────────────────────────

#[test]
fn cat_chunks_output_matches_the_contract() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    let out = spelunk_cmd(&db_path, &config_path)
        .args(["cat-chunks", "src/lib.rs"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    check("cat-chunks", &out);
}

#[test]
fn ls_files_output_matches_the_contract() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    let out = spelunk_cmd(&db_path, &config_path)
        .arg("ls-files")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    check("ls-files", &out);
}

#[test]
fn hash_file_output_matches_the_contract() {
    let (_tmp, db_path, config_path) = index_fixture_project();
    let file = plumbing_helpers::fixture_path().join("src/lib.rs");

    let out = spelunk_cmd(&db_path, &config_path)
        .arg("hash-file")
        .arg(&file)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    check("hash-file", &out);
}

#[test]
fn graph_edges_output_matches_the_contract() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    // `main.rs` calls into `lib.rs`, so the edge table is non-empty for it.
    // Asserting success (not "success or exit 1") keeps this from degrading
    // into a test that passes by never checking anything.
    let out = spelunk_cmd(&db_path, &config_path)
        .args(["graph-edges", "--file", "src/main.rs"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    check("graph-edges", &out);
}

#[test]
fn knn_output_matches_the_contract() {
    let (_tmp, db_path, config_path) = index_fixture_project();

    // The mock embedder gives every chunk the same vector, so any query of the
    // right width matches everything. Ordering is meaningless here; the schema
    // is not.
    let payload = serde_json::json!({
        "model": "test-model",
        "dimensions": FIXTURE_EMBEDDING_DIM,
        "vector": vec![0.1f32; FIXTURE_EMBEDDING_DIM],
    });

    let out = spelunk_cmd(&db_path, &config_path)
        .arg("knn")
        .write_stdin(payload.to_string())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    check("knn", &out);
}

// ── commands that need no index ──────────────────────────────────────────────

#[test]
fn parse_file_output_matches_the_contract() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("index.db");
    let config_path = write_config(tmp.path(), &db_path, "http://127.0.0.1:1");
    let file = plumbing_helpers::fixture_path().join("src/lib.rs");

    // `parse-file` returns before the index-exists check, so an absent DB here
    // is deliberate: it proves the command really is index-free.
    let out = spelunk_cmd(&db_path, &config_path)
        .arg("parse-file")
        .arg(&file)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    check("parse-file", &out);
}

#[tokio::test]
async fn embed_output_matches_the_contract() {
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
    let config = tmp.path().join("config.toml");
    std::fs::write(&config, "mode = \"cloud_first\"\n").unwrap();
    write_project_server_config(tmp.path(), &mock.uri(), FIXTURE_PROJECT_ID);

    let out = spelunk_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&config)
        .args(["plumbing", "embed"])
        .write_stdin("fn greet(name: &str) -> String\n")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    check("embed", &out);
}

#[test]
fn read_memory_output_matches_the_contract() {
    // `read-memory` derives the memory path from `--db`, and the index-exists
    // check in the plumbing dispatcher runs first, so a real index has to sit
    // next to the memory store even though this command never reads it.
    let (tmp, db_path, config_path) = index_fixture_project();
    let mem_path = db_path.with_file_name("memory.db");

    // The git-notes carrier follows the process CWD and ignores `--db`, so this
    // runs in the temp dir rather than the repo under test.
    spelunk_bin()
        .current_dir(tmp.path())
        .env("SPELUNK_NO_SERVER", "1")
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("--db")
        .arg(&mem_path)
        .args([
            "add",
            "--kind",
            "decision",
            "--title",
            "Plumbing JSONL fields are semver-bound",
            "--body",
            "Removing a field is a breaking change.",
        ])
        .assert()
        .success();

    let out = spelunk_cmd(&db_path, &config_path)
        .arg("read-memory")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    check("read-memory", &out);
}

#[test]
fn publish_notes_skip_output_matches_the_contract() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    plumbing_helpers::init_git_repo(&repo);

    // No `refs/notes/spelunk` in a fresh repo, so this takes the skip branch,
    // which is the outcome shape reachable without a remote.
    let out = spelunk_bin()
        .current_dir(&repo)
        .env("SPELUNK_NO_SERVER", "1")
        .args(["plumbing", "publish-notes", "origin"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows = parse_jsonl(&out);
    assert_eq!(rows.len(), 1, "publish-notes emits exactly one object");
    assert_eq!(
        rows[0].get("published").and_then(|v| v.as_bool()),
        Some(false),
        "a repo with no notes ref has nothing to publish"
    );
    assert!(
        rows[0].get("skipped").is_some(),
        "the skip shape carries a machine-readable reason: {}",
        rows[0]
    );
    check("publish-notes", &out);
}

#[test]
fn publish_notes_published_output_matches_the_contract() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let remote = tmp.path().join("remote.git");
    std::fs::create_dir_all(&repo).unwrap();
    plumbing_helpers::init_git_repo(&repo);
    init_bare_remote(&remote);
    git_in(
        &repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );

    // A real `memory add` writes the notes ref, so the published shape is
    // reached through the same path a user takes.
    spelunk_bin()
        .current_dir(&repo)
        .env("SPELUNK_NO_SERVER", "1")
        .args([
            "memory",
            "add",
            "--kind",
            "decision",
            "--title",
            "Publish-notes emits its outcome as JSONL",
            "--body",
            "The pre-push hook drops stdout.",
        ])
        .assert()
        .success();

    let out = spelunk_bin()
        .current_dir(&repo)
        .env("SPELUNK_NO_SERVER", "1")
        .args(["plumbing", "publish-notes", "origin"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows = parse_jsonl(&out);
    assert_eq!(
        rows[0].get("published").and_then(|v| v.as_bool()),
        Some(true),
        "expected a successful publish, got {}",
        rows[0]
    );
    check("publish-notes", &out);
}

fn init_bare_remote(path: &Path) {
    plumbing_helpers::isolate_git_config();
    let status = std::process::Command::new("git")
        .args(["init", "--bare", "-q"])
        .arg(path)
        .status()
        .expect("run git init --bare");
    assert!(status.success(), "git init --bare failed");
}

fn git_in(dir: &Path, args: &[&str]) {
    plumbing_helpers::isolate_git_config();
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}
