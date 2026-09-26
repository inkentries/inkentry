use crate::plumbing_helpers;
use plumbing_helpers::inkentry_bin;

use std::path::{Path, PathBuf};
use tempfile::TempDir;

// Must never appear on stdout when reads route to the server.
const LOCAL_TITLE: &str = "local only entry";

fn write_cfg(dir: &Path, name: &str, db_path: &Path, extra: &str) -> PathBuf {
    let cfg = format!(
        "db_path = {:?}\napi_base_url = \"http://127.0.0.1:1\"\n\
         llm_model = \"test-chat\"\n{extra}",
        db_path
    );
    let path = dir.join(name);
    std::fs::write(&path, cfg).expect("write config");
    path
}

fn seeded_project() -> (TempDir, PathBuf, String) {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("inkentry.db");
    let mem_path = db_path.with_file_name("memory.db");
    let cfg = write_cfg(tmp.path(), "config-seed.toml", &db_path, "");
    let out = inkentry_bin()
        // Not a git repo, so the entry lands only in memory.db.
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&cfg)
        .args(["memory", "--db"])
        .arg(&mem_path)
        .args([
            "add",
            "--kind",
            "note",
            "--title",
            LOCAL_TITLE,
            "--body",
            "b",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let id = stdout
        .lines()
        .find_map(|l| l.trim_start().strip_prefix("id:"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| panic!("could not parse stored id from: {stdout}"));
    assert!(
        uuid::Uuid::parse_str(&id).is_ok(),
        "stored id must be a UUID, got {id:?}"
    );
    (tmp, mem_path, id)
}

fn memory_list(tmp: &TempDir, mem_path: &Path, cfg: &Path) -> std::process::Output {
    inkentry_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(cfg)
        .args(["memory", "--db"])
        .arg(mem_path)
        .args(["list", "--format", "json"])
        .output()
        .unwrap()
}

fn assert_no_sync_nag(stderr: &str) {
    assert!(
        !stderr.contains("inkentry sync"),
        "read must not nag about manual sync: {stderr}"
    );
    assert!(
        !stderr.contains("showing local data"),
        "read must not label local data on stderr: {stderr}"
    );
}

#[test]
fn local_first_read_serves_data_without_sync_nag() {
    let (tmp, mem_path, _id) = seeded_project();
    // The host is unresolvable on purpose: local_first must never contact it.
    let cfg = write_cfg(
        tmp.path(),
        "config-local-first.toml",
        &tmp.path().join("inkentry.db"),
        "",
    );
    // `server_url`/`project_id` only apply from the project-level `.inkentry/config.toml`.
    plumbing_helpers::write_project_server_config(
        tmp.path(),
        "https://team.invalid:4655",
        "team/proj",
    );

    let out = memory_list(&tmp, &mem_path, &cfg);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(out.status.success(), "expected exit 0; stderr: {stderr}");
    assert_no_sync_nag(&stderr);
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout must be pure JSON");
    assert!(
        parsed.as_array().is_some_and(|a| !a.is_empty()),
        "expected the seeded entry on stdout: {stdout}"
    );
    assert!(stdout.contains(LOCAL_TITLE), "got: {stdout}");
}

#[test]
fn read_commands_never_print_pending_or_last_synced_banner() {
    let (tmp, mem_path, id) = seeded_project();
    let cfg = write_cfg(
        tmp.path(),
        "config-read-commands.toml",
        &tmp.path().join("inkentry.db"),
        "",
    );
    plumbing_helpers::write_project_server_config(
        tmp.path(),
        "https://team.invalid:4655",
        "team/proj",
    );

    let assert_clean = |out: &std::process::Output, label: &str| {
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        for needle in ["pending", "last synced", "sync error", "inkentry sync"] {
            assert!(
                !stdout.contains(needle) && !stderr.contains(needle),
                "{label} must never mention {needle:?} (status-only content): \
                 stdout={stdout} stderr={stderr}"
            );
        }
    };

    let list = memory_list(&tmp, &mem_path, &cfg);
    assert_clean(&list, "memory list");

    let show = inkentry_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&cfg)
        .args(["memory", "--db"])
        .arg(&mem_path)
        .args(["show", &id])
        .output()
        .unwrap();
    assert_clean(&show, "memory show");

    let timeline = inkentry_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&cfg)
        .args(["memory", "--db"])
        .arg(&mem_path)
        .args(["timeline", LOCAL_TITLE])
        .output()
        .unwrap();
    assert_clean(&timeline, "memory timeline");

    let context = inkentry_bin()
        .current_dir(tmp.path())
        .arg("--config")
        .arg(&cfg)
        .args(["context", "--db"])
        .arg(&mem_path)
        .output()
        .unwrap();
    assert_clean(&context, "context");
}

#[test]
fn cloud_first_read_unreachable_server_errors_without_local_data() {
    let (tmp, mem_path, _id) = seeded_project();
    // Nothing listens on port 1, so the read must fail. A raw-UUID project_id skips slug
    // resolution, so the failure is the memory read itself. `mode` is not a project-config
    // field, so it stays in the global file.
    let cfg = write_cfg(
        tmp.path(),
        "config-cloud-first.toml",
        &tmp.path().join("inkentry.db"),
        "mode = \"cloud_first\"\n",
    );
    plumbing_helpers::write_project_server_config(
        tmp.path(),
        "http://127.0.0.1:1",
        "11111111-1111-1111-1111-111111111111",
    );

    let out = memory_list(&tmp, &mem_path, &cfg);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        !out.status.success(),
        "cloud_first read against an unreachable server must exit non-zero; \
         stdout: {stdout}"
    );
    assert!(
        !stdout.contains(LOCAL_TITLE),
        "local data must never be printed when reads route to the server: {stdout}"
    );
    assert!(
        stderr.contains("GET /memory"),
        "error must name the failed server read: {stderr}"
    );
    assert!(
        stderr.contains("Caused by"),
        "error must carry the cause chain: {stderr}"
    );
}

fn indexed_project() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("lib.rs"), "pub fn hello() {}").unwrap();
    let db_path = tmp.path().join("index.db");
    let cfg = write_cfg(tmp.path(), "config-index.toml", &db_path, "");
    inkentry_bin()
        .env("INKENTRY_NO_SERVER", "1")
        .arg("--config")
        .arg(&cfg)
        .arg("index")
        .arg(&project)
        .assert()
        .success();
    (tmp, project)
}

#[test]
fn status_shows_neutral_mode_and_truthful_hints_with_unreachable_server_url() {
    let (tmp, project) = indexed_project();
    // Nothing listens on port 1, so the tier probe fails: Offline with server_url set.
    let cfg = write_cfg(
        tmp.path(),
        "config-team.toml",
        &tmp.path().join("index.db"),
        "",
    );
    // `server_url`/`project_id` only apply from the project-level config, which must sit
    // under `project` (the cwd of `status`), not `tmp.path()`.
    plumbing_helpers::write_project_server_config(&project, "https://127.0.0.1:1", "team/proj");

    let out = inkentry_bin()
        .current_dir(&project)
        .arg("--config")
        .arg(&cfg)
        .arg("status")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout: {stdout}");

    assert!(stdout.contains("mode"), "got: {stdout}");
    assert!(stdout.contains("local_first"), "got: {stdout}");
    assert!(
        !stdout.contains("inkentry sync"),
        "status must not pre-teach a manual sync workflow: {stdout}"
    );
}

#[test]
fn status_has_no_mode_line_on_solo_default() {
    let (tmp, project) = indexed_project();
    let cfg = write_cfg(
        tmp.path(),
        "config-solo-status.toml",
        &tmp.path().join("index.db"),
        "",
    );

    let out = inkentry_bin()
        .env("INKENTRY_NO_SERVER", "1") // hermetic: no loopback auto-discovery
        .current_dir(&project)
        .arg("--config")
        .arg(&cfg)
        .arg("status")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stdout: {stdout}");

    assert!(!stdout.contains("\n  mode"), "got: {stdout}");
    assert!(!stdout.contains("local_first"), "got: {stdout}");
    // The kill-switch makes this run offline, so the search hint names it, not the inert `server_url`.
    assert!(stdout.contains("INKENTRY_NO_SERVER"), "got: {stdout}");
    assert!(!stdout.contains("server_url"), "got: {stdout}");
}
