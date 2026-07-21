//! ADR-037 P2: post-write nudge + status poll for the local relay
//! (`crate::cli::cmd::server::probe_local_relay_port`, spelunk-server's
//! `relay` module).
//!
//! Two entry points:
//! - [`nudge_after_write`] — called after a `local_first` `memory
//!   add`/`archive`/`supersede` commits, to auto-start (interactive only, D6)
//!   and hand the local server's relay any newly-unpushed rows. Best-effort
//!   only: any failure here must never surface as a write error, a
//!   meaningfully added write latency, or a non-zero exit (items 7/9/10/11).
//! - [`poll_and_apply`] — called from `spelunk status` to drain whatever the
//!   relay has buffered (push acks, pulled rows) and apply it locally, so
//!   status can report a fresh "N pending, last synced Xm ago" without ever
//!   printing a manual-sync call to action.
//!
//! `memory.db` is opened and written **only** by this CLI-side code — never
//! by the server (D5); the relay's own local HTTP surface only ever carries
//! row data and identifiers, never a filesystem path.

use std::io::IsTerminal;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::storage::MemoryStore;

/// Bound on both the nudge and poll HTTP calls to the local relay: high
/// enough for a loopback round trip, low enough that an absent/wedged local
/// server can never make a write feel slow (item 9).
const LOCAL_RELAY_TIMEOUT: Duration = Duration::from_millis(800);

/// Cap on entries offered in a single nudge, mirroring `push_local`'s own
/// batch chunking (`sync.rs`) — the relay forwards them to the team server in
/// its own batches regardless, so this only bounds one loopback request body.
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

/// Resolve `(server_url, project_id)` for the relay, or `None` when this
/// project has nothing to relay: not `local_first`, no `server_url`, or no
/// `project_id` (mirrors `spelunk sync`'s own requirement — there is no
/// `--project` override on a write command to fall back to, so a missing
/// `project_id` here just means the background nudge quietly does nothing,
/// same as it always has).
fn relay_target(cfg: &Config) -> Option<(String, String)> {
    if cfg.resolve_mode() != spelunk_core::config::SyncMode::LocalFirst {
        return None;
    }
    let server_url = cfg.server_url.clone()?;
    let project_id = cfg.project_id.clone()?;
    Some((server_url, project_id))
}

/// Auto-start (interactive only, D6) and nudge the local relay after a
/// `local_first` write. See module docs for the non-blocking contract.
pub(super) async fn nudge_after_write(cfg: &Config, mem_path: &std::path::Path) {
    let Some((server_url, project_id)) = relay_target(cfg) else {
        return;
    };

    if std::io::stdin().is_terminal() {
        let _ = super::super::server::ensure_server_running(7777).await;
    }

    let Some(port) = super::super::server::probe_local_relay_port().await else {
        return;
    };
    register_and_push(cfg, mem_path, &server_url, &project_id, port).await;
}

/// Register this project's relay session (creating it and starting its pull
/// loop on first sight — item 12/18/20) and hand over any currently-pending
/// outbox rows. An empty outbox still registers: the server-side push
/// handler starts the session's pull task regardless of whether `entries` is
/// empty, which is what lets a purely-read instance (never writing locally)
/// still receive live pulls (item 20's two-instance scenario needs exactly
/// this — instance B may never call `memory add` at all).
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
            external_id: r.uuid.clone(),
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

/// What [`poll_and_apply`] applied, for `spelunk status`'s pending/last-synced
/// line.
pub(crate) struct PollOutcome {
    pub applied_pushes: usize,
    pub applied_pulls: usize,
    pub last_synced_at: Option<i64>,
    pub last_error: Option<String>,
}

/// Poll the local relay (if reachable) for a project's buffered push-acks and
/// pulled rows, apply them via the CLI-side storage layer, and return what
/// happened. Returns `None` when there is nothing to poll (not `local_first`,
/// no server configured, or no local relay reachable) — `spelunk status`
/// falls back to a purely local pending-count in that case.
///
/// Also registers the relay session (via [`register_and_push`]) before
/// polling, same as a write's nudge: a purely-read instance that never calls
/// `memory add` still needs its session registered at some point for live
/// pull to reach it at all (item 20 — this is the mechanism that lets
/// instance B in the two-instance scenario pick up instance A's write
/// without ever writing locally itself).
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

    let mut applied_pushes = 0usize;
    for r in &body.push_results {
        let durably_persisted = r.status == "created" || r.status == "skipped";
        if !durably_persisted {
            continue;
        }
        if let (Some(remote_id), Ok(Some(local_id))) =
            (&r.remote_id, local.note_id_for_uuid(&r.external_id))
            && local.set_remote_id(local_id, remote_id).is_ok()
        {
            applied_pushes += 1;
        }
    }

    let mut applied_pulls = 0usize;
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
        }
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

    /// Spin up a real `spelunk-server` axum router (the actual production
    /// router, not a hand-rolled stand-in) on an ephemeral loopback port and
    /// return its address. Serves BOTH roles the same binary can play: the
    /// team-hosting `/v1/projects/*/memory*` routes (a stand-in for a real
    /// team server) and the local-only `/local/relay/*` routes (a stand-in
    /// for a real `spelunk server start`-ed daemon) — callers pick which
    /// role they're using it for by whether they write a state-dir port file
    /// (see [`spawn_local_relay`]) or pass the address as `server_url`.
    async fn spawn_spelunk_server() -> SocketAddr {
        register_sqlite_vec();
        let db_dir = TempDir::new().unwrap();
        let db =
            spelunk_server::db::ServerDb::open(&db_dir.path().join("server.db"), 4, "test-model")
                .unwrap();
        let instance_id = db.get_or_create_instance_id().unwrap();
        let state = spelunk_server::AppState {
            db: std::sync::Arc::new(tokio::sync::Mutex::new(db)),
            auth: std::sync::Arc::new(spelunk_server::auth::ApiKeyAuth::new(None)),
            conflict_threshold: spelunk_server::default_conflict_threshold(),
            embedder: spelunk_server::EmbedderSlot::disabled(),
            llm: None,
            max_tokens_ceiling: 8192,
            rate_limiter: std::sync::Arc::new(spelunk_server::rate_limiter::RateLimiter::new(
                1000, 60,
            )),
            instance_id,
            started_by: None,
            relay: spelunk_server::relay::RelayRegistry::new(),
        };
        let app = spelunk_server::router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        addr
    }

    /// Like [`spawn_spelunk_server`], but also writes the port into
    /// `state_dir/server.port` so `server::probe_local_relay_port`
    /// discovers it exactly the way it would discover a real
    /// `spelunk server start`-ed daemon — the *local relay* role.
    async fn spawn_local_relay(state_dir: &std::path::Path) -> SocketAddr {
        let addr = spawn_spelunk_server().await;
        std::fs::create_dir_all(state_dir).unwrap();
        std::fs::write(state_dir.join("server.port"), format!("{}\n", addr.port())).unwrap();
        addr
    }

    /// Sets `SPELUNK_STATE_DIR` to a fresh temp dir for the test's duration,
    /// restoring the previous value on drop. Mirrors the guard in
    /// `server.rs`'s own tests.
    struct StateDirGuard(Option<std::ffi::OsString>, TempDir);
    impl StateDirGuard {
        fn new() -> Self {
            let prev = std::env::var_os("SPELUNK_STATE_DIR");
            let tmp = TempDir::new().unwrap();
            unsafe { std::env::set_var("SPELUNK_STATE_DIR", tmp.path()) };
            Self(prev, tmp)
        }
        fn path(&self) -> &std::path::Path {
            self.1.path()
        }
    }
    impl Drop for StateDirGuard {
        fn drop(&mut self) {
            // SAFETY: `#[serial(server_state_dir_env)]` on every test using
            // this guard serialises against every other test touching the var
            // (this crate's `server.rs` tests use the same group name).
            unsafe {
                match &self.0 {
                    Some(v) => std::env::set_var("SPELUNK_STATE_DIR", v),
                    None => std::env::remove_var("SPELUNK_STATE_DIR"),
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

    // ── items 7/8/11/12/14: nudge -> relay push -> poll stamps remote_id ────

    #[tokio::test]
    #[serial(server_state_dir_env)]
    async fn nudge_after_write_relays_pending_rows_and_a_later_poll_stamps_remote_id() {
        let state_guard = StateDirGuard::new();
        spawn_local_relay(state_guard.path()).await;

        let mem_dir = TempDir::new().unwrap();
        let mem_path = mem_dir.path().join("memory.db");
        let store = open_store(&mem_path);
        store
            .add_note("decision", "T", "body", &[], &[], None, None)
            .unwrap();
        let uuid = store.rows_for_sync(false).unwrap()[0].uuid.clone();
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

        let cfg = local_first_cfg(&team_server.uri());
        nudge_after_write(&cfg, &mem_path).await;

        // The remote push happens in the local relay's own detached task;
        // poll until it lands rather than assuming a fixed sleep.
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
            store.note_id_for_uuid(&uuid).unwrap(),
            "the row must carry the cloud-assigned remote_id after the poll applies it"
        );
        assert_eq!(
            store.pending_sync_count().unwrap(),
            0,
            "a stamped row must no longer count as pending"
        );
    }

    // ── item 26/29: gated on local_first (offline/cloud_first are no-ops) ──

    #[tokio::test]
    #[serial(server_state_dir_env)]
    async fn nudge_after_write_is_a_noop_when_mode_is_not_local_first() {
        let state_guard = StateDirGuard::new();
        let addr = spawn_local_relay(state_guard.path()).await;

        let mem_dir = TempDir::new().unwrap();
        let mem_path = mem_dir.path().join("memory.db");
        let store = open_store(&mem_path);
        store
            .add_note("decision", "T", "body", &[], &[], None, None)
            .unwrap();
        drop(store);

        for mode in [
            spelunk_core::config::SyncMode::Offline,
            spelunk_core::config::SyncMode::CloudFirst,
        ] {
            let cfg = Config {
                server_url: Some(format!("http://127.0.0.1:{}", addr.port())),
                project_id: Some("proj".to_string()),
                mode: Some(mode),
                ..Default::default()
            };
            nudge_after_write(&cfg, &mem_path).await;
        }

        // Direct, race-free check: neither nudge touched the row at all — it
        // is exactly as it was before either call, still pending. (A
        // subsequent `local_first` `poll_and_apply` would itself register
        // and push, per item 20, so it is not used here to avoid conflating
        // "the gated nudges did nothing" with "a later, ungated poll did
        // something".)
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
        // No SPELUNK_STATE_DIR override, no local relay reachable either way:
        // this must return promptly without an unbounded wait.
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

    // ── item 9/10: absent local relay must not add meaningful latency ──────

    #[tokio::test]
    #[serial(server_state_dir_env)]
    async fn nudge_after_write_returns_quickly_when_no_local_relay_is_running() {
        let _state_guard = StateDirGuard::new();
        // No `spawn_local_relay` call: the state dir has no port file, so
        // `probe_local_relay_port` must return `None` without any network
        // call (item 10: outbox visibility never depends on a live
        // reconciler; item 9: latency stays offline-shaped).
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

    // ── poll_and_apply gating mirrors nudge_after_write's ───────────────────

    #[tokio::test]
    async fn poll_and_apply_returns_none_when_not_local_first() {
        let mem_dir = TempDir::new().unwrap();
        let mem_path = mem_dir.path().join("memory.db");
        let _store = open_store(&mem_path);

        let cfg = Config {
            mode: Some(spelunk_core::config::SyncMode::Offline),
            ..Default::default()
        };
        assert!(poll_and_apply(&cfg, &mem_path).await.is_none());
    }

    // ── item 30 guard: repeated nudges never disturb the running relay ─────
    // No idle-reap logic exists anywhere in this task's scope; this pins that
    // a later change cannot silently smuggle one in via this call path.

    #[tokio::test]
    #[serial(server_state_dir_env)]
    async fn repeated_nudges_never_stop_the_local_relay() {
        let state_guard = StateDirGuard::new();
        spawn_local_relay(state_guard.path()).await;

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

    /// Point `SPELUNK_STATE_DIR` at `dir` for the remainder of the current
    /// scope. Caller must hold `#[serial(server_state_dir_env)]` AND keep a
    /// [`RestoreStateDirOnDrop`] alive for the test's duration, or the
    /// mutated value leaks into whichever test in the same serial group runs
    /// next.
    fn point_state_dir_at(dir: &std::path::Path) {
        unsafe { std::env::set_var("SPELUNK_STATE_DIR", dir) };
    }

    /// Captures the current `SPELUNK_STATE_DIR` on construction and restores
    /// it on drop. Tests that call [`point_state_dir_at`] more than once (so
    /// [`StateDirGuard`] alone won't do, since it only knows the value it
    /// itself set) must hold one of these for the whole test body.
    struct RestoreStateDirOnDrop(Option<std::ffi::OsString>);
    impl RestoreStateDirOnDrop {
        fn capture() -> Self {
            Self(std::env::var_os("SPELUNK_STATE_DIR"))
        }
    }
    impl Drop for RestoreStateDirOnDrop {
        fn drop(&mut self) {
            // SAFETY: see `StateDirGuard::drop` above; same serial group.
            unsafe {
                match &self.0 {
                    Some(v) => std::env::set_var("SPELUNK_STATE_DIR", v),
                    None => std::env::remove_var("SPELUNK_STATE_DIR"),
                }
            }
        }
    }

    // ── item 20: SSE-driven live pull, two local instances, one team server ─
    //
    // Uses a REAL `spelunk-server` router as the team server (not a wiremock
    // stub): its `/v1/projects/*/memory*` team-hosting routes are the same
    // production handlers a real cloud-api-or-OSS team server would run, so
    // pushing through instance A's relay and having instance B's relay pick
    // it up exercises the actual SSE `/memory/stream` code path this
    // module's pull loop consumes, not just `/memory/since` polling.

    #[tokio::test]
    #[serial(server_state_dir_env)]
    async fn entry_added_on_instance_a_becomes_visible_on_instance_b_via_live_pull() {
        let _restore_state_dir = RestoreStateDirOnDrop::capture();
        let team_addr = spawn_spelunk_server().await;
        let team_uri = format!("http://{}", team_addr);

        let state_a = TempDir::new().unwrap();
        let state_b = TempDir::new().unwrap();
        spawn_local_relay(state_a.path()).await;
        spawn_local_relay(state_b.path()).await;

        let mem_dir_a = TempDir::new().unwrap();
        let mem_a = mem_dir_a.path().join("memory.db");
        let mem_dir_b = TempDir::new().unwrap();
        let mem_b = mem_dir_b.path().join("memory.db");
        let _store_a = open_store(&mem_a);
        let _store_b = open_store(&mem_b);

        let cfg = local_first_cfg(&team_uri);

        // Register B FIRST, before A ever writes, so B's pull loop is live
        // (holding its SSE connection) with nothing yet to catch up on — the
        // entry it eventually sees can only have arrived via the live SSE
        // wake-up + re-catch-up path, not B's own initial registration
        // catch-up.
        point_state_dir_at(state_b.path());
        assert!(
            poll_and_apply(&cfg, &mem_b).await.is_some(),
            "instance B's relay must be reachable"
        );

        // Now instance A writes and relays it to the team server.
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

        // Instance B: poll until the entry arrives via live pull.
        point_state_dir_at(state_b.path());
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut seen = false;
        while std::time::Instant::now() < deadline {
            if let Some(outcome) = poll_and_apply(&cfg, &mem_b).await
                && outcome.applied_pulls > 0
            {
                seen = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            seen,
            "instance A's write must become visible on instance B without \
             any explicit `spelunk sync`/`memory pull`"
        );

        let store_b = open_store(&mem_b);
        let notes = store_b.rows_for_sync(false).unwrap();
        assert!(
            notes.iter().any(|n| n.title == "Cross-instance entry"),
            "the pulled entry must be applied into instance B's own memory.db"
        );
    }

    // ── item 16: kill-and-restart mid-drain, no data loss, no duplicates ───
    //
    // Simulates a killed-and-restarted local server with a genuinely fresh
    // process: a second `spawn_local_relay` (a brand new axum router + a
    // brand new, empty `RelayRegistry`) that shares no state at all with the
    // first. Re-registering against it must re-derive the outbox/cursor from
    // `memory.db` alone and reach a correct, duplicate-free end state.

    #[tokio::test]
    #[serial(server_state_dir_env)]
    async fn kill_and_restart_the_local_relay_mid_drain_loses_nothing_and_dedupes() {
        let _restore_state_dir = RestoreStateDirOnDrop::capture();
        let team_addr = spawn_spelunk_server().await;
        let team_uri = format!("http://{}", team_addr);
        let cfg = local_first_cfg(&team_uri);

        let state_1 = TempDir::new().unwrap();
        spawn_local_relay(state_1.path()).await;

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

        // Wait for both A and B to land on the (real) team server before the
        // "restart".
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
        spawn_local_relay(state_2.path()).await;

        // A new, never-yet-pushed row, added after the "restart".
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
}
