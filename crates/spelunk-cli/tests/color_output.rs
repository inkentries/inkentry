//! Regression coverage for the "ANSI color leaks onto piped/non-tty stdout,
//! and NO_COLOR is ignored" bug.
//!
//! `spelunk memory list` (default text format) is the lightweight target here
//! (no index or server needed, see `memory_list_format.rs`), but the fix
//! lives in a shared helper so this doubles as coverage for every text-mode
//! command that prints `\x1b[...m` escapes.

mod plumbing_helpers;
use plumbing_helpers::{spelunk_bin, write_config};

use assert_cmd::Command;
use tempfile::TempDir;

/// Create a temp project with a single memory note and return
/// `(TempDir, mem_path, config_path)`. The `TempDir` must be kept alive for
/// the duration of the test.
fn project_with_memory_note() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("spelunk.db");
    let mem_path = db_path.with_file_name("memory.db");

    let config_path = write_config(tmp.path(), &db_path, "http://127.0.0.1:1");

    spelunk_bin()
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
    let mut cmd = spelunk_bin();
    cmd.arg("--config")
        .arg(config_path)
        .arg("memory")
        .arg("--db")
        .arg(mem_path)
        .arg("list");
    cmd
}

/// `assert_cmd::Command` always captures stdout through a pipe, so the child
/// process's stdout is never a tty. That's exactly the "piped" case in the
/// bug report: the raw `\x1b` (0x1b) control byte must never appear in
/// output that isn't going to a terminal.
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

// ── (a) non-tty stdout defaults to no color ─────────────────────────────────

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

// ── (b) NO_COLOR forces color off regardless of tty state ──────────────────

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

// ── (c) --color=always overrides both the non-tty default and NO_COLOR ─────

#[test]
fn color_always_flag_overrides_non_tty_default() {
    let (_tmp, mem_path, config_path) = project_with_memory_note();
    let mut cmd = spelunk_bin();
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
    let mut cmd = spelunk_bin();
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

// ── --color=never is an explicit, unconditional off-switch ─────────────────

#[test]
fn color_never_flag_suppresses_color() {
    let (_tmp, mem_path, config_path) = project_with_memory_note();
    let mut cmd = spelunk_bin();
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
