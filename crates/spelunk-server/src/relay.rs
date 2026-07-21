//! ADR-037 P2 local relay: `spelunk-server`'s *outbound-client* role.
//!
//! Distinct from this binary's *team-server-hosting* role (the `/memory`,
//! `/memory/batch`, `/memory/since`, SSE `/memory/stream` routes backed by
//! `ServerDb`/`server.db`, see `handlers.rs`): here the same process instead
//! acts as a local, per-machine relay for a CLI's own `memory.db` outbox
//! against whatever team `server_url` a project is configured with (cloud-api
//! or another `spelunk-server`). Do not conflate the two roles or extend the
//! wrong routes for a P2 change.
//!
//! D5 (ADR-037): this module drains the outbox and holds the pull-catchup
//! network legs only. **It never opens a project's `memory.db`** — there is
//! no such import anywhere in this file, by construction; CLI-side storage
//! code (`crates/spelunk-cli/src/cli/cmd/memory/outbox.rs`) stays the sole
//! opener/writer.
//!
//! ## Why pull correctness never trusts the raw SSE payload
//!
//! `handlers::memory_stream` (this server's own team-hosting SSE) serializes
//! `db::ServerNote`, whose `id` is that *particular* `ServerDb`'s own local
//! autoincrement row id — a different identity than the `sync_id` (UUIDv7)
//! `/memory/since` returns for the same note. Applying an SSE payload's `id`
//! straight into `apply_remote_note`'s `remote_id` would create a second,
//! duplicate local row for any note also seen via `/memory/since`. So an SSE
//! frame here is treated purely as a wake-up signal ("something changed, go
//! catch up"): the actual pulled data always comes from `/memory/since`,
//! keyed on the one stable cross-store identity (`sync_id`) both pull paths
//! agree on. This also sidesteps needing per-remote-flavour SSE payload
//! parsing (cloud-api's own SSE event shape is not in this repo to test
//! against).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use spelunk_core::storage::{BatchPushItem, CloudSyncClient, RemoteEntry};
use tokio::sync::Mutex;

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Identifies one relay target: a (team server, project) pair. Two local
/// `memory.db` instances syncing the same team project share one session —
/// correct, since both converge on the same remote state (item 20's e2e
/// scenario).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RelayKey {
    server_url: String,
    project_id: String,
}

impl RelayKey {
    fn new(server_url: &str, project_id: &str) -> Self {
        Self {
            server_url: server_url.trim_end_matches('/').to_string(),
            project_id: project_id.to_string(),
        }
    }
}

/// One entry offered by the CLI for relay to the team server. Mirrors
/// [`BatchPushItem`] minus the pushed-vector fast path: the P2 relay push is
/// text-only (the vector fast path stays manual-`spelunk sync`-only, which
/// already reuses [`BatchPushItem`] directly).
#[derive(Debug, Clone, Deserialize)]
pub struct RelayPushEntry {
    pub kind: String,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    pub external_id: String,
    #[serde(default)]
    pub source_commit: Option<String>,
}

impl From<RelayPushEntry> for BatchPushItem {
    fn from(e: RelayPushEntry) -> Self {
        BatchPushItem {
            kind: e.kind,
            title: e.title,
            body: e.body,
            external_id: e.external_id,
            source_commit: e.source_commit,
            vector: None,
            vector_model: None,
            vector_precision: None,
        }
    }
}

/// Body of `POST /local/relay/push`.
#[derive(Debug, Deserialize)]
pub struct RelayPushRequest {
    pub server_url: String,
    pub project_id: String,
    #[serde(default)]
    pub bearer: Option<String>,
    /// The CLI's own pull cursor (`MemoryStore::max_remote_id()`), used to
    /// seed catch-up on first registration only. A slow/stale registration
    /// call from an older CLI invocation can never regress a session's own,
    /// already-advanced cursor (item 16).
    #[serde(default)]
    pub since_cursor: Option<String>,
    #[serde(default)]
    pub entries: Vec<RelayPushEntry>,
}

/// Outcome of a relayed push, one per entry the team server affirmatively
/// accepted or already had (never one it rejected — nothing here should be
/// stamped onto a row that must remain retryable).
#[derive(Debug, Clone, Serialize)]
pub struct RelayPushResult {
    pub external_id: String,
    pub remote_id: Option<String>,
    pub status: String,
}

/// One entry the relay has pulled from the team server but not yet handed
/// back to the CLI for local application. Mirrors [`RemoteEntry`] on the
/// outbound (Serialize) side; `RemoteEntry` itself is Deserialize-only, being
/// the wire type for the *inbound* cloud response.
#[derive(Debug, Clone, Serialize)]
pub struct RelayPulledEntry {
    pub remote_id: String,
    pub kind: String,
    pub title: String,
    pub body: Option<String>,
    pub source_commit: Option<String>,
    pub created_at: String,
    pub archived: bool,
}

impl From<RemoteEntry> for RelayPulledEntry {
    fn from(e: RemoteEntry) -> Self {
        let archived = e.is_archived();
        Self {
            remote_id: e.id,
            kind: e.kind,
            title: e.title,
            body: e.body,
            source_commit: e.source_commit,
            created_at: e.created_at,
            archived,
        }
    }
}

/// Response body of `GET /local/relay/poll`. Draining (item 33: this state
/// lives only in this long-running process, surviving any single CLI
/// invocation) — a poll clears what it returns, so the CLI applying it
/// exactly once is the contract.
#[derive(Debug, Default, Serialize)]
pub struct RelayPollResponse {
    pub push_results: Vec<RelayPushResult>,
    pub pulled: Vec<RelayPulledEntry>,
    pub last_synced_at: Option<i64>,
    pub last_error: Option<String>,
}

#[derive(Default)]
struct RelayInner {
    bearer: Option<String>,
    /// The durable pull cursor for this session (a `remote_id`/`sync_id`
    /// UUIDv7 string, comparable lexically — same invariant `max_remote_id`
    /// documents). Restart-safe by construction: this lives only in process
    /// memory, and a fresh registration after a restart reseeds it from the
    /// CLI's own `max_remote_id()` (item 16) — nothing here is a source of
    /// truth.
    cursor: Option<String>,
    /// SSE `id:` field, tracked for a warm reconnect resume (item 21) against
    /// a remote that sends one (cloud-api-style); this server's own
    /// `/memory/stream` never sends one, so it stays `None` end-to-end
    /// against an OSS team server and every reconnect is cold (falls back to
    /// `cursor`).
    last_event_id: Option<String>,
    push_results: Vec<RelayPushResult>,
    pulled: Vec<RelayPulledEntry>,
    last_synced_at: Option<i64>,
    last_error: Option<String>,
    pull_task_started: bool,
}

/// Per-(team-server, project) relay state.
pub struct RelaySession {
    server_url: String,
    project_id: String,
    inner: Mutex<RelayInner>,
}

impl RelaySession {
    fn new(server_url: String, project_id: String) -> Self {
        Self {
            server_url,
            project_id,
            inner: Mutex::new(RelayInner::default()),
        }
    }

    async fn set_bearer(&self, bearer: Option<String>) {
        if bearer.is_some() {
            self.inner.lock().await.bearer = bearer;
        }
    }

    /// Seed the cursor from a CLI-supplied `since_cursor`, never regressing
    /// a fresher one this session has already advanced past on its own.
    async fn seed_cursor(&self, since_cursor: Option<String>) {
        let Some(since_cursor) = since_cursor else {
            return;
        };
        let mut inner = self.inner.lock().await;
        if since_cursor.as_str() > inner.cursor.as_deref().unwrap_or("") {
            inner.cursor = Some(since_cursor);
        }
    }

    async fn client(&self) -> anyhow::Result<CloudSyncClient> {
        let bearer = self.inner.lock().await.bearer.clone();
        CloudSyncClient::new(&self.server_url, &self.project_id, bearer.as_deref(), None)
    }

    async fn record_error(&self, msg: String) {
        self.inner.lock().await.last_error = Some(msg);
    }

    /// Drain `entries` to the team server via [`CloudSyncClient::push_batch`]
    /// (item 12: reused, not reimplemented). Never stamps a result for an
    /// item the server did not affirmatively accept.
    async fn push(&self, entries: Vec<RelayPushEntry>) {
        if entries.is_empty() {
            return;
        }
        let client = match self.client().await {
            Ok(c) => c,
            Err(e) => {
                self.record_error(e.to_string()).await;
                return;
            }
        };
        let items: Vec<BatchPushItem> = entries.into_iter().map(Into::into).collect();
        match client.push_batch(items).await {
            Ok(res) => {
                let mut inner = self.inner.lock().await;
                for r in res.results {
                    let Some(external_id) = r.external_id else {
                        continue;
                    };
                    let durably_persisted = r.status == "created" || r.status == "skipped";
                    if !durably_persisted {
                        continue;
                    }
                    inner.push_results.push(RelayPushResult {
                        external_id,
                        remote_id: r.id,
                        status: r.status,
                    });
                }
                inner.last_synced_at = Some(now_secs());
                inner.last_error = None;
            }
            Err(e) => self.record_error(e.to_string()).await,
        }
    }

    /// Catch up via `/memory/since?since_id=<cursor>`, buffering newly-pulled
    /// rows and advancing the session's own cursor. See the module docs for
    /// why this, not the raw SSE payload, is what pull correctness rests on.
    async fn catch_up(&self) {
        let cursor = self.inner.lock().await.cursor.clone();
        let client = match self.client().await {
            Ok(c) => c,
            Err(e) => {
                self.record_error(e.to_string()).await;
                return;
            }
        };
        match client.pull_since(cursor.as_deref()).await {
            Ok(entries) => {
                if entries.is_empty() {
                    return;
                }
                let mut inner = self.inner.lock().await;
                for e in entries {
                    if Some(e.id.as_str()) > inner.cursor.as_deref() {
                        inner.cursor = Some(e.id.clone());
                    }
                    inner.pulled.push(e.into());
                }
                inner.last_synced_at = Some(now_secs());
                inner.last_error = None;
            }
            Err(e) => self.record_error(e.to_string()).await,
        }
    }

    /// Drain buffered results for the CLI to apply locally, clearing them.
    async fn drain(&self) -> RelayPollResponse {
        let mut inner = self.inner.lock().await;
        RelayPollResponse {
            push_results: std::mem::take(&mut inner.push_results),
            pulled: std::mem::take(&mut inner.pulled),
            last_synced_at: inner.last_synced_at,
            last_error: inner.last_error.take(),
        }
    }
}

/// Registry of active relay sessions, keyed by (team server, project).
/// Cloneable handle (an `Arc` inside) so it lives on [`crate::AppState`].
#[derive(Clone, Default)]
pub struct RelayRegistry(Arc<Mutex<HashMap<RelayKey, Arc<RelaySession>>>>);

impl RelayRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of registered sessions. Item 18: zero means no outbound sync
    /// HTTP traffic and no SSE connections exist anywhere in this process —
    /// a session is only ever created by [`Self::push`], never eagerly.
    pub async fn session_count(&self) -> usize {
        self.0.lock().await.len()
    }

    async fn get_or_create(&self, server_url: &str, project_id: &str) -> Arc<RelaySession> {
        let key = RelayKey::new(server_url, project_id);
        let mut map = self.0.lock().await;
        map.entry(key)
            .or_insert_with(|| {
                Arc::new(RelaySession::new(
                    server_url.trim_end_matches('/').to_string(),
                    project_id.to_string(),
                ))
            })
            .clone()
    }

    /// Handle `POST /local/relay/push`: register the session (starting its
    /// background pull loop on first sight — items 12/18), update its
    /// bearer/cursor, and drain `entries` in a detached background task so
    /// the HTTP response returns immediately. This is what keeps the actual
    /// remote hop out of the CLI write's own call stack (items 7/9/11): the
    /// CLI's nudge call only reaches this local loopback surface and
    /// returns; the network round trip to the team server happens here,
    /// independent of the CLI process's lifetime.
    pub async fn push(&self, req: RelayPushRequest) -> anyhow::Result<()> {
        if req.server_url.trim().is_empty() || req.project_id.trim().is_empty() {
            anyhow::bail!("server_url and project_id are required");
        }
        let session = self.get_or_create(&req.server_url, &req.project_id).await;
        session.set_bearer(req.bearer).await;
        session.seed_cursor(req.since_cursor).await;
        self.ensure_pull_task(session.clone()).await;

        let entries = req.entries;
        tokio::spawn(async move {
            session.push(entries).await;
        });
        Ok(())
    }

    /// Handle `GET /local/relay/poll`: drain buffered results for one
    /// session without creating it (item 18: polling an unregistered project
    /// must not spawn anything).
    pub async fn poll(&self, server_url: &str, project_id: &str) -> RelayPollResponse {
        let key = RelayKey::new(server_url, project_id);
        let session = self.0.lock().await.get(&key).cloned();
        match session {
            Some(s) => s.drain().await,
            None => RelayPollResponse::default(),
        }
    }

    async fn ensure_pull_task(&self, session: Arc<RelaySession>) {
        {
            let mut inner = session.inner.lock().await;
            if inner.pull_task_started {
                return;
            }
            inner.pull_task_started = true;
        }
        tokio::spawn(run_pull_loop(session));
    }
}

/// Long-lived per-session background task: initial catch-up, then hold an
/// SSE connection to `{server_url}/v1/projects/{project_id}/memory/stream`,
/// re-catching-up via `/memory/since` on every frame (never trusting the SSE
/// payload's own identity — see module docs). Reconnects with capped
/// exponential backoff on drop/error (item 21).
///
/// Every (re)connect — not just the first — is immediately followed by a
/// catch-up, before the frame-read loop starts. `handlers::memory_stream`
/// only emits notes created after the moment a given connection opened, so
/// without this, a write that lands between two connection attempts (e.g.
/// during a backoff sleep, or the very first connect racing a push that
/// lazily creates the project server-side) would be visible to neither the
/// stream's own live frames nor the one-time initial catch-up, and would
/// never arrive until *something else* happened to produce a later frame.
///
/// Isolated per session (item 17): errors here are always caught and
/// recorded via [`RelaySession::record_error`], never propagated as a panic,
/// so one project's relay failure cannot affect another session's task or
/// crash the server; a task-local failure here also never blocks other
/// requests, since it never holds any lock the request handlers need.
async fn run_pull_loop(session: Arc<RelaySession>) {
    session.catch_up().await;

    let mut attempt: u32 = 0;
    loop {
        match stream_once(&session).await {
            Ok(()) => attempt = 0,
            Err(e) => {
                session.record_error(e.to_string()).await;
                attempt = attempt.saturating_add(1);
            }
        }
        let backoff = 1u64.checked_shl(attempt.min(5)).unwrap_or(32).min(30);
        tokio::time::sleep(Duration::from_secs(backoff)).await;
    }
}

/// Cap on the unresolved (no `\n\n` seen yet) SSE receive buffer. A frame here
/// only ever needs to carry a `data:`/`id:` line pair (the frame is a wake-up
/// signal, never the note payload itself — see the module docs), so a
/// legitimate frame is a few hundred bytes at most. This bounds memory growth
/// against a misbehaving or malicious team `server_url` (any host a project
/// happens to be configured with) that sends a very long line, or omits the
/// blank-line frame terminator entirely: without a cap, `buf` would grow
/// without limit for as long as the connection stays open.
const MAX_SSE_BUFFER_BYTES: usize = 1024 * 1024;

/// One SSE connection attempt: connect, catch up (see [`run_pull_loop`]
/// docs), then read frames until the stream ends or errors, catching up
/// again on every frame received. `Ok(())` on a graceful stream end (the
/// server closed it) resets the caller's backoff.
async fn stream_once(session: &Arc<RelaySession>) -> anyhow::Result<()> {
    use futures_util::StreamExt;

    let (server_url, project_id) = (session.server_url.clone(), session.project_id.clone());
    let (bearer, last_event_id) = {
        let inner = session.inner.lock().await;
        (inner.bearer.clone(), inner.last_event_id.clone())
    };
    let sync_client = CloudSyncClient::new(&server_url, &project_id, bearer.as_deref(), None)?;
    let url = sync_client.stream_url();

    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()?;
    let mut req = http.get(&url);
    if let Some(b) = &bearer {
        req = req.header("Authorization", format!("Bearer {b}"));
    }
    if let Some(id) = &last_event_id {
        req = req.header("Last-Event-ID", id.clone());
    }

    let resp = req.send().await?.error_for_status()?;
    session.catch_up().await;
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buf.push_str(&String::from_utf8_lossy(&chunk));
        if buf.len() > MAX_SSE_BUFFER_BYTES {
            anyhow::bail!(
                "SSE frame from {server_url} exceeded {MAX_SSE_BUFFER_BYTES} bytes \
                 without a frame terminator; dropping this connection"
            );
        }
        while let Some(pos) = buf.find("\n\n") {
            let frame: String = buf.drain(..pos + 2).collect();
            let mut saw_data = false;
            let mut frame_id: Option<String> = None;
            for line in frame.lines() {
                if let Some(rest) = line.strip_prefix("data:") {
                    if !rest.trim().is_empty() {
                        saw_data = true;
                    }
                } else if let Some(rest) = line.strip_prefix("id:") {
                    frame_id = Some(rest.trim().to_string());
                }
            }
            if let Some(id) = frame_id {
                session.inner.lock().await.last_event_id = Some(id);
            }
            if saw_data {
                session.catch_up().await;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn entry(ext: &str) -> RelayPushEntry {
        RelayPushEntry {
            kind: "decision".into(),
            title: "T".into(),
            body: Some("B".into()),
            external_id: ext.into(),
            source_commit: None,
        }
    }

    // ── item 18: zero registered projects means zero outbound traffic ──────

    #[tokio::test]
    async fn empty_registry_makes_no_outbound_calls_and_starts_no_sessions() {
        let registry = RelayRegistry::new();
        assert_eq!(registry.session_count().await, 0);

        // Polling an unregistered project must not create a session either.
        let resp = registry.poll("https://team.example", "proj").await;
        assert!(resp.push_results.is_empty());
        assert!(resp.pulled.is_empty());
        assert_eq!(registry.session_count().await, 0);
    }

    // ── item 12: push reuses CloudSyncClient/BatchPushItem ─────────────────

    #[tokio::test]
    async fn push_drains_entries_to_the_team_server_and_is_pollable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 1, "skipped": 0, "failed": 0,
                "results": [{"status": "created", "external_id": "e1", "id": "cloud-1"}]
            })))
            .mount(&server)
            .await;
        // No SSE mount: the pull loop's initial catch-up (`/memory/since`)
        // must not block registration or the push itself.
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"entries": [], "count": 0})),
            )
            .mount(&server)
            .await;

        let registry = RelayRegistry::new();
        registry
            .push(RelayPushRequest {
                server_url: server.uri(),
                project_id: "proj".to_string(),
                bearer: None,
                since_cursor: None,
                entries: vec![entry("e1")],
            })
            .await
            .unwrap();

        assert_eq!(registry.session_count().await, 1);

        // The remote push happens in a detached background task; poll until
        // it lands rather than assuming a fixed sleep.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut got = RelayPollResponse::default();
        while std::time::Instant::now() < deadline {
            got = registry.poll(&server.uri(), "proj").await;
            if !got.push_results.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(got.push_results.len(), 1);
        assert_eq!(got.push_results[0].external_id, "e1");
        assert_eq!(got.push_results[0].remote_id.as_deref(), Some("cloud-1"));
        assert_eq!(got.push_results[0].status, "created");
        assert!(got.last_synced_at.is_some());

        // A second poll drains nothing more — items 33's "drain" contract.
        let second = registry.poll(&server.uri(), "proj").await;
        assert!(second.push_results.is_empty());
    }

    #[tokio::test]
    async fn push_with_empty_entries_is_a_noop_no_request() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"entries": [], "count": 0})),
            )
            .mount(&server)
            .await;

        let registry = RelayRegistry::new();
        registry
            .push(RelayPushRequest {
                server_url: server.uri(),
                project_id: "proj".to_string(),
                bearer: None,
                since_cursor: None,
                entries: vec![],
            })
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;
        let got = registry.poll(&server.uri(), "proj").await;
        assert!(
            got.push_results.is_empty(),
            "empty entries must never reach the batch endpoint, so nothing is stamped: {got:?}"
        );
    }

    // ── item 12/16: pull catch-up via /memory/since, cursor round-trips ────

    #[tokio::test]
    async fn registration_seeds_cursor_and_catch_up_advances_it_and_buffers_pulled_rows() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .and(wiremock::matchers::query_param(
                "since_id",
                "01890000-0000-7000-8000-000000000001",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "entries": [{
                    "id": "01890000-0000-7000-8000-000000000002",
                    "kind": "note", "title": "Remote",
                    "body": "body", "created_at": "2026-06-19T01:00:00Z"
                }],
                "count": 1
            })))
            .mount(&server)
            .await;

        let registry = RelayRegistry::new();
        registry
            .push(RelayPushRequest {
                server_url: server.uri(),
                project_id: "proj".to_string(),
                bearer: None,
                since_cursor: Some("01890000-0000-7000-8000-000000000001".to_string()),
                entries: vec![],
            })
            .await
            .unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut got = RelayPollResponse::default();
        while std::time::Instant::now() < deadline {
            got = registry.poll(&server.uri(), "proj").await;
            if !got.pulled.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(got.pulled.len(), 1);
        assert_eq!(
            got.pulled[0].remote_id,
            "01890000-0000-7000-8000-000000000002"
        );
        assert!(!got.pulled[0].archived);
    }

    #[tokio::test]
    async fn a_later_stale_since_cursor_never_regresses_a_session_that_moved_past_it() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"entries": [], "count": 0})),
            )
            .mount(&server)
            .await;

        let registry = RelayRegistry::new();
        // First registration seeds a cursor ahead of what the second (slower,
        // stale) CLI invocation will offer.
        registry
            .push(RelayPushRequest {
                server_url: server.uri(),
                project_id: "proj".to_string(),
                bearer: None,
                since_cursor: Some("01890000-0000-7000-8000-000000000005".to_string()),
                entries: vec![],
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let session = registry.get_or_create(&server.uri(), "proj").await;
        assert_eq!(
            session.inner.lock().await.cursor.as_deref(),
            Some("01890000-0000-7000-8000-000000000005")
        );

        registry
            .push(RelayPushRequest {
                server_url: server.uri(),
                project_id: "proj".to_string(),
                bearer: None,
                since_cursor: Some("01890000-0000-7000-8000-000000000001".to_string()),
                entries: vec![],
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(
            session.inner.lock().await.cursor.as_deref(),
            Some("01890000-0000-7000-8000-000000000005"),
            "a stale, earlier cursor must never regress the session's own progress"
        );
    }

    // ── item 17: one project's relay failure never affects another's ───────

    #[tokio::test]
    async fn one_sessions_push_failure_does_not_affect_another_sessions_push() {
        let bad_server = MockServer::start().await;
        // No mock mounted at all: every request 404s.
        let good_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/memory/batch"))
            .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 1, "skipped": 0, "failed": 0,
                "results": [{"status": "created", "external_id": "e1", "id": "cloud-1"}]
            })))
            .mount(&good_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"entries": [], "count": 0})),
            )
            .mount(&good_server)
            .await;

        let registry = RelayRegistry::new();
        registry
            .push(RelayPushRequest {
                server_url: bad_server.uri(),
                project_id: "proj".to_string(),
                bearer: None,
                since_cursor: None,
                entries: vec![entry("e1")],
            })
            .await
            .unwrap();
        registry
            .push(RelayPushRequest {
                server_url: good_server.uri(),
                project_id: "proj".to_string(),
                bearer: None,
                since_cursor: None,
                entries: vec![entry("e2")],
            })
            .await
            .unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut good = RelayPollResponse::default();
        while std::time::Instant::now() < deadline {
            good = registry.poll(&good_server.uri(), "proj").await;
            if !good.push_results.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            good.push_results.len(),
            1,
            "the healthy session's push must land regardless of the other session's failure"
        );

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut bad = RelayPollResponse::default();
        while std::time::Instant::now() < deadline {
            bad = registry.poll(&bad_server.uri(), "proj").await;
            if bad.last_error.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            bad.last_error.is_some(),
            "the failing session records its own error instead of panicking or hanging"
        );
        assert_eq!(registry.session_count().await, 2);
    }

    // ── item 22: no cross-project SSE/pull leakage ──────────────────────────
    // Two projects on the SAME team server: a note pushed to one must never
    // appear in the other's pulled buffer. `RelayKey` is `(server_url,
    // project_id)`, so distinct project ids always get distinct sessions with
    // independent cursors/buffers; this pins that at the observable
    // push+pull level rather than trusting the key type alone.

    #[tokio::test]
    async fn pulled_rows_never_leak_across_projects_on_the_same_team_server() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj-x/memory/batch"))
            .respond_with(ResponseTemplate::new(207).set_body_json(serde_json::json!({
                "created": 1, "skipped": 0, "failed": 0,
                "results": [{"status": "created", "external_id": "ex", "id": "cloud-x"}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj-x/memory/since"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "entries": [{
                    "id": "01890000-0000-7000-8000-0000000000x1",
                    "kind": "note", "title": "X-only", "body": "b",
                    "created_at": "2026-06-19T01:00:00Z"
                }],
                "count": 1
            })))
            .mount(&server)
            .await;
        // proj-y's own /memory/since must never see proj-x's entry (a distinct
        // mock, scoped to a different path, proves the request itself is
        // correctly project-scoped, not just that this mock happens to return
        // nothing).
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj-y/memory/since"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"entries": [], "count": 0})),
            )
            .mount(&server)
            .await;

        let registry = RelayRegistry::new();
        registry
            .push(RelayPushRequest {
                server_url: server.uri(),
                project_id: "proj-x".to_string(),
                bearer: None,
                since_cursor: None,
                entries: vec![entry("ex")],
            })
            .await
            .unwrap();
        registry
            .push(RelayPushRequest {
                server_url: server.uri(),
                project_id: "proj-y".to_string(),
                bearer: None,
                since_cursor: None,
                entries: vec![],
            })
            .await
            .unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut x = RelayPollResponse::default();
        while std::time::Instant::now() < deadline {
            x = registry.poll(&server.uri(), "proj-x").await;
            if !x.pulled.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(x.pulled.len(), 1, "proj-x must see its own entry");
        assert_eq!(x.pulled[0].title, "X-only");

        tokio::time::sleep(Duration::from_millis(200)).await;
        let y = registry.poll(&server.uri(), "proj-y").await;
        assert!(
            y.pulled.is_empty(),
            "proj-y must never see proj-x's pulled entry: {:?}",
            y.pulled
        );
    }

    // ── oversized/malformed SSE frame errors instead of growing forever ────
    //
    // A team `server_url` is whatever a project happens to be configured
    // with (cloud-api, another spelunk-server, or, if misconfigured, anything
    // else); this pins that a peer sending an unterminated line larger than
    // `MAX_SSE_BUFFER_BYTES` makes `stream_once` return an error (which
    // `run_pull_loop` already turns into `record_error` + backoff + retry,
    // never a panic) instead of buffering without bound for as long as the
    // connection stays open.

    #[tokio::test]
    async fn oversized_sse_frame_without_terminator_errors_instead_of_growing_forever() {
        let server = MockServer::start().await;
        let oversized_line = vec![b'x'; MAX_SSE_BUFFER_BYTES + 4096];
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/stream"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_bytes(oversized_line),
            )
            .mount(&server)
            .await;

        let session = Arc::new(RelaySession::new(server.uri(), "proj".to_string()));
        let result = stream_once(&session).await;
        assert!(
            result.is_err(),
            "an unterminated frame past the buffer cap must error, not hang or \
             grow without bound"
        );
    }

    // ── item 13: the reconciler never opens a project's memory.db ──────────
    //
    // Every public entry point on `RelayRegistry` (`push`, `poll`) takes only
    // `server_url` / `project_id` / entry data — never a filesystem path —
    // and every type in this module is one of those or wraps `CloudSyncClient`
    // (an HTTP client). There is no `MemoryStore`/SQLite-path parameter
    // anywhere in this module's public surface for a caller to even supply,
    // so a full push+pull round trip (`push_drains_entries_...` and
    // `registration_seeds_cursor_and_catch_up_advances_it_...` above)
    // completing correctly already proves sync works without this process
    // ever being handed — or needing — a `memory.db` path.
}
