use std::io::IsTerminal;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::{Config, DEFAULT_SERVER_PORT};
use crate::storage::{MemoryStore, NoteId};

// Short so an absent or wedged local server can't slow a write.
const LOCAL_RELAY_TIMEOUT: Duration = Duration::from_millis(800);

// Bounds one loopback request body; the relay batches to the team server itself.
const MAX_NUDGE_ENTRIES: usize = 200;

#[derive(Debug, Serialize)]
struct RelayPushEntryWire {
    kind: String,
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<String>,
    external_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_commit: Option<String>,
}

#[derive(Debug, Serialize)]
struct RelayPushRequestWire {
    server_url: String,
    project_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    bearer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    since_cursor: Option<String>,
    entries: Vec<RelayPushEntryWire>,
}

#[derive(Debug, Default, Deserialize)]
struct RelayPushResultWire {
    external_id: String,
    remote_id: Option<String>,
    status: String,
}

#[derive(Debug, Default, Deserialize)]
struct RelayPulledEntryWire {
    remote_id: String,
    kind: String,
    title: String,
    body: Option<String>,
    source_commit: Option<String>,
    created_at: String,
    archived: bool,
}

#[derive(Debug, Default, Deserialize)]
struct RelayPollResponseWire {
    #[serde(default)]
    push_results: Vec<RelayPushResultWire>,
    #[serde(default)]
    pulled: Vec<RelayPulledEntryWire>,
    #[serde(default)]
    last_synced_at: Option<i64>,
    #[serde(default)]
    last_error: Option<String>,
}

#[derive(Debug, Default, Serialize)]
struct RelayAckRequestWire {
    server_url: String,
    project_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    applied_push_external_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    applied_pull_remote_ids: Vec<String>,
}

fn relay_target(cfg: &Config) -> Option<(String, String)> {
    if cfg.resolve_mode() != inkentry_core::config::SyncMode::LocalFirst {
        return None;
    }
    let server_url = cfg.server_url.clone()?;
    let project_id = cfg.project_id.clone()?;
    Some((server_url, project_id))
}

// Best-effort: must never fail or noticeably slow the write.
pub(super) async fn nudge_after_write(cfg: &Config, mem_path: &std::path::Path) {
    let Some((server_url, project_id)) = relay_target(cfg) else {
        return;
    };

    if std::io::stdin().is_terminal() {
        // Keep on one line: `daemon_spawn_call_sites` matches this call lexically, line by line.
        let _ = super::super::server::ensure_server_running(DEFAULT_SERVER_PORT, cfg).await;
    }

    let Some(port) = super::super::server::probe_local_relay_port().await else {
        return;
    };
    register_and_push(cfg, mem_path, &server_url, &project_id, port).await;
}

// An empty outbox still registers: any push starts the session's pull task,
// so a read-only instance still receives live pulls.
async fn register_and_push(
    cfg: &Config,
    mem_path: &std::path::Path,
    server_url: &str,
    project_id: &str,
    port: u16,
) {
    let Ok(local) = MemoryStore::open(mem_path) else {
        return;
    };
    let Ok(rows) = local.rows_for_sync(false) else {
        return;
    };
    let entries: Vec<RelayPushEntryWire> = rows
        .iter()
        .filter(|r| !r.archived && r.remote_id.is_none())
        .take(MAX_NUDGE_ENTRIES)
        .map(|r| RelayPushEntryWire {
            kind: r.kind.clone(),
            title: r.title.clone(),
            body: if r.body.is_empty() {
                None
            } else {
                Some(r.body.clone())
            },
            external_id: r.id.to_string(),
            source_commit: r.source_ref.clone(),
        })
        .collect();
    let since_cursor = local.max_remote_id().ok().flatten();
    let bearer = super::super::auth_api::ensure_fresh_server_key(cfg, server_url)
        .await
        .ok()
        .flatten();

    let Ok(client) = reqwest::Client::builder()
        .timeout(LOCAL_RELAY_TIMEOUT)
        .build()
    else {
        return;
    };
    let body = RelayPushRequestWire {
        server_url: server_url.to_string(),
        project_id: project_id.to_string(),
        bearer,
        since_cursor,
        entries,
    };
    let _ = client
        .post(format!("http://127.0.0.1:{port}/local/relay/push"))
        .json(&body)
        .send()
        .await;
}

pub(crate) struct PollOutcome {
    pub applied_pushes: usize,
    pub applied_pulls: usize,
    pub last_synced_at: Option<i64>,
    pub last_error: Option<String>,
}

pub(crate) async fn poll_and_apply(
    cfg: &Config,
    mem_path: &std::path::Path,
) -> Option<PollOutcome> {
    let (server_url, project_id) = relay_target(cfg)?;
    let port = super::super::server::probe_local_relay_port().await?;
    register_and_push(cfg, mem_path, &server_url, &project_id, port).await;
    let local = MemoryStore::open(mem_path).ok()?;

    let client = reqwest::Client::builder()
        .timeout(LOCAL_RELAY_TIMEOUT)
        .build()
        .ok()?;
    let resp = client
        .get(format!("http://127.0.0.1:{port}/local/relay/poll"))
        .query(&[("server_url", &server_url), ("project_id", &project_id)])
        .send()
        .await
        .ok()?;
    let body: RelayPollResponseWire = resp.json().await.ok()?;

    // Peek, not drain: only entries named in the ack below are retired from the relay's
    // buffer, so one that fails to apply is offered again on the next poll.
    let mut applied_pushes = 0usize;
    let mut acked_push_ids: Vec<String> = Vec::new();
    for r in &body.push_results {
        let durably_persisted = r.status == "created" || r.status == "skipped";
        if !durably_persisted {
            continue;
        }
        if let (Some(remote_id), Ok(local_id)) = (&r.remote_id, r.external_id.parse::<NoteId>())
            && local.has_note(&local_id).unwrap_or(false)
            && local.set_remote_id(&local_id, remote_id).is_ok()
        {
            applied_pushes += 1;
            acked_push_ids.push(r.external_id.clone());
        }
    }

    let mut applied_pulls = 0usize;
    let mut acked_pull_ids: Vec<String> = Vec::new();
    for e in &body.pulled {
        let created_secs = super::sync::parse_iso_to_secs(&e.created_at);
        if local
            .apply_remote_note(
                &e.remote_id,
                &e.kind,
                &e.title,
                e.body.as_deref().unwrap_or(""),
                e.source_commit.as_deref(),
                created_secs,
                e.archived,
            )
            .is_ok()
        {
            applied_pulls += 1;
            acked_pull_ids.push(e.remote_id.clone());
        }
    }

    if !acked_push_ids.is_empty() || !acked_pull_ids.is_empty() {
        let ack_body = RelayAckRequestWire {
            server_url: server_url.clone(),
            project_id: project_id.clone(),
            applied_push_external_ids: acked_push_ids,
            applied_pull_remote_ids: acked_pull_ids,
        };
        // Best-effort: a dropped ack only means already-applied (idempotent) entries
        // are offered again on the next poll.
        let _ = client
            .post(format!("http://127.0.0.1:{port}/local/relay/ack"))
            .json(&ack_body)
            .send()
            .await;
    }

    let outcome = PollOutcome {
        applied_pushes,
        applied_pulls,
        last_synced_at: body.last_synced_at,
        last_error: body.last_error,
    };
    if outcome.applied_pushes > 0 || outcome.applied_pulls > 0 {
        tracing::debug!(
            applied_pushes = outcome.applied_pushes,
            applied_pulls = outcome.applied_pulls,
            "applied relay poll results to local memory.db"
        );
    }
    Some(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::sync::OnceLock;

    use serial_test::serial;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn register_sqlite_vec() {
        static INIT: OnceLock<()> = OnceLock::new();
        INIT.get_or_init(|| {
            #[allow(clippy::missing_transmute_annotations)]
            unsafe {
                rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                    sqlite_vec::sqlite3_vec_init as *const (),
                )));
            }
        });
    }

    fn open_store(path: &std::path::Path) -> MemoryStore {
        register_sqlite_vec();
        MemoryStore::open(path).expect("open memory.db")
    }

    // Stands in for `.inkentry/config.toml`: the relay connects only to team targets
    // that local configuration declares.
    #[derive(Clone, Default)]
    struct DeclaredTargets(
        std::sync::Arc<std::sync::Mutex<Vec<inkentry_core::config::TeamTarget>>>,
    );

    impl DeclaredTargets {
        fn declare(&self, server_url: &str, project_id: &str) {
            self.0
                .lock()
                .unwrap()
                .push(inkentry_core::config::TeamTarget {
                    server_url: server_url.to_string(),
                    project_id: project_id.to_string(),
                    server_ca: None,
                });
        }

        fn policy(&self) -> inkentry_server::relay::RelayPolicy {
            let declared = self.0.clone();
            inkentry_server::relay::RelayPolicy::from_fn(move || declared.lock().unwrap().clone())
        }
    }

    async fn spawn_inkentry_server(declared: &DeclaredTargets) -> (SocketAddr, String) {
        register_sqlite_vec();
        let db_dir = TempDir::new().unwrap();
        let db =
            inkentry_server::db::ServerDb::open(&db_dir.path().join("server.db"), 4, "test-model")
                .unwrap();
        let instance_id = db.get_or_create_instance_id().unwrap();
        let reported_id = instance_id.clone();
        let state = inkentry_server::AppState {
            db: std::sync::Arc::new(tokio::sync::Mutex::new(db)),
            auth: std::sync::Arc::new(inkentry_server::auth::ApiKeyAuth::new(None)),
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
            rate_limiter: std::sync::Arc::new(inkentry_server::rate_limiter::RateLimiter::new(
                1000, 60,
            )),
            instance_id,
            started_by: None,
            trusted_proxies: Default::default(),
            relay: inkentry_server::relay::RelayRegistry::new(declared.policy()),
            repair_signal: inkentry_server::repair::RepairSignal::new(),
        };
        let app = inkentry_server::router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (addr, reported_id)
    }

    async fn spawn_local_relay(state_dir: &std::path::Path) -> (SocketAddr, DeclaredTargets) {
        let declared = DeclaredTargets::default();
        let (addr, instance_id) = spawn_inkentry_server(&declared).await;
        std::fs::create_dir_all(state_dir).unwrap();
        std::fs::write(state_dir.join("server.port"), format!("{}\n", addr.port())).unwrap();
        // The relay gate requires all three files a real start records; the pid is this
        // process because the relay runs in-process.
        std::fs::write(
            state_dir.join("server.pid"),
            format!("{}\n", std::process::id()),
        )
        .unwrap();
        std::fs::write(
            state_dir.join("server.instance_id"),
            format!("{instance_id}\n"),
        )
        .unwrap();
        (addr, declared)
    }

    struct StateDirGuard {
        prev_state_dir: Option<std::ffi::OsString>,
        prev_trust: Option<std::ffi::OsString>,
        tmp: TempDir,
    }
    impl StateDirGuard {
        fn new() -> Self {
            let prev_state_dir = std::env::var_os("INKENTRY_STATE_DIR");
            let prev_trust = std::env::var_os("INKENTRY_TEST_TRUST_RECORDED_RESPONDER");
            let tmp = TempDir::new().unwrap();
            unsafe {
                std::env::set_var("INKENTRY_STATE_DIR", tmp.path());
                // The relay is in-process, so the recorded pid is this binary and the OS query
                // cannot match it; the seam relaxes only that.
                std::env::set_var("INKENTRY_TEST_TRUST_RECORDED_RESPONDER", "1");
            }
            Self {
                prev_state_dir,
                prev_trust,
                tmp,
            }
        }
        fn path(&self) -> &std::path::Path {
            self.tmp.path()
        }
    }
    impl Drop for StateDirGuard {
        fn drop(&mut self) {
            // SAFETY: `#[serial(server_state_dir_env)]` on every test using
            // this guard serialises against every other test touching these
            // vars (this crate's `server.rs` tests use the same group name).
            unsafe {
                match &self.prev_state_dir {
                    Some(v) => std::env::set_var("INKENTRY_STATE_DIR", v),
                    None => std::env::remove_var("INKENTRY_STATE_DIR"),
                }
                match &self.prev_trust {
                    Some(v) => std::env::set_var("INKENTRY_TEST_TRUST_RECORDED_RESPONDER", v),
                    None => std::env::remove_var("INKENTRY_TEST_TRUST_RECORDED_RESPONDER"),
                }
            }
        }
    }

    fn local_first_cfg(team_server_uri: &str) -> Config {
        Config {
            server_url: Some(team_server_uri.to_string()),
            project_id: Some("proj".to_string()),
            ..Default::default()
        }
    }

    #[tokio::test]
    #[serial(server_state_dir_env)]
    async fn nudge_after_write_relays_pending_rows_and_a_later_poll_stamps_remote_id() {
        let state_guard = StateDirGuard::new();
        let (_, declared) = spawn_local_relay(state_guard.path()).await;

        let mem_dir = TempDir::new().unwrap();
        let mem_path = mem_dir.path().join("memory.db");
        let store = open_store(&mem_path);
        store
            .add_note("decision", "T", "body", &[], &[], None, None)
            .unwrap();
        let uuid = store.rows_for_sync(false).unwrap()[0].id.to_string();
        drop(store);

        let team_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 1, "skipped": 0, "failed": 0,
                "results": [{"status": "created", "external_id": uuid, "id": "cloud-1"}]
            })))
            .mount(&team_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"entries": [], "count": 0})),
            )
            .mount(&team_server)
            .await;

        declared.declare(&team_server.uri(), "proj");
        let cfg = local_first_cfg(&team_server.uri());
        nudge_after_write(&cfg, &mem_path).await;

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut applied = 0usize;
        while std::time::Instant::now() < deadline {
            if let Some(outcome) = poll_and_apply(&cfg, &mem_path).await {
                applied = outcome.applied_pushes;
                if applied > 0 {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        assert!(
            applied >= 1,
            "the push-ack must be applied via CLI-side storage (poll_and_apply also \
             re-registers on every call, per item 20, so a still-unstamped row can \
             legitimately be offered more than once before it lands — the row-level \
             assertions below are the authoritative check): got {applied}"
        );

        let store = open_store(&mem_path);
        assert_eq!(
            store.note_id_for_remote_id("cloud-1").unwrap(),
            Some(uuid.parse().unwrap()),
            "the row must carry the cloud-assigned remote_id after the poll applies it"
        );
        assert_eq!(
            store.pending_sync_count().unwrap(),
            0,
            "a stamped row must no longer count as pending"
        );
    }

    #[tokio::test]
    #[serial(server_state_dir_env)]
    async fn nudge_after_write_is_a_noop_when_mode_is_not_local_first() {
        let state_guard = StateDirGuard::new();
        let (addr, declared) = spawn_local_relay(state_guard.path()).await;
        declared.declare(&format!("http://127.0.0.1:{}", addr.port()), "proj");

        let mem_dir = TempDir::new().unwrap();
        let mem_path = mem_dir.path().join("memory.db");
        let store = open_store(&mem_path);
        store
            .add_note("decision", "T", "body", &[], &[], None, None)
            .unwrap();
        drop(store);

        for mode in [
            inkentry_core::config::SyncMode::Offline,
            inkentry_core::config::SyncMode::CloudFirst,
        ] {
            let cfg = Config {
                server_url: Some(format!("http://127.0.0.1:{}", addr.port())),
                project_id: Some("proj".to_string()),
                mode: Some(mode),
                ..Default::default()
            };
            nudge_after_write(&cfg, &mem_path).await;
        }

        // Not checked via `poll_and_apply`: a local_first poll would itself register and push.
        let store = open_store(&mem_path);
        assert_eq!(
            store.pending_sync_count().unwrap(),
            1,
            "offline/cloud_first nudges must never touch the outbox row"
        );
    }

    #[tokio::test]
    async fn nudge_after_write_is_a_noop_without_project_id() {
        let mem_dir = TempDir::new().unwrap();
        let mem_path = mem_dir.path().join("memory.db");
        let store = open_store(&mem_path);
        store
            .add_note("decision", "T", "body", &[], &[], None, None)
            .unwrap();
        drop(store);

        let cfg = Config {
            server_url: Some("https://team.example".to_string()),
            project_id: None,
            ..Default::default()
        };
        let start = std::time::Instant::now();
        nudge_after_write(&cfg, &mem_path).await;
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "must not hang absent a project_id"
        );

        let store = open_store(&mem_path);
        assert_eq!(
            store.pending_sync_count().unwrap(),
            1,
            "the row stays queued; nothing to relay to without a project_id"
        );
    }

    #[tokio::test]
    #[serial(server_state_dir_env)]
    async fn nudge_after_write_returns_quickly_when_no_local_relay_is_running() {
        let _state_guard = StateDirGuard::new();
        let mem_dir = TempDir::new().unwrap();
        let mem_path = mem_dir.path().join("memory.db");
        let store = open_store(&mem_path);
        store
            .add_note("decision", "T", "body", &[], &[], None, None)
            .unwrap();
        drop(store);

        let cfg = local_first_cfg("https://team.example");
        let start = std::time::Instant::now();
        nudge_after_write(&cfg, &mem_path).await;
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "no local relay running must be a fast no-op, not a bounded-timeout wait: {:?}",
            start.elapsed()
        );

        let store = open_store(&mem_path);
        assert_eq!(
            store.pending_sync_count().unwrap(),
            1,
            "the write itself is unaffected: the row stays durably queued"
        );
    }

    #[tokio::test]
    async fn poll_and_apply_returns_none_when_not_local_first() {
        let mem_dir = TempDir::new().unwrap();
        let mem_path = mem_dir.path().join("memory.db");
        let _store = open_store(&mem_path);

        let cfg = Config {
            mode: Some(inkentry_core::config::SyncMode::Offline),
            ..Default::default()
        };
        assert!(poll_and_apply(&cfg, &mem_path).await.is_none());
    }

    #[tokio::test]
    #[serial(server_state_dir_env)]
    async fn repeated_nudges_never_stop_the_local_relay() {
        let state_guard = StateDirGuard::new();
        let (_, declared) = spawn_local_relay(state_guard.path()).await;

        let mem_dir = TempDir::new().unwrap();
        let mem_path = mem_dir.path().join("memory.db");
        let _store = open_store(&mem_path);

        let team_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"entries": [], "count": 0})),
            )
            .mount(&team_server)
            .await;
        declared.declare(&team_server.uri(), "proj");
        let cfg = local_first_cfg(&team_server.uri());

        for _ in 0..3 {
            nudge_after_write(&cfg, &mem_path).await;
            assert!(
                crate::cli::cmd::server::probe_local_relay_port()
                    .await
                    .is_some(),
                "the local relay must still be reachable after each nudge"
            );
        }
    }

    // Caller must hold `serial(server_state_dir_env)` and a `RelayEnvGuard`, or the value
    // leaks into the next test in the group.
    fn point_state_dir_at(dir: &std::path::Path) {
        unsafe { std::env::set_var("INKENTRY_STATE_DIR", dir) };
    }

    struct RelayEnvGuard {
        prev_state_dir: Option<std::ffi::OsString>,
        prev_trust: Option<std::ffi::OsString>,
    }
    impl RelayEnvGuard {
        fn install() -> Self {
            let me = Self {
                prev_state_dir: std::env::var_os("INKENTRY_STATE_DIR"),
                prev_trust: std::env::var_os("INKENTRY_TEST_TRUST_RECORDED_RESPONDER"),
            };
            // In-process relays: the recorded pid is this test binary.
            unsafe { std::env::set_var("INKENTRY_TEST_TRUST_RECORDED_RESPONDER", "1") };
            me
        }
    }
    impl Drop for RelayEnvGuard {
        fn drop(&mut self) {
            // SAFETY: see `StateDirGuard::drop` above; same serial group.
            unsafe {
                match &self.prev_state_dir {
                    Some(v) => std::env::set_var("INKENTRY_STATE_DIR", v),
                    None => std::env::remove_var("INKENTRY_STATE_DIR"),
                }
                match &self.prev_trust {
                    Some(v) => std::env::set_var("INKENTRY_TEST_TRUST_RECORDED_RESPONDER", v),
                    None => std::env::remove_var("INKENTRY_TEST_TRUST_RECORDED_RESPONDER"),
                }
            }
        }
    }

    // Runs with the CWD outside any git repo: `memory list` imports `refs/notes/inkentry`
    // from the CWD's repo, and stray rows there get pushed and advance `since_cursor`
    // past the entry under test. No config disables that read path.
    struct CwdOutsideAnyRepo {
        prev: std::path::PathBuf,
        _dir: TempDir,
    }
    impl CwdOutsideAnyRepo {
        fn enter() -> Self {
            let prev = std::env::current_dir().expect("cwd");
            let dir = TempDir::new().unwrap();
            std::env::set_current_dir(dir.path()).expect("set cwd");
            let guard = Self { prev, _dir: dir };
            // Constructed first so a failure still restores the CWD. A TMPDIR inside a checkout
            // would silently undo the isolation, so assert it.
            assert!(
                crate::storage::NotesRefs::discover(None).is_none(),
                "TMPDIR resolves inside a git repo, so the CWD guard isolates nothing"
            );
            guard
        }
    }
    impl Drop for CwdOutsideAnyRepo {
        fn drop(&mut self) {
            // Restore before the TempDir field drops.
            let _ = std::env::set_current_dir(&self.prev);
        }
    }

    #[tokio::test]
    #[serial(server_state_dir_env, process_cwd)]
    async fn entry_added_on_instance_a_becomes_visible_on_instance_b_via_live_pull() {
        let _restore_state_dir = RelayEnvGuard::install();
        let _cwd = CwdOutsideAnyRepo::enter();
        let (team_addr, _) = spawn_inkentry_server(&DeclaredTargets::default()).await;
        let team_uri = format!("http://{}", team_addr);

        let state_a = TempDir::new().unwrap();
        let state_b = TempDir::new().unwrap();
        let (_, declared_a) = spawn_local_relay(state_a.path()).await;
        let (_, declared_b) = spawn_local_relay(state_b.path()).await;
        declared_a.declare(&team_uri, "proj");
        declared_b.declare(&team_uri, "proj");

        let mem_dir_a = TempDir::new().unwrap();
        let mem_a = mem_dir_a.path().join("memory.db");
        let mem_dir_b = TempDir::new().unwrap();
        let mem_b = mem_dir_b.path().join("memory.db");
        let _store_a = open_store(&mem_a);
        let _store_b = open_store(&mem_b);

        let cfg = local_first_cfg(&team_uri);

        // Register B first so its pull loop is live with nothing to catch up on: the entry
        // can then only arrive via the live SSE wake-up.
        point_state_dir_at(state_b.path());
        assert!(
            poll_and_apply(&cfg, &mem_b).await.is_some(),
            "instance B's relay must be reachable"
        );

        point_state_dir_at(state_a.path());
        let store_a = open_store(&mem_a);
        store_a
            .add_note(
                "decision",
                "Cross-instance entry",
                "body",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();
        drop(store_a);
        nudge_after_write(&cfg, &mem_a).await;

        // Drive the real `memory list`, not `poll_and_apply`, so the test fails unless the
        // read path itself applies relay results.
        point_state_dir_at(state_b.path());
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut seen = false;
        while std::time::Instant::now() < deadline {
            crate::cli::cmd::memory::list::memory_list(
                crate::cli::cmd::memory::MemoryListArgs {
                    kind: None,
                    source_ref: None,
                    limit: 20,
                    format: "json".to_string(),
                    archived: false,
                    as_of: None,
                    local_only: true,
                    tag: None,
                    file: None,
                },
                &mem_b,
                &cfg,
                None,
                false,
            )
            .await
            .unwrap();
            let store_b = open_store(&mem_b);
            if store_b
                .rows_for_sync(false)
                .unwrap()
                .iter()
                .any(|n| n.title == "Cross-instance entry")
            {
                seen = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            seen,
            "instance A's write must become visible via a real `inkentry memory list` \
             invocation on instance B, without any explicit `inkentry sync`/`plumbing pull`"
        );
    }

    #[tokio::test]
    #[serial(server_state_dir_env)]
    async fn kill_and_restart_the_local_relay_mid_drain_loses_nothing_and_dedupes() {
        let _restore_state_dir = RelayEnvGuard::install();
        let (team_addr, _) = spawn_inkentry_server(&DeclaredTargets::default()).await;
        let team_uri = format!("http://{}", team_addr);
        let cfg = local_first_cfg(&team_uri);

        let state_1 = TempDir::new().unwrap();
        let (_, declared_1) = spawn_local_relay(state_1.path()).await;
        declared_1.declare(&team_uri, "proj");

        let mem_dir = TempDir::new().unwrap();
        let mem_path = mem_dir.path().join("memory.db");
        let store = open_store(&mem_path);
        store
            .add_note("decision", "A", "body", &[], &[], None, None)
            .unwrap();
        store
            .add_note("decision", "B", "body", &[], &[], None, None)
            .unwrap();
        drop(store);

        point_state_dir_at(state_1.path());
        nudge_after_write(&cfg, &mem_path).await;

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            poll_and_apply(&cfg, &mem_path).await;
            let store = open_store(&mem_path);
            if store.pending_sync_count().unwrap() == 0 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "A and B did not land on the team server before the deadline"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // "Restart": a second, wholly independent local relay process/registry.
        let state_2 = TempDir::new().unwrap();
        let (_, declared_2) = spawn_local_relay(state_2.path()).await;
        declared_2.declare(&team_uri, "proj");

        let store = open_store(&mem_path);
        store
            .add_note("decision", "C", "body", &[], &[], None, None)
            .unwrap();
        drop(store);

        point_state_dir_at(state_2.path());
        nudge_after_write(&cfg, &mem_path).await;

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            poll_and_apply(&cfg, &mem_path).await;
            let store = open_store(&mem_path);
            if store.pending_sync_count().unwrap() == 0 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "C did not land on the team server before the deadline"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let store = open_store(&mem_path);
        assert_eq!(
            store.count().unwrap(),
            3,
            "no data loss and no duplicates across the simulated restart"
        );
        let rows = store.rows_for_sync(false).unwrap();
        assert!(
            rows.iter().all(|r| r.remote_id.is_some()),
            "every row must carry a remote_id after re-deriving through the new relay"
        );
    }

    // Forces the apply failure with a competing SQLite writer holding memory.db's write
    // lock. State is read through a bare connection because `MemoryStore::open` always
    // writes and would itself contend for the lock.

    fn raw_has_remote_id(mem_path: &std::path::Path, remote_id: &str) -> bool {
        let conn = rusqlite::Connection::open(mem_path).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM notes WHERE remote_id = ?1",
                rusqlite::params![remote_id],
                |r| r.get(0),
            )
            .unwrap();
        n > 0
    }

    async fn raw_relay_peek(
        port: u16,
        server_url: &str,
        project_id: &str,
    ) -> RelayPollResponseWire {
        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://127.0.0.1:{port}/local/relay/poll"))
            .query(&[("server_url", server_url), ("project_id", project_id)])
            .send()
            .await
            .unwrap();
        resp.json().await.unwrap()
    }

    #[tokio::test]
    #[serial(server_state_dir_env)]
    async fn a_pull_apply_failure_without_a_restart_does_not_lose_the_row() {
        let state_guard = StateDirGuard::new();
        let (team_addr, _) = spawn_inkentry_server(&DeclaredTargets::default()).await;
        let (_, declared) = spawn_local_relay(state_guard.path()).await;
        let team_uri = format!("http://{}", team_addr);
        declared.declare(&team_uri, "proj");
        let cfg = local_first_cfg(&team_uri);

        let mem_dir = TempDir::new().unwrap();
        let mem_path = mem_dir.path().join("memory.db");
        let _store = open_store(&mem_path);

        // Register unlocked so the relay's pull loop is live before the row is seeded and
        // its own catch-up picks the row up.
        assert!(
            poll_and_apply(&cfg, &mem_path).await.is_some(),
            "the local relay must be reachable"
        );

        // The stream only yields notes created strictly after its second-granularity start;
        // cross a second boundary so the seed cannot be missed.
        tokio::time::sleep(Duration::from_millis(1100)).await;

        // Seed on the team server as if pushed by another instance. Take the identity from
        // the relay's buffered entry: the batch response's `id` differs from the sync_id
        // `/memory/since` keys on.
        let http = reqwest::Client::new();
        http.post(format!("{team_uri}/v1/projects/proj/memory/batch"))
            .json(&serde_json::json!({
                "entries": [{
                    "kind": "decision", "title": "Remote", "body": "b",
                    "external_id": "seed-ext-1"
                }]
            }))
            .send()
            .await
            .unwrap();

        // Wait via a non-destructive peek so the wait never applies anything.
        // never applies/consumes anything).
        let port = crate::cli::cmd::server::probe_local_relay_port()
            .await
            .expect("local relay must be reachable");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let remote_id = loop {
            let peek = raw_relay_peek(port, &team_uri, "proj").await;
            if let Some(entry) = peek.pulled.iter().find(|p| p.title == "Remote") {
                break entry.remote_id.clone();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the relay never buffered the seeded row via its background catch-up"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        };

        // Lock only after the row is buffered, so this is the CLI's first apply attempt.
        let locker = rusqlite::Connection::open(&mem_path).unwrap();
        locker.execute_batch("BEGIN IMMEDIATE;").unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            poll_and_apply(&cfg, &mem_path).await;
            assert!(
                !raw_has_remote_id(&mem_path, &remote_id),
                "the row must not appear locally while the competing writer holds the lock"
            );
            if std::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        locker.execute_batch("COMMIT;").unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            poll_and_apply(&cfg, &mem_path).await;
            if raw_has_remote_id(&mem_path, &remote_id) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the pulled row must still be recoverable once the lock is released \
                 (it must never have been dropped by an earlier failed apply attempt)"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    #[serial(server_state_dir_env)]
    async fn a_push_stamp_failure_without_a_restart_does_not_strand_the_row_pending_forever() {
        let state_guard = StateDirGuard::new();
        let (_, declared) = spawn_local_relay(state_guard.path()).await;

        let mem_dir = TempDir::new().unwrap();
        let mem_path = mem_dir.path().join("memory.db");
        let uuid = {
            let store = open_store(&mem_path);
            store
                .add_note("decision", "T", "body", &[], &[], None, None)
                .unwrap();
            store.rows_for_sync(false).unwrap()[0].id.to_string()
        };

        let team_server = MockServer::start().await;
        // Only the first push creates the row. A re-push of the same external_id gets
        // `skipped` with no id, like a real team server's dedupe, so recovery must come from
        // the relay's still-buffered `created` result rather than a re-push.
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 1, "skipped": 0, "failed": 0,
                "results": [{"status": "created", "external_id": uuid, "id": "cloud-1"}]
            })))
            .up_to_n_times(1)
            .mount(&team_server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 0, "skipped": 1, "failed": 0,
                "results": [{"status": "skipped", "external_id": uuid, "id": null}]
            })))
            .mount(&team_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"entries": [], "count": 0})),
            )
            .mount(&team_server)
            .await;

        declared.declare(&team_server.uri(), "proj");
        let cfg = local_first_cfg(&team_server.uri());
        nudge_after_write(&cfg, &mem_path).await;

        let port = crate::cli::cmd::server::probe_local_relay_port()
            .await
            .expect("local relay must be reachable");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let peek = raw_relay_peek(port, &team_server.uri(), "proj").await;
            if peek.push_results.iter().any(|r| r.external_id == uuid) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the relay never buffered the push ack"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Lock only after the ack is buffered.
        let locker = rusqlite::Connection::open(&mem_path).unwrap();
        locker.execute_batch("BEGIN IMMEDIATE;").unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            poll_and_apply(&cfg, &mem_path).await;
            assert!(
                !raw_has_remote_id(&mem_path, "cloud-1"),
                "the row must not get stamped while the competing writer holds the lock"
            );
            if std::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        locker.execute_batch("COMMIT;").unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            poll_and_apply(&cfg, &mem_path).await;
            if raw_has_remote_id(&mem_path, "cloud-1") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the push ack must still be recoverable once the lock is released \
                 (it must never have been dropped by an earlier failed stamp attempt)"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let reader = open_store(&mem_path);
        assert_eq!(
            reader.pending_sync_count().unwrap(),
            0,
            "the row must no longer read as pending once the stamp succeeds"
        );
    }
}
