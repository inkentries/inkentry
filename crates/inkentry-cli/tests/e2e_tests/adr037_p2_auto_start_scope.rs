use crate::plumbing_helpers;
use plumbing_helpers::{init_git_repo, inkentry_bin_in, write_project_server_config};

use std::path::Path;
use tempfile::TempDir;

fn state_dir_under(home: &Path) -> std::path::PathBuf {
    home.join(".local").join("state").join("inkentry")
}

// `ensure_server_running` creates the state dir before anything else, so its absence
// proves auto-start was never attempted.
fn assert_write_never_auto_starts(mode_toml: &str) {
    let home = TempDir::new().unwrap().keep();
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let mem_path = project.join("memory.db");
    let config_path = home.join("config.toml");
    std::fs::write(&config_path, "").unwrap();

    write_project_server_config(&project, "https://team.invalid:4655", "team/proj");
    if !mode_toml.is_empty() {
        let cfg_path = project.join(".inkentry").join("config.toml");
        let mut existing = std::fs::read_to_string(&cfg_path).unwrap();
        existing.push_str(mode_toml);
        std::fs::write(&cfg_path, existing).unwrap();
    }

    assert!(
        !state_dir_under(&home).exists(),
        "precondition: state dir must not exist before the write"
    );

    let out = inkentry_bin_in(&home)
        .current_dir(&project)
        .arg("--config")
        .arg(&config_path)
        .args(["memory", "--db"])
        .arg(&mem_path)
        .args(["add", "--kind", "note", "--title", "T", "--body", "b"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        !state_dir_under(&home).exists(),
        "a non-interactive write must never auto-start the local server \
         (state dir would exist if it had): {}",
        state_dir_under(&home).display()
    );
}

#[test]
fn non_interactive_local_first_write_never_auto_starts() {
    assert_write_never_auto_starts("");
}

#[test]
fn offline_mode_write_never_auto_starts() {
    assert_write_never_auto_starts("mode = \"offline\"\n");
}

#[test]
fn cloud_first_mode_write_never_auto_starts() {
    let home = TempDir::new().unwrap().keep();
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let mem_path = project.join("memory.db");
    let config_path = home.join("config.toml");
    std::fs::write(&config_path, "").unwrap();

    // Loopback http passes the transport guard, so the write reaches the wire instead of
    // failing config validation.
    write_project_server_config(&project, "http://127.0.0.1:1", "team/proj");
    let cfg_path = project.join(".inkentry").join("config.toml");
    let mut existing = std::fs::read_to_string(&cfg_path).unwrap();
    existing.push_str("mode = \"cloud_first\"\n");
    std::fs::write(&cfg_path, existing).unwrap();

    // The write is expected to fail (unreachable server, no local fallback); only auto-start matters.
    let _ = inkentry_bin_in(&home)
        .current_dir(&project)
        .arg("--config")
        .arg(&config_path)
        .args(["memory", "--db"])
        .arg(&mem_path)
        .args(["add", "--kind", "note", "--title", "T", "--body", "b"])
        .output()
        .unwrap();

    assert!(
        !state_dir_under(&home).exists(),
        "cloud_first must never trigger the local_first-only auto-start path"
    );
}

#[test]
fn inkentry_no_server_env_write_never_auto_starts() {
    let home = TempDir::new().unwrap().keep();
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let mem_path = project.join("memory.db");
    let config_path = home.join("config.toml");
    std::fs::write(&config_path, "").unwrap();
    write_project_server_config(&project, "https://team.invalid:4655", "team/proj");

    let out = inkentry_bin_in(&home)
        .env("INKENTRY_NO_SERVER", "1")
        .current_dir(&project)
        .arg("--config")
        .arg(&config_path)
        .args(["memory", "--db"])
        .arg(&mem_path)
        .args(["add", "--kind", "note", "--title", "T", "--body", "b"])
        .output()
        .unwrap();
    assert!(out.status.success());

    assert!(
        !state_dir_under(&home).exists(),
        "INKENTRY_NO_SERVER=1 must never auto-start, matching the existing hard kill-switch"
    );
}

// Only sync endpoints are asserted: `memory add` legitimately calls `/v1/health` and
// `/index/embed` for inference routing.
#[tokio::test]
async fn write_never_makes_a_sync_call_to_server_url_even_when_it_is_reachable() {
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let team_server = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok", "version": "test", "capabilities": ["memory"]
        })))
        .mount(&team_server)
        .await;

    let home = TempDir::new().unwrap().keep();
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let mem_path = project.join("memory.db");
    let config_path = home.join("config.toml");
    std::fs::write(&config_path, "").unwrap();
    write_project_server_config(&project, &team_server.uri(), "team/proj");

    let out = inkentry_bin_in(&home)
        .current_dir(&project)
        .arg("--config")
        .arg(&config_path)
        .args(["memory", "--db"])
        .arg(&mem_path)
        .args(["add", "--kind", "note", "--title", "T", "--body", "b"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let received = team_server.received_requests().await.unwrap();
    let sync_reqs: Vec<_> = received
        .iter()
        .filter(|r| {
            r.url.path().contains("/memory/batch") || r.url.path().contains("/memory/since")
        })
        .collect();
    assert!(
        sync_reqs.is_empty(),
        "the write's own call stack must never reach the team server's sync \
         endpoints directly (no local relay was running to hand off to): {:?}",
        received.iter().map(|r| r.url.path()).collect::<Vec<_>>()
    );
}

// Needs a running relay: without one `probe_local_relay_port` returns None and the nudge
// no-ops regardless of the gate, so the test would pass vacuously.
// `multi_thread`: the test blocks on a synchronous `Command::output()`, so the in-process
// relay needs its own worker thread or the CLI's health probe starves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_git_notes_backend_pre_init_never_creates_a_phantom_memory_db() {
    use std::sync::Arc;

    #[allow(clippy::missing_transmute_annotations)]
    unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    }

    let home = TempDir::new().unwrap().keep();
    let repo = home.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);

    let db_dir = TempDir::new().unwrap();
    let db = inkentry_server::db::ServerDb::open(&db_dir.path().join("server.db"), 4, "test-model")
        .unwrap();
    let instance_id = db.get_or_create_instance_id().unwrap();
    let state = inkentry_server::AppState {
        db: Arc::new(tokio::sync::Mutex::new(db)),
        auth: Arc::new(inkentry_server::auth::ApiKeyAuth::new(None)),
        conflict_threshold: inkentry_server::default_conflict_threshold(),
        embedder: inkentry_server::EmbedderSlot::disabled(),
        embed_admission: inkentry_server::EmbedAdmission::new(
            inkentry_server::EMBED_QUEUE_CAPACITY,
            inkentry_server::EMBED_INTERACTIVE_CAPACITY_HIGH,
            inkentry_server::EMBED_BUSY_RETRY_AFTER_SECS,
        ),
        embed_threads: 4,
        llm: None,
        max_tokens_ceiling: 8192,
        rate_limiter: Arc::new(inkentry_server::rate_limiter::RateLimiter::new(1000, 60)),
        instance_id: instance_id.clone(),
        started_by: None,
        trusted_proxies: Default::default(),
        relay: inkentry_server::relay::RelayRegistry::disabled(),
        repair_signal: inkentry_server::repair::RepairSignal::new(),
    };
    let app = inkentry_server::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay_port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let state_dir = home.join(".local").join("state").join("inkentry");
    std::fs::create_dir_all(&state_dir).unwrap();
    std::fs::write(state_dir.join("server.port"), format!("{relay_port}\n")).unwrap();
    // The recorded responder is trusted only if its pid and instance_id match; the relay is
    // in-process, so record a fake pid trusted via the test seam (instance_id is still
    // checked for real). Otherwise the child refuses the relay and the gate is never reached.
    std::fs::write(state_dir.join("server.pid"), "99999\n").unwrap();
    std::fs::write(
        state_dir.join("server.instance_id"),
        format!("{instance_id}\n"),
    )
    .unwrap();

    let out = inkentry_bin_in(&home)
        .current_dir(&repo)
        .env("INKENTRY_STATE_DIR", &state_dir)
        .env("INKENTRY_TEST_TRUST_RECORDED_RESPONDER", "1")
        .env("INKENTRY_SERVER_URL", "https://team.invalid:4655")
        .env("INKENTRY_PROJECT_ID", "team/proj")
        .env("INKENTRY_MODE", "local_first")
        .args([
            "memory",
            "add",
            "--backend",
            "git-notes",
            "--kind",
            "note",
            "--title",
            "T",
            "--body",
            "b",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // Give the fire-and-forget nudge time to reach `MemoryStore::open` if the gate is broken.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    assert!(
        !repo.join(".inkentry").exists(),
        "explicit --backend git-notes with no local project must never create \
         a .inkentry/ project as a side effect of the post-write relay nudge"
    );
    // The placeholder `mem_path` resolves to the global default under the config dir,
    // not under `repo`.
    assert!(
        !home
            .join(".config")
            .join("inkentry")
            .join("memory.db")
            .exists(),
        "must never create a phantom global memory.db as a side effect of an \
         explicit --backend git-notes write with no local project"
    );
}

#[test]
fn write_still_commits_and_stays_outbox_pending_when_no_auto_start_happens() {
    let home = TempDir::new().unwrap().keep();
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let mem_path = project.join("memory.db");
    let config_path = home.join("config.toml");
    std::fs::write(&config_path, "").unwrap();
    write_project_server_config(&project, "https://team.invalid:4655", "team/proj");

    let out = inkentry_bin_in(&home)
        .current_dir(&project)
        .arg("--config")
        .arg(&config_path)
        .args(["memory", "--db"])
        .arg(&mem_path)
        .args(["add", "--kind", "note", "--title", "T", "--body", "b"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("Stored"),
        "the write must commit locally regardless of relay reachability"
    );

    let out = inkentry_bin_in(&home)
        .current_dir(&project)
        .arg("--config")
        .arg(&config_path)
        .args(["memory", "--db"])
        .arg(&mem_path)
        .args(["list", "--format", "json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let parsed: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert!(parsed.as_array().is_some_and(|a| a.len() == 1));
}

// `memory list` stands in for every read command: they share `outbox::poll_and_apply`,
// which must only poll an already-running relay, never `ensure_server_running`.
#[test]
fn memory_list_never_auto_starts_the_local_server() {
    let home = TempDir::new().unwrap().keep();
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let mem_path = project.join("memory.db");
    let config_path = home.join("config.toml");
    std::fs::write(&config_path, "").unwrap();
    write_project_server_config(&project, "https://team.invalid:4655", "team/proj");

    assert!(
        !state_dir_under(&home).exists(),
        "precondition: state dir must not exist before the read"
    );

    let out = inkentry_bin_in(&home)
        .current_dir(&project)
        .arg("--config")
        .arg(&config_path)
        .args(["memory", "--db"])
        .arg(&mem_path)
        .args(["list", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        !state_dir_under(&home).exists(),
        "`memory list`'s relay poll (items 42-47) must never auto-start the local \
         server — it may only poll one that is already running: {}",
        state_dir_under(&home).display()
    );
}
