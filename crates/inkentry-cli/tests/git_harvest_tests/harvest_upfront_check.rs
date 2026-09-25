use crate::plumbing_helpers;
use plumbing_helpers::inkentry_bin;

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::tempdir;

// Both invocation paths name the canonical top-level command.
const SERVER_REQUIRED: &str = "'inkentry harvest' requires inkentry-server";

fn write_harvest_config(dir: &std::path::Path, extra: &str) -> std::path::PathBuf {
    // Without a local `.inkentry/` harvest fails closed, pre-empting the server-gate check under test.
    fs::create_dir_all(dir.join(".inkentry")).expect("create .inkentry");
    let db_path = dir.join("memory.db");
    let config_path = dir.join("config.toml");
    let content = format!(
        "db_path = {:?}\napi_base_url = \"http://127.0.0.1:1234\"\n{extra}",
        db_path
    );
    fs::write(&config_path, content).expect("write config.toml");
    config_path
}

fn harvest_cmd(config_path: &std::path::Path, dir: &std::path::Path) -> Command {
    let mut cmd = inkentry_bin();
    cmd.current_dir(dir)
        .env_remove("INKENTRY_SERVER_URL")
        .env_remove("INKENTRY_LLM_URL")
        // Loopback discovery off, so the gate fires even when a local inkentry-server is running.
        .env("INKENTRY_NO_SERVER", "1")
        .arg("--config")
        .arg(config_path)
        .arg("memory")
        .arg("harvest")
        .arg("--git-range")
        .arg("HEAD~1..HEAD");
    cmd
}

fn toplevel_harvest_cmd(config_path: &std::path::Path, dir: &std::path::Path) -> Command {
    let mut cmd = inkentry_bin();
    cmd.current_dir(dir)
        .env_remove("INKENTRY_SERVER_URL")
        .env_remove("INKENTRY_LLM_URL")
        .env("INKENTRY_NO_SERVER", "1")
        .arg("--config")
        .arg(config_path)
        .arg("harvest")
        .arg("--git-range")
        .arg("HEAD~1..HEAD");
    cmd
}

#[test]
fn harvest_fails_with_actionable_error_when_no_server_and_no_model() {
    let temp = tempdir().unwrap();
    let config_path = write_harvest_config(temp.path(), "");

    harvest_cmd(&config_path, temp.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains(SERVER_REQUIRED))
        .stderr(predicate::str::contains("inkentry server start"))
        .stderr(predicate::str::contains("server_url").not());
}

#[test]
fn toplevel_harvest_fails_with_actionable_error_when_no_server() {
    let temp = tempdir().unwrap();
    let config_path = write_harvest_config(temp.path(), "");

    toplevel_harvest_cmd(&config_path, temp.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains(SERVER_REQUIRED))
        .stderr(predicate::str::contains("inkentry server start"))
        .stderr(predicate::str::contains("server_url").not())
        .stderr(predicate::str::contains("deprecated").not());
}

#[test]
fn harvest_check_passes_when_server_url_is_set() {
    let temp = tempdir().unwrap();
    // `Config::load` reads `server_url`/`project_id` only from project-level `.inkentry/config.toml`, so they
    // are written separately from `extra`. `cloud_first` is needed: a bare `server_url` defaults to
    // `local_first`, which never uses it for inference, so the gate would still fire.
    let config_path = write_harvest_config(temp.path(), "mode = \"cloud_first\"\n");
    plumbing_helpers::write_project_server_config(temp.path(), "http://127.0.0.1:0", "test/proj");

    // `INKENTRY_NO_SERVER=1` forces Offline regardless of mode, so it is removed here. `cloud_first` does not
    // depend on the URL being reachable, so a local server on 4655 cannot change the outcome.
    harvest_cmd(&config_path, temp.path())
        .env_remove("INKENTRY_NO_SERVER")
        .assert()
        .failure()
        .stderr(predicate::str::contains(SERVER_REQUIRED).not());
}

#[test]
fn harvest_fails_when_llm_model_set_but_no_server_url() {
    let temp = tempdir().unwrap();
    let config_path = write_harvest_config(temp.path(), "llm_model = \"local-chat-model\"\n");

    harvest_cmd(&config_path, temp.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains(SERVER_REQUIRED));
}
