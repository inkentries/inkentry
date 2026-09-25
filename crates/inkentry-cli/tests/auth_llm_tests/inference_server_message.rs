// A no-server user must be pointed at `inkentry server start`, never told to
// set `server_url`; a real invocation catches a caller passing the wrong URL,
// which a pure-function test cannot.

use crate::plumbing_helpers;
use plumbing_helpers::inkentry_bin;

use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

// Its absence from the no-server message is the regression guard.
const REGRESSION_SUBSTR: &str = "server_url";

fn write_no_server_config(dir: &Path) -> PathBuf {
    let db_path = dir.join("index.db");
    let config_path = dir.join("config.toml");
    fs::write(&config_path, format!("db_path = {db_path:?}\n")).expect("write config.toml");
    config_path
}

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

// A `require_server_client` caller; with no server the gate fires before any
// stdin line is read.
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
