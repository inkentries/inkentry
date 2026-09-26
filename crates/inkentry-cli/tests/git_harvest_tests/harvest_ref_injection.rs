use crate::plumbing_helpers;
use plumbing_helpers::{init_git_repo, inkentry_bin};

use predicates::prelude::*;
use std::fs;
use tempfile::tempdir;

// `server_url`/`project_id` only satisfy harvest's upfront server gate; the guard under test fires first,
// so the address is never contacted. `Config::load` reads them only from project-level
// `.inkentry/config.toml`, so they land there rather than in the `--config` file.
// `mode = "cloud_first"` makes the gate accept a configured `server_url` without probing its reachability.
fn write_harvest_config(dir: &std::path::Path) -> std::path::PathBuf {
    let db_path = dir.join("memory.db");
    let config_path = dir.join("config.toml");
    let content = format!(
        "db_path = {:?}\napi_base_url = \"http://127.0.0.1:1234\"\nmode = \"cloud_first\"\n",
        db_path
    );
    fs::write(&config_path, content).expect("write config.toml");
    // Port 0 never has a listener, so the test cannot probe the developer's own daemon.
    plumbing_helpers::write_project_server_config(dir, "http://127.0.0.1:0", "test/proj");
    config_path
}

fn init_repo(dir: &std::path::Path) {
    init_git_repo(dir);
    // Without a local `.inkentry/`, harvest fails closed before reaching the ref check under test.
    fs::create_dir_all(dir.join(".inkentry")).expect("create .inkentry");
}

#[test]
fn harvest_rejects_option_like_branch_and_does_not_touch_victim_file() {
    let temp = tempdir().unwrap();
    init_repo(temp.path());

    let victim_dir = tempdir().unwrap();
    let victim_path = victim_dir.path().join("victim.txt");
    assert!(!victim_path.exists());

    let config_path = write_harvest_config(temp.path());

    let malicious_branch_arg = format!("--branch=--output={}", victim_path.display());

    let mut cmd = inkentry_bin();
    cmd.current_dir(temp.path())
        .env_remove("INKENTRY_SERVER_URL")
        .env_remove("INKENTRY_LLM_URL")
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("harvest")
        .arg(&malicious_branch_arg);

    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("rejected").or(predicate::str::contains("Invalid")));

    assert!(
        !victim_path.exists(),
        "option-injection via --branch must not create the victim file"
    );
}

#[test]
fn toplevel_harvest_rejects_option_like_branch_and_does_not_touch_victim_file() {
    let temp = tempdir().unwrap();
    init_repo(temp.path());

    let victim_dir = tempdir().unwrap();
    let victim_path = victim_dir.path().join("victim_toplevel.txt");
    assert!(!victim_path.exists());

    let config_path = write_harvest_config(temp.path());

    let malicious_branch_arg = format!("--branch=--output={}", victim_path.display());

    let mut cmd = inkentry_bin();
    cmd.current_dir(temp.path())
        .env_remove("INKENTRY_SERVER_URL")
        .env_remove("INKENTRY_LLM_URL")
        .arg("--config")
        .arg(&config_path)
        .arg("harvest")
        .arg(&malicious_branch_arg);

    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("rejected").or(predicate::str::contains("Invalid")));

    assert!(
        !victim_path.exists(),
        "option-injection via --branch must not create the victim file under the top-level command"
    );
}

#[test]
fn harvest_rejects_option_like_git_range() {
    let temp = tempdir().unwrap();
    init_repo(temp.path());

    let victim_dir = tempdir().unwrap();
    let victim_path = victim_dir.path().join("victim2.txt");

    let config_path = write_harvest_config(temp.path());

    let malicious_range_arg = format!("--git-range=--output={}", victim_path.display());

    let mut cmd = inkentry_bin();
    cmd.current_dir(temp.path())
        .env_remove("INKENTRY_SERVER_URL")
        .env_remove("INKENTRY_LLM_URL")
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("harvest")
        .arg(&malicious_range_arg);

    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("rejected").or(predicate::str::contains("Invalid")));

    assert!(!victim_path.exists());
}

#[test]
fn harvest_rejects_short_option_like_branch() {
    let temp = tempdir().unwrap();
    init_repo(temp.path());

    let config_path = write_harvest_config(temp.path());

    let mut cmd = inkentry_bin();
    cmd.current_dir(temp.path())
        .env_remove("INKENTRY_SERVER_URL")
        .env_remove("INKENTRY_LLM_URL")
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("harvest")
        .arg("--branch=-1");

    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("rejected").or(predicate::str::contains("Invalid")));
}

#[test]
fn harvest_rejects_bare_double_dash_branch() {
    let temp = tempdir().unwrap();
    init_repo(temp.path());

    let config_path = write_harvest_config(temp.path());

    let mut cmd = inkentry_bin();
    cmd.current_dir(temp.path())
        .env_remove("INKENTRY_SERVER_URL")
        .env_remove("INKENTRY_LLM_URL")
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("harvest")
        .arg("--branch=--");

    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("rejected").or(predicate::str::contains("Invalid")));
}

// No shell is involved; this pins that metacharacters offer no bypass of the leading-`-` guard.
#[test]
fn harvest_rejects_option_like_branch_with_shell_metacharacters() {
    let temp = tempdir().unwrap();
    init_repo(temp.path());

    let victim_dir = tempdir().unwrap();
    let victim_path = victim_dir.path().join("victim3.txt");

    let config_path = write_harvest_config(temp.path());

    let malicious_branch_arg = format!(
        "--branch=--output={};touch /tmp/oss61-pwned",
        victim_path.display()
    );

    let mut cmd = inkentry_bin();
    cmd.current_dir(temp.path())
        .env_remove("INKENTRY_SERVER_URL")
        .env_remove("INKENTRY_LLM_URL")
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("harvest")
        .arg(&malicious_branch_arg);

    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("rejected").or(predicate::str::contains("Invalid")));

    assert!(!victim_path.exists());
    assert!(!std::path::Path::new("/tmp/oss61-pwned").exists());
}
