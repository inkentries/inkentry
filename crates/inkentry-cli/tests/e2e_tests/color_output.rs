use crate::plumbing_helpers;
use plumbing_helpers::{inkentry_bin, write_config};

use assert_cmd::Command;
use tempfile::TempDir;

fn project_with_memory_note() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");
    let mem_path = db_path.with_file_name("memory.db");

    let config_path = write_config(tmp.path(), &db_path, "http://127.0.0.1:1");

    inkentry_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("--db")
        .arg(&mem_path)
        .arg("add")
        .arg("--kind")
        .arg("note")
        .arg("--title")
        .arg("color output test note")
        .arg("--body")
        .arg("body content here")
        .assert()
        .success();

    (tmp, mem_path, config_path)
}

fn memory_list_cmd(mem_path: &std::path::Path, config_path: &std::path::Path) -> Command {
    let mut cmd = inkentry_bin();
    cmd.arg("--config")
        .arg(config_path)
        .arg("memory")
        .arg("--db")
        .arg(mem_path)
        .arg("list");
    cmd
}

fn assert_no_ansi(stdout: &[u8]) {
    assert!(
        !stdout.contains(&0x1b),
        "expected no ANSI escape bytes in non-tty stdout, got: {:?}",
        String::from_utf8_lossy(stdout)
    );
}

fn assert_has_ansi(stdout: &[u8]) {
    assert!(
        stdout.contains(&0x1b),
        "expected ANSI escape bytes (forced via --color=always), got: {:?}",
        String::from_utf8_lossy(stdout)
    );
}

#[test]
fn memory_list_default_has_no_ansi_on_non_tty_stdout() {
    let (_tmp, mem_path, config_path) = project_with_memory_note();
    let out = memory_list_cmd(&mem_path, &config_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_no_ansi(&out);
}

#[test]
fn no_color_env_suppresses_color() {
    let (_tmp, mem_path, config_path) = project_with_memory_note();
    let out = memory_list_cmd(&mem_path, &config_path)
        .env("NO_COLOR", "1")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_no_ansi(&out);
}

#[test]
fn color_always_flag_overrides_non_tty_default() {
    let (_tmp, mem_path, config_path) = project_with_memory_note();
    let mut cmd = inkentry_bin();
    let out = cmd
        .arg("--color")
        .arg("always")
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("--db")
        .arg(&mem_path)
        .arg("list")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_has_ansi(&out);
}

#[test]
fn color_always_flag_overrides_no_color_env() {
    let (_tmp, mem_path, config_path) = project_with_memory_note();
    let mut cmd = inkentry_bin();
    let out = cmd
        .arg("--color")
        .arg("always")
        .env("NO_COLOR", "1")
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("--db")
        .arg(&mem_path)
        .arg("list")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_has_ansi(&out);
}

#[test]
fn color_never_flag_suppresses_color() {
    let (_tmp, mem_path, config_path) = project_with_memory_note();
    let mut cmd = inkentry_bin();
    let out = cmd
        .arg("--color")
        .arg("never")
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("--db")
        .arg(&mem_path)
        .arg("list")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_no_ansi(&out);
}

fn indexed_search_project() -> TempDir {
    let tmp = TempDir::new().unwrap();
    std::fs::write(
        tmp.path().join("a.rs"),
        "fn helper_fn() {}\nfn caller() { helper_fn(); }\n",
    )
    .unwrap();
    inkentry_bin()
        .current_dir(tmp.path())
        .env("INKENTRY_NO_SERVER", "1")
        .args(["index", "."])
        .assert()
        .success();
    tmp
}

#[test]
fn search_default_has_no_ansi_on_non_tty_stdout() {
    let tmp = indexed_search_project();
    let out = inkentry_bin()
        .current_dir(tmp.path())
        .env("INKENTRY_NO_SERVER", "1")
        .args(["search", "helper_fn", "--only-text", "--no-stale-check"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_no_ansi(&out);
}

#[test]
fn search_color_always_has_ansi() {
    let tmp = indexed_search_project();
    let out = inkentry_bin()
        .current_dir(tmp.path())
        .env("INKENTRY_NO_SERVER", "1")
        .arg("--color")
        .arg("always")
        .args(["search", "helper_fn", "--only-text", "--no-stale-check"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_has_ansi(&out);
}

fn context_project_with_decision() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");
    let mem_path = db_path.with_file_name("memory.db");
    let config_path = write_config(tmp.path(), &db_path, "http://127.0.0.1:1");

    inkentry_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&config_path)
        .arg("memory")
        .arg("--db")
        .arg(&mem_path)
        .arg("add")
        .arg("--kind")
        .arg("decision")
        .arg("--title")
        .arg("color output test decision")
        .arg("--body")
        .arg("why we made this call")
        .assert()
        .success();

    (tmp, mem_path, config_path)
}

// Keeps `context` on the plain memory-list path: no index DB, cross-project
// lookup or embedding call.
fn context_cmd(mem_path: &std::path::Path, config_path: &std::path::Path) -> Command {
    let mut cmd = inkentry_bin();
    cmd.arg("--config")
        .arg(config_path)
        .arg("context")
        .arg("--db")
        .arg(mem_path)
        .arg("--no-conventions")
        .arg("--local-only");
    cmd
}

#[test]
fn context_section_header_has_no_ansi_on_non_tty_stdout() {
    let (_tmp, mem_path, config_path) = context_project_with_decision();
    let out = context_cmd(&mem_path, &config_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_no_ansi(&out);
}

#[test]
fn context_section_header_no_color_env_suppresses_color() {
    let (_tmp, mem_path, config_path) = context_project_with_decision();
    let out = context_cmd(&mem_path, &config_path)
        .env("NO_COLOR", "1")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_no_ansi(&out);
}

#[test]
fn context_section_header_color_always_has_ansi() {
    let (_tmp, mem_path, config_path) = context_project_with_decision();
    let mut cmd = context_cmd(&mem_path, &config_path);
    let out = cmd
        .arg("--color")
        .arg("always")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_has_ansi(&out);
}
