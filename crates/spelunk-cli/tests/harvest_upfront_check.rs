//! Integration tests for the `spelunk memory harvest` up-front config check
//! (issue #287).
//!
//! The check fires before any LLM or git I/O, so these tests don't need a
//! running server or a real git repo.  They only exercise three branches:
//!   (a) neither server_url nor llm_model  → immediate error with the exact message
//!   (b) server_url set, llm_model absent  → check passes (fails later for another reason)
//!   (c) llm_model set, server_url absent  → check passes (fails later for another reason)

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::tempdir;

const UPFRONT_ERROR: &str = "Harvest requires a remote server (--remote-url) or a local model (SPELUNK_LLM_URL). \
     Run 'spelunk sync' to push entries to the server, or configure a model.";

// ── helpers ───────────────────────────────────────────────────────────────────

/// Write a minimal config file under `dir`.  `extra` is appended verbatim.
fn write_harvest_config(dir: &std::path::Path, extra: &str) -> std::path::PathBuf {
    let db_path = dir.join("memory.db");
    let config_path = dir.join("config.toml");
    let content = format!(
        "db_path = {:?}\napi_base_url = \"http://127.0.0.1:1234\"\n{extra}",
        db_path
    );
    fs::write(&config_path, content).expect("write config.toml");
    config_path
}

/// Build a `spelunk --config <cfg> memory harvest --git-range HEAD~1..HEAD` command.
fn harvest_cmd(config_path: &std::path::Path, dir: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("spelunk").unwrap();
    cmd.current_dir(dir)
        .env_remove("SPELUNK_SERVER_URL")
        .env_remove("SPELUNK_MEMORY_SERVER_URL")
        .env_remove("SPELUNK_LLM_URL")
        .arg("--config")
        .arg(config_path)
        .arg("memory")
        .arg("harvest")
        .arg("--git-range")
        .arg("HEAD~1..HEAD");
    cmd
}

// ── (a) neither server_url nor llm_model → up-front error ────────────────────

#[test]
fn harvest_fails_with_actionable_error_when_no_server_and_no_model() {
    let temp = tempdir().unwrap();
    // Config has no server_url and no llm_model.
    let config_path = write_harvest_config(temp.path(), "");

    harvest_cmd(&config_path, temp.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains(UPFRONT_ERROR));
}

// ── (b) server_url set, llm_model absent → check passes ──────────────────────

#[test]
fn harvest_check_passes_when_server_url_is_set() {
    let temp = tempdir().unwrap();
    // server_url is set; project_id is required alongside it.
    let config_path = write_harvest_config(
        temp.path(),
        "server_url = \"http://127.0.0.1:7777\"\nproject_id = \"test/proj\"\n",
    );

    // The command will fail (no live server, no git repo) but NOT with the
    // up-front "Harvest requires" message.
    harvest_cmd(&config_path, temp.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains(UPFRONT_ERROR).not());
}

// ── (c) llm_model set, server_url absent → check passes ──────────────────────

#[test]
fn harvest_check_passes_when_llm_model_is_set() {
    let temp = tempdir().unwrap();
    // llm_model is set; no server_url.
    let config_path = write_harvest_config(temp.path(), "llm_model = \"local-chat-model\"\n");

    // Same reasoning: fails later, but NOT with the up-front message.
    harvest_cmd(&config_path, temp.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains(UPFRONT_ERROR).not());
}
