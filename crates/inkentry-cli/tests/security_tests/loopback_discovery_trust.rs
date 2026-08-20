// The state-file discovery path (step 3a) works end to end, driven through the
// real binary rather than an in-process `probe_loopback` call.
//
// This is the integration-level counterpart to the unit tests in
// `capability/probe.rs`: it stands up a recorded server (port + pid +
// instance_id) beside a mock daemon and asserts that `inkentry status`
// discovers it and reports it as the auto-discovered loopback server.
//
// Cross-platform, unlike the live-process unit tests: the un-fakeable OS pid
// query is relaxed by `INKENTRY_TEST_TRUST_RECORDED_RESPONDER`, while the
// recorded instance_id is still checked for real against what the mock reports.
// That is the only signal a Windows test cannot otherwise stage, so this is how
// the happy path every command travels gets coverage that does not depend on
// the host OS.

use crate::plumbing_helpers;
use plumbing_helpers::{init_git_repo, inkentry_bin_in};

use std::path::Path;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const RECORDED_INSTANCE_ID: &str = "00000000-0000-0000-0000-0000000000aa";

fn run(home: &Path, state: &Path, project: &Path, args: &[&str], trust_recorded: bool) -> Vec<u8> {
    let mut cmd = inkentry_bin_in(home);
    cmd.current_dir(project)
        .env("INKENTRY_STATE_DIR", state)
        .env_remove("INKENTRY_SERVER_URL")
        .env_remove("INKENTRY_NO_SERVER");
    if trust_recorded {
        cmd.env("INKENTRY_TEST_TRUST_RECORDED_RESPONDER", "1");
    }
    let out = cmd.args(args).output().expect("run the inkentry binary");
    out.stdout
}

// Configured so `relay_target` resolves and the relay gate is actually reached:
// without a team server and a project id, the outbox never probes the recorded
// port at all and this would assert against the inference path's warning.
fn run_stderr(home: &Path, state: &Path, project: &Path, team_url: &str, args: &[&str]) -> String {
    let mut cmd = inkentry_bin_in(home);
    cmd.current_dir(project)
        .env("INKENTRY_STATE_DIR", state)
        .env("INKENTRY_MODE", "local_first")
        .env("INKENTRY_SERVER_URL", team_url)
        .env("INKENTRY_PROJECT_ID", "proj")
        // Past the pid check, so the refusal below is the instance-id mismatch
        // this test is about rather than the fabricated pid.
        .env("INKENTRY_TEST_TRUST_RECORDED_RESPONDER", "1")
        .env_remove("INKENTRY_NO_SERVER");
    let out = cmd.args(args).output().expect("run the inkentry binary");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn write_recorded_daemon(state: &Path, port: u16) {
    std::fs::create_dir_all(state).expect("create the state dir");
    std::fs::write(state.join("server.port"), format!("{port}\n")).expect("write server.port");
    // Any numeric pid: the OS query that would reject it is the one the seam
    // relaxes. The instance_id below is still matched for real.
    std::fs::write(state.join("server.pid"), "99999\n").expect("write server.pid");
    std::fs::write(
        state.join("server.instance_id"),
        format!("{RECORDED_INSTANCE_ID}\n"),
    )
    .expect("write server.instance_id");
}

async fn mock_daemon() -> (MockServer, u16) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok",
            "version": "1.0.0",
            "capabilities": ["memory"],
            "instance_id": RECORDED_INSTANCE_ID,
            "started_by": null,
            "embedding_dim": 0
        })))
        // Verified on drop: a run that never reached the probe would otherwise
        // look the same as one whose discovery silently failed.
        .expect(1..)
        .mount(&server)
        .await;
    let port = server.address().port();
    (server, port)
}

#[tokio::test]
async fn a_recorded_daemon_is_discovered_by_status_end_to_end() {
    let (_server, port) = mock_daemon().await;

    let home = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();

    let stdout = tokio::task::spawn_blocking({
        let home = home.path().to_path_buf();
        let state = state.path().to_path_buf();
        let project = project.path().to_path_buf();
        move || {
            init_git_repo(&project);
            // `init` first, while nothing is recorded, so it never reaches the
            // mock and never records a port of its own.
            run(&home, &state, &project, &["init"], false);
            // Only now record the running daemon, and let `status` discover it.
            write_recorded_daemon(&state, port);
            run(&home, &state, &project, &["status"], true)
        }
    })
    .await
    .expect("join the spawned commands");

    let stdout = String::from_utf8_lossy(&stdout);
    assert!(
        stdout.contains("Server")
            && stdout.contains(&port.to_string())
            && stdout.contains("local, auto"),
        "status did not report the recorded daemon as the auto-discovered \
         loopback server. stdout was:\n{stdout}"
    );
}

// A responder that answers but cannot be verified is the case worth telling the
// user about: a silent `None` here reads as "no daemon" while a process is in
// fact holding the port. The refusal names the failed check and the remedy.
#[tokio::test]
async fn a_refused_relay_responder_says_why_on_stderr() {
    let (_server, port) = mock_daemon().await;

    let home = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();

    let team_url = format!("http://127.0.0.1:{port}");
    let stderr = tokio::task::spawn_blocking({
        let home = home.path().to_path_buf();
        let state = state.path().to_path_buf();
        let project = project.path().to_path_buf();
        move || {
            init_git_repo(&project);
            run(&home, &state, &project, &["init"], false);
            write_recorded_daemon(&state, port);
            // Record an id the mock does not report, so the responder is
            // healthy and reachable but not the daemon that was recorded.
            std::fs::write(
                state.join("server.instance_id"),
                "00000000-0000-0000-0000-0000000000bb\n",
            )
            .expect("overwrite server.instance_id");
            run_stderr(&home, &state, &project, &team_url, &["status"])
        }
    })
    .await
    .expect("join the spawned commands");

    assert!(
        stderr.contains("No memory entries or credentials were sent to it"),
        "the relay refusal must say nothing was sent. stderr was:\n{stderr}"
    );
    assert!(
        stderr.contains("inkentry server stop && inkentry server start"),
        "the relay refusal must name the remedy. stderr was:\n{stderr}"
    );
}
