// End-to-end coverage for the inference-server-required guidance.
//
// The bug: an inference-only command told a solo user with no server to set a
// team `server_url` when all they needed was `inkentry server start`. The fix
// routes the caller's effective `server_url` (which is `None` for a
// solo/no-server user, never the auto-discovered loopback URL) into
// `capability::inference_server_required_message`, whose no-`server_url` branch
// must point at the local server and must NOT mention `server_url`.
//
// `plumbing embed` is the raw embedding primitive and still hard-requires a
// server, so it is the durable end-to-end exercise of this message. (The former
// `memory search --mode semantic|hybrid` no longer gates here: unified `search`
// degrades to full-text search when no embedder is reachable rather than
// erroring, so it is not a server-required caller anymore.)
//
// The engineer's unit tests pin the pure function and `embed_text` body
// surfacing. This test closes the highest-value gap: a real CLI invocation, so
// a caller that passed the wrong argument (e.g. the loopback inference URL, or
// a hard-coded `None` that suppresses a legitimately-configured URL) would be
// caught: a pure-fn test cannot see the wiring.

mod plumbing_helpers;
use plumbing_helpers::inkentry_bin;

use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

/// The substring that reintroducing the bug would add back to the message. Its
/// ABSENCE is the core regression guard: the no-server message must never tell a
/// solo user to configure `server_url`.
const REGRESSION_SUBSTR: &str = "server_url";

/// Write a minimal config with NO `server_url` (solo / no-server user).
fn write_no_server_config(dir: &Path) -> PathBuf {
    let db_path = dir.join("index.db");
    let config_path = dir.join("config.toml");
    fs::write(&config_path, format!("db_path = {db_path:?}\n")).expect("write config.toml");
    config_path
}

/// Shared assertion: the no-server message points at the local server and never
/// mentions `server_url`.
fn assert_local_start_no_server_url(stderr: &str) {
    assert!(
        stderr.contains("requires inkentry-server"),
        "must state the feature requires the server; got: {stderr}"
    );
    assert!(
        stderr.contains("inkentry server start"),
        "must point at the local auto-server; got: {stderr}"
    );
    assert!(
        !stderr.contains(REGRESSION_SUBSTR),
        "no-server message must NOT mention `server_url`; got: {stderr}"
    );
}

// `plumbing embed` is the low-level embedding path (a `require_server_client`
// caller). It reads stdin; with no server the gate fires before any line is read.
#[test]
fn plumbing_embed_no_server_points_at_local_start() {
    let temp = tempdir().unwrap();
    let config_path = write_no_server_config(temp.path());
    let db = temp.path().join("index.db");

    let assert = inkentry_bin()
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(temp.path())
        .arg("--config")
        .arg(&config_path)
        .arg("plumbing")
        .arg("--db")
        .arg(&db)
        .arg("embed")
        .write_stdin("some text\n")
        .assert()
        .failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert_local_start_no_server_url(&stderr);
    assert!(
        stderr.contains("plumbing embed"),
        "message must name the invoked feature; got: {stderr}"
    );
}
