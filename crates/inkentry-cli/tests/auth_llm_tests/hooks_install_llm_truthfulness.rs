// `inkentry hooks install` promises per-commit harvesting, and harvest is the
// only feature that needs an LLM. A default install has none, so the promise
// has to be qualified at the moment the user opts in.

use crate::command_llm_routing::{
    base_cmd, combined, harvest_payload, loopback_discovery_port, seed_index, server_mock,
    write_git_project,
};
use crate::plumbing_helpers::init_local_project;
use std::path::Path;
use tempfile::TempDir;

fn install_cmd(
    home: &Path,
    project: &Path,
    state_dir: &Path,
    discovery_port: &str,
) -> assert_cmd::Command {
    let mut cmd = base_cmd(home, project);
    cmd.env("INKENTRY_STATE_DIR", state_dir)
        .env("INKENTRY_TEST_DISCOVERY_PORT", discovery_port)
        .arg("hooks")
        .arg("install");
    cmd
}

fn post_commit_hook(project: &Path) -> std::path::PathBuf {
    project.join(".git").join("hooks").join("post-commit")
}

// The top-level harvest the installed hook runs, detached exactly as it is
// there.
fn detached_harvest_cmd(
    home: &Path,
    project: &Path,
    state_dir: &Path,
    discovery_port: &str,
) -> assert_cmd::Command {
    let mut cmd = base_cmd(home, project);
    cmd.env("INKENTRY_STATE_DIR", state_dir)
        .env("INKENTRY_TEST_DISCOVERY_PORT", discovery_port)
        .arg("harvest")
        .arg("--git-range")
        .arg("HEAD~1..HEAD")
        .arg("--detach");
    cmd
}

// The detached child outlives its parent, so the artifact appears after the
// command has already returned.
fn wait_for_log(path: &Path) -> String {
    for _ in 0..200 {
        if let Ok(body) = std::fs::read_to_string(path) {
            let lines = body.lines().filter(|l| !l.trim().is_empty()).count();
            if lines > 1 {
                return body;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("no background log appeared at {}", path.display());
}

#[tokio::test]
async fn a_detached_harvest_failure_lands_in_the_background_log() {
    let loopback = server_mock(None).await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_git_project(project.path());
    init_local_project(project.path());
    let state_dir = home.path().join("state");
    let discovery_port = loopback_discovery_port(&state_dir, &loopback.uri());

    let output = detached_harvest_cmd(home.path(), project.path(), &state_dir, &discovery_port)
        .output()
        .expect("run harvest --detach");
    let text = combined(&output);

    assert!(
        output.status.success(),
        "detaching itself must still succeed: {text}"
    );
    assert!(
        text.trim().is_empty(),
        "a detached run says nothing to the caller, which is the whole problem: {text}"
    );

    let log = project.path().join(".inkentry").join("background.log");
    let body = wait_for_log(&log);
    assert!(
        body.contains("harvest --git-range HEAD~1..HEAD"),
        "the log must name the run it reports on: {body}"
    );
    assert!(
        body.contains("no LLM is available"),
        "the failure the user never saw must be here: {body}"
    );
}

#[tokio::test]
async fn configuring_an_llm_after_install_needs_no_reinstall() {
    let dark = server_mock(None).await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_git_project(project.path());
    let db = project.path().join("index.db");
    seed_index(home.path(), project.path(), &db);
    let state_dir = home.path().join("state");
    let discovery_port = loopback_discovery_port(&state_dir, &dark.uri());

    install_cmd(home.path(), project.path(), &state_dir, &discovery_port)
        .assert()
        .success();
    let installed = std::fs::read(post_commit_hook(project.path())).expect("hook installed");

    let lit = server_mock(Some(harvest_payload())).await;
    let discovery_port = loopback_discovery_port(&state_dir, &lit.uri());

    let mut cmd = base_cmd(home.path(), project.path());
    let output = cmd
        .env("INKENTRY_STATE_DIR", &state_dir)
        .env("INKENTRY_TEST_DISCOVERY_PORT", &discovery_port)
        .arg("harvest")
        .arg("--db")
        .arg(project.path().join("memory.db"))
        .arg("--branch")
        .arg("HEAD")
        .output()
        .expect("run harvest");
    let text = combined(&output);

    assert!(
        output.status.success(),
        "harvesting must work once an LLM is reachable: {text}"
    );
    assert_eq!(
        std::fs::read(post_commit_hook(project.path())).unwrap(),
        installed,
        "nothing about the hook depends on the LLM, so it must not need rewriting"
    );
}

#[tokio::test]
async fn install_without_an_llm_says_harvesting_is_inactive_and_still_installs() {
    let loopback = server_mock(None).await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_git_project(project.path());
    let state_dir = home.path().join("state");
    let discovery_port = loopback_discovery_port(&state_dir, &loopback.uri());

    let output = install_cmd(home.path(), project.path(), &state_dir, &discovery_port)
        .output()
        .expect("run hooks install");
    let text = combined(&output);

    assert!(output.status.success(), "install must not fail: {text}");
    assert!(
        text.contains("Harvesting stays inactive"),
        "the install must qualify the harvest promise:\n{text}"
    );
    assert!(
        text.contains("no LLM is available"),
        "the caveat must carry the shared no-LLM guidance:\n{text}"
    );

    let hook = post_commit_hook(project.path());
    let body = std::fs::read_to_string(&hook).expect("the hook is installed anyway");
    assert!(
        body.contains("inkentry post-commit hook"),
        "the hook must still be ours: {body}"
    );
}

#[tokio::test]
async fn install_with_an_llm_prints_no_caveat() {
    let loopback = server_mock(Some("unused".to_string())).await;

    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_git_project(project.path());
    let state_dir = home.path().join("state");
    let discovery_port = loopback_discovery_port(&state_dir, &loopback.uri());

    let output = install_cmd(home.path(), project.path(), &state_dir, &discovery_port)
        .output()
        .expect("run hooks install");
    let text = combined(&output);

    assert!(output.status.success(), "install must not fail: {text}");
    assert!(
        text.contains("Harvest memory from the new commit"),
        "the harvest promise is still made:\n{text}"
    );
    assert!(
        !text.contains("Harvesting stays inactive"),
        "a reachable LLM must produce no caveat:\n{text}"
    );
}
