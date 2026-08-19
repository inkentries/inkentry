// A loopback responder started by another user must say so on stderr.
//
// `/v1/health`'s `started_by` is the only thing that separates "my daemon"
// from "another account's daemon on this host", and it reached the user
// through `tracing::warn!` alone, which is off at the default log level: the
// CLI picked that server up silently. The warning has to arrive on stderr,
// where a default `inkentry` run shows it.
//
// Unix only: the check compares against this process's effective UID, and the
// CLI reports no UID at all on Windows.

#![cfg(unix)]

use crate::plumbing_helpers;
use plumbing_helpers::{init_git_repo, inkentry_bin_in};

use std::path::Path;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn current_uid() -> u32 {
    let out = std::process::Command::new("id")
        .arg("-u")
        .output()
        .expect("run `id -u`");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("`id -u` prints a number")
}

fn run(home: &Path, state: &Path, project: &Path, discovery_port: &str, arg: &str) -> Vec<u8> {
    let out = inkentry_bin_in(home)
        .current_dir(project)
        .env("INKENTRY_STATE_DIR", state)
        .env("INKENTRY_TEST_DISCOVERY_PORT", discovery_port)
        .env_remove("INKENTRY_SERVER_URL")
        .env_remove("INKENTRY_NO_SERVER")
        .arg(arg)
        .output()
        .expect("run the inkentry binary");
    out.stderr
}

// No state file at all (a machine that never ran `inkentry server start`), so
// discovery reaches the fixed-port fallback, pointed here at the mock. That
// fallback cannot check a PID or an instance id it was never given, which is
// why the UID it *is* given has to be surfaced.
#[tokio::test]
async fn a_foreign_uid_on_the_discovered_port_reaches_stderr() {
    let mine = current_uid();
    let theirs = if mine == 0 { 1 } else { 0 };

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "version": "1.0.0",
            "capabilities": ["memory"],
            "instance_id": "00000000-0000-0000-0000-000000000001",
            "started_by": theirs,
            "embedding_dim": 0
        })))
        // Verified when the mock server drops: a test that never reached the
        // probe would otherwise look the same as one whose warning is missing.
        .expect(1..)
        .mount(&server)
        .await;
    let port = server.address().port().to_string();

    let home = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();

    let stderr = tokio::task::spawn_blocking({
        let home = home.path().to_path_buf();
        let state = state.path().to_path_buf();
        let project = project.path().to_path_buf();
        move || {
            init_git_repo(&project);
            // `status` needs a project before it gets as far as probing, and
            // `init` must not reach the mock while making one.
            run(&home, &state, &project, "0", "init");
            run(&home, &state, &project, &port, "status")
        }
    })
    .await
    .expect("join the spawned commands");

    let stderr = String::from_utf8_lossy(&stderr);
    assert!(
        stderr.contains(&format!("UID {theirs}")) && stderr.contains(&format!("UID {mine}")),
        "the UID mismatch never reached stderr, so a default run cannot see that \
         the discovered server belongs to another account. stderr was:\n{stderr}"
    );
}
