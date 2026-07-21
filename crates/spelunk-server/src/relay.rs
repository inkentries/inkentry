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

/// Body of `POST /local/relay/ack`: the CLI confirming which polled entries
/// it durably applied to `memory.db`, so the relay can retire exactly those
/// and keep offering the rest on the next poll. See [`RelaySession::poll`]'s
/// doc comment for why this handshake exists.
#[derive(Debug, Deserialize)]
pub struct RelayAckRequest {
    pub server_url: String,
    pub project_id: String,
    #[serde(default)]
    pub applied_push_external_ids: Vec<String>,
    #[serde(default)]
    pub applied_pull_remote_ids: Vec<String>,
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

/// Response body of `GET /local/relay/poll` (item 33: this state lives only
/// in this long-running process, surviving any single CLI invocation). A
/// poll is a **peek**, not a drain: the CLI applies the returned entries
/// locally, then confirms which ones it actually applied via
/// `POST /local/relay/ack` (see [`RelaySession::poll`] / [`RelaySession::ack`]).
/// A crashed or failed apply between poll and ack simply sees the same
/// entries again on the next poll.
#[derive(Debug, Default, Serialize)]
pub struct RelayPollResponse {
    pub push_results: Vec<RelayPushResult>,
    pub pulled: Vec<RelayPulledEntry>,
    pub last_synced_at: Option<i64>,
    pub last_error: Option<String>,
}

/// Cap on buffered-but-unconfirmed push results / pulled entries per session.
/// Same class of bound as [`MAX_SSE_BUFFER_BYTES`]: without one, a resident
/// server relaying for an active team while the local CLI never polls (or
/// polls but never confirms via [`RelaySession::ack`]) grows these without
/// limit. The maps below dedupe by identity, so this bounds the number of
/// distinct outstanding rows, not repeat re-fetches of the same one.
const MAX_BUFFERED_ITEMS_PER_SESSION: usize = 10_000;

#[derive(Default)]
struct RelayInner {
    bearer: Option<String>,
    /// The durable pull cursor for this session (a `remote_id`/`sync_id`
    /// UUIDv7 string, comparable lexically — same invariant `max_remote_id`
    /// documents). Restart-safe by construction: this lives only in process
    /// memory, and a fresh registration after a restart reseeds it from the
    /// CLI's own `max_remote_id()` (item 16) — nothing here is a source of
    /// truth. Only ever advanced past an entry that made it into `pulled`
    /// (see [`RelaySession::catch_up`]): advancing it past an entry dropped
    /// for being over [`MAX_BUFFERED_ITEMS_PER_SESSION`] would make that
    /// entry permanently unfetchable, the same class of loss this module's
    /// ack handshake exists to close.
    cursor: Option<String>,
    /// SSE `id:` field, tracked for a warm reconnect resume (item 21) against
    /// a remote that sends one (cloud-api-style); this server's own
    /// `/memory/stream` never sends one, so it stays `None` end-to-end
    /// against an OSS team server and every reconnect is cold (falls back to
    /// `cursor`).
    last_event_id: Option<String>,
    /// Keyed by `external_id`, not a `Vec`: dedupes repeated re-offers of the
    /// same still-unstamped row (the CLI re-offers every unpushed row on
    /// every nudge/poll registration until it is stamped) and gives
    /// [`RelaySession::ack`] a targeted removal instead of a full clear.
    push_results: HashMap<String, RelayPushResult>,
    /// Keyed by `remote_id`. See [`RelaySession::poll`] / [`RelaySession::ack`]
    /// for why this is no longer cleared on every poll.
    pulled: HashMap<String, RelayPulledEntry>,
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

    /// Push `entries` to the team server via [`CloudSyncClient::push_batch`]
    /// (item 12: reused, not reimplemented). Never stamps a result for an
    /// item the server did not affirmatively accept. Buffered results are
    /// keyed by `external_id` (dedupe: the CLI re-offers every still-unstamped
    /// row on every registration, so a slow-to-be-applied earlier result must
    /// not pile up duplicates) and are **not cleared here** — only
    /// [`Self::ack`] retires a buffered result, once the CLI confirms it
    /// durably applied it locally. This is the fix for the push-side half of
    /// the destructive-drain data-loss bug: the old `drain`-on-poll contract
    /// handed a result to the CLI and forgot it in the same call, so a local
    /// `set_remote_id` failure after a successful poll permanently stranded
    /// the row pending (a re-push of an already-persisted row comes back
    /// `skipped`, which may carry no id to stamp with).
    ///
    /// A later result for an `external_id` already buffered never regresses
    /// a known `remote_id` to `None`: since the row stays outbox-pending
    /// (`remote_id IS NULL`) locally until it is actually stamped, the CLI
    /// keeps re-offering it on every registration while an earlier result
    /// sits unacked, and a real team server's idempotent dedupe answers a
    /// repeat push with `skipped` — which may carry no id. Letting that
    /// overwrite an earlier `created`/`skipped` result that DID carry an id
    /// would reintroduce the exact "no id to stamp with" trap this buffer
    /// retention is meant to close.
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
                let mut buffer_full = false;
                for r in res.results {
                    let Some(external_id) = r.external_id else {
                        continue;
                    };
                    let durably_persisted = r.status == "created" || r.status == "skipped";
                    if !durably_persisted {
                        continue;
                    }
                    let at_capacity = inner.push_results.len() >= MAX_BUFFERED_ITEMS_PER_SESSION;
                    match inner.push_results.entry(external_id.clone()) {
                        std::collections::hash_map::Entry::Occupied(mut o) => {
                            if r.id.is_some() || o.get().remote_id.is_none() {
                                o.insert(RelayPushResult {
                                    external_id,
                                    remote_id: r.id,
                                    status: r.status,
                                });
                            }
                        }
                        std::collections::hash_map::Entry::Vacant(v) => {
                            if at_capacity {
                                buffer_full = true;
                                continue;
                            }
                            v.insert(RelayPushResult {
                                external_id,
                                remote_id: r.id,
                                status: r.status,
                            });
                        }
                    }
                }
                inner.last_synced_at = Some(now_secs());
                inner.last_error = if buffer_full {
                    Some(format!(
                        "local relay push-result buffer is full ({MAX_BUFFERED_ITEMS_PER_SESSION} \
                         unconfirmed rows); waiting for the CLI to poll and confirm before \
                         buffering more"
                    ))
                } else {
                    None
                };
            }
            Err(e) => self.record_error(e.to_string()).await,
        }
    }

    /// Catch up via `/memory/since?since_id=<cursor>`, buffering newly-pulled
    /// rows and advancing the session's own cursor. See the module docs for
    /// why this, not the raw SSE payload, is what pull correctness rests on.
    ///
    /// Buffered rows are keyed by `remote_id` and are **not cleared here** —
    /// see [`Self::push`]'s doc comment for why (the pull-side half of the
    /// same fix). The cursor only ever advances past an entry that actually
    /// made it into the buffer; an entry dropped for hitting
    /// [`MAX_BUFFERED_ITEMS_PER_SESSION`] is never counted as fetched, so a
    /// later catch-up (once the CLI has drained some of the buffer via
    /// [`Self::ack`]) re-offers it instead of skipping it forever.
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
                let mut buffer_full = false;
                for e in entries {
                    let id = e.id.clone();
                    let already_buffered = inner.pulled.contains_key(&id);
                    if !already_buffered {
                        if inner.pulled.len() >= MAX_BUFFERED_ITEMS_PER_SESSION {
                            buffer_full = true;
                            break;
                        }
                        inner.pulled.insert(id.clone(), e.into());
                    }
                    if Some(id.as_str()) > inner.cursor.as_deref() {
                        inner.cursor = Some(id);
                    }
                }
                inner.last_synced_at = Some(now_secs());
                inner.last_error = if buffer_full {
                    Some(format!(
                        "local relay pulled-entry buffer is full ({MAX_BUFFERED_ITEMS_PER_SESSION} \
                         unconfirmed rows); waiting for the CLI to poll and confirm before \
                         fetching further"
                    ))
                } else {
                    None
                };
            }
            Err(e) => self.record_error(e.to_string()).await,
        }
    }

    /// Snapshot buffered results for the CLI to apply locally. Non-destructive
    /// by design (renamed from the old `drain`): a poll used to hand back
    /// buffered state and clear it in the same call
    /// (`std::mem::take`), so a CLI-side apply failure *after* a successful
    /// poll permanently lost the row (pull) or stranded it pending forever
    /// (push). Only [`Self::ack`] — sent by the CLI after it confirms the
    /// local apply actually succeeded — retires an entry, so a failed or
    /// interrupted apply simply sees the same entry again on the next poll.
    async fn poll(&self) -> RelayPollResponse {
        let inner = self.inner.lock().await;
        RelayPollResponse {
            push_results: inner.push_results.values().cloned().collect(),
            pulled: inner.pulled.values().cloned().collect(),
            last_synced_at: inner.last_synced_at,
            last_error: inner.last_error.clone(),
        }
    }

    /// Retire buffered results the CLI has confirmed applying to `memory.db`.
    /// Anything not named here stays buffered for the next poll — see
    /// [`Self::poll`]'s doc comment.
    async fn ack(&self, applied_push_external_ids: &[String], applied_pull_remote_ids: &[String]) {
        let mut inner = self.inner.lock().await;
        for id in applied_push_external_ids {
            inner.push_results.remove(id);
        }
        for id in applied_pull_remote_ids {
            inner.pulled.remove(id);
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

    /// Handle `GET /local/relay/poll`: snapshot buffered results for one
    /// session without creating it (item 18: polling an unregistered project
    /// must not spawn anything). Non-destructive — see [`RelaySession::poll`].
    pub async fn poll(&self, server_url: &str, project_id: &str) -> RelayPollResponse {
        let key = RelayKey::new(server_url, project_id);
        let session = self.0.lock().await.get(&key).cloned();
        match session {
            Some(s) => s.poll().await,
            None => RelayPollResponse::default(),
        }
    }

    /// Handle `POST /local/relay/ack`: retire buffered results the CLI
    /// confirms it applied. A no-op for an unregistered session (nothing
    /// buffered to retire); never creates one.
    pub async fn ack(
        &self,
        server_url: &str,
        project_id: &str,
        applied_push_external_ids: &[String],
        applied_pull_remote_ids: &[String],
    ) {
        let key = RelayKey::new(server_url, project_id);
        let session = self.0.lock().await.get(&key).cloned();
        if let Some(s) = session {
            s.ack(applied_push_external_ids, applied_pull_remote_ids)
                .await;
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

/// Byte-offset of the first `"\n\n"` frame terminator in `buf`, if any.
/// Searched over raw bytes (not a decoded `&str`) so a not-yet-complete
/// multi-byte UTF-8 sequence at the end of `buf` can never cause a spurious
/// match or a decode error before a full frame has arrived.
fn find_double_newline(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\n\n")
}

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
    // Raw bytes, not a `String`: a chunk boundary from the underlying HTTP
    // stream has no relation to a UTF-8 character boundary or a frame
    // boundary. Decoding each chunk in isolation (the previous
    // `String::from_utf8_lossy(&chunk)` per iteration) could corrupt a
    // multi-byte character, or a `Last-Event-ID` value, split across two
    // chunks — decoding only happens below, once a complete frame (delimited
    // by `\n\n`) has been assembled from raw bytes.
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buf.extend_from_slice(&chunk);
        if buf.len() > MAX_SSE_BUFFER_BYTES {
            anyhow::bail!(
                "SSE frame from {server_url} exceeded {MAX_SSE_BUFFER_BYTES} bytes \
                 without a frame terminator; dropping this connection"
            );
        }
        while let Some(pos) = find_double_newline(&buf) {
            let frame_bytes: Vec<u8> = buf.drain(..pos + 2).collect();
            let frame = String::from_utf8_lossy(&frame_bytes);
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
    async fn push_lands_on_the_team_server_and_is_pollable_and_reoffered_until_acked() {
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

        // A second poll before any ack must return the SAME result again —
        // this is the fix for the destructive-drain data-loss bug: a poll
        // used to clear the buffer in the same call, so a CLI-side apply
        // failure after this first poll would have permanently stranded the
        // row pending forever (nothing left to retry against).
        let second = registry.poll(&server.uri(), "proj").await;
        assert_eq!(
            second.push_results.len(),
            1,
            "an unacked result must still be offered on the next poll"
        );
        assert_eq!(second.push_results[0].external_id, "e1");

        // Only an explicit ack retires it.
        registry
            .ack(&server.uri(), "proj", &["e1".to_string()], &[])
            .await;
        let third = registry.poll(&server.uri(), "proj").await;
        assert!(
            third.push_results.is_empty(),
            "an acked result must not be offered again"
        );
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

    // ── founder review (PR #728): pull-side data loss without a restart ────
    //
    // The bug: `GET /local/relay/poll` used to destructively drain buffered
    // pulled rows (`std::mem::take`) while the CLI's `apply_remote_note` call
    // can fail (SQLITE_BUSY, a killed process) without re-buffering — and the
    // session's pull cursor had already advanced past the row when it was
    // first buffered, so a restart-free retry would never re-offer it. This
    // pins the fix directly at the relay level, independent of any CLI-side
    // failure injection: a poll never clears the buffer by itself, so a CLI
    // that never acks (modelling "poll succeeded, the local apply after it
    // failed") must see the exact same row again, indefinitely, across many
    // polls and additional catch-up cycles — never silently dropped. Fails
    // against the pre-fix `drain`-on-poll code (the second poll below would
    // return empty), passes after.

    #[tokio::test]
    async fn a_pulled_row_survives_repeated_polls_when_the_cli_never_acks_it() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/projects/proj/memory/since"))
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
                since_cursor: None,
                entries: vec![],
            })
            .await
            .unwrap();

        // Wait for the initial catch-up to buffer the row.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if !registry.poll(&server.uri(), "proj").await.pulled.is_empty() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the row never arrived to begin with"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Simulate "the CLI polled it, but its local apply failed" by simply
        // never acking, across several more polls (each of which also lets
        // the background pull loop run another catch-up cycle against a
        // cursor that must not have moved past this still-unacked row).
        for _ in 0..5 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let got = registry.poll(&server.uri(), "proj").await;
            assert_eq!(
                got.pulled.len(),
                1,
                "an unacked pulled row must never disappear from the buffer"
            );
            assert_eq!(
                got.pulled[0].remote_id,
                "01890000-0000-7000-8000-000000000002"
            );
        }

        // Once the CLI confirms it actually applied the row, it is retired.
        registry
            .ack(
                &server.uri(),
                "proj",
                &[],
                &["01890000-0000-7000-8000-000000000002".to_string()],
            )
            .await;
        let after_ack = registry.poll(&server.uri(), "proj").await;
        assert!(
            after_ack.pulled.is_empty(),
            "an acked pulled row must not be offered again"
        );
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

    // ── founder review (PR #728): SSE frames decode across chunk boundaries ─
    //
    // `stream_once` used to decode each raw HTTP chunk in isolation
    // (`String::from_utf8_lossy(&chunk)` per iteration, before frame
    // boundaries were known), which could corrupt a multi-byte UTF-8
    // character or a `Last-Event-ID` value split across two chunks. The fix
    // accumulates raw bytes and only decodes once a complete `\n\n`-
    // terminated frame has been assembled. This pins the byte-safety
    // primitive the fix relies on: `find_double_newline` must locate the
    // terminator by raw bytes, never by decoding (which would panic or
    // silently corrupt data on a not-yet-complete multi-byte sequence sitting
    // at the search boundary).

    #[test]
    fn find_double_newline_locates_the_terminator_around_a_multibyte_char() {
        // "café" — 'é' is the two-byte UTF-8 sequence 0xC3 0xA9. Split the
        // buffer such that this sequence itself sits right before the
        // terminator, the exact shape a chunk-boundary split could produce.
        let mut buf = b"data: caf\xc3\xa9\n\n".to_vec();
        let pos = find_double_newline(&buf).expect("terminator must be found");
        let frame = String::from_utf8_lossy(&buf[..pos + 2]).into_owned();
        assert_eq!(
            frame, "data: café\n\n",
            "the multibyte character must decode intact"
        );

        // No terminator yet (a chunk boundary landed mid-frame, even mid-
        // character): must not find a false match or panic on invalid UTF-8
        // in the not-yet-complete tail.
        buf.truncate(buf.len() - 2); // drop the "\n\n"
        assert_eq!(find_double_newline(&buf), None);
        let mid_char = &buf[..buf.len() - 1]; // split inside 'é''s 2-byte sequence
        assert_eq!(find_double_newline(mid_char), None);
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
