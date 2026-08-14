//! ADR-037 P2 local relay: `inkentry-server`'s *outbound-client* role.
//!
//! Distinct from this binary's *team-server-hosting* role (the `/memory`,
//! `/memory/batch`, `/memory/since`, SSE `/memory/stream` routes backed by
//! `ServerDb`/`server.db`, see `handlers.rs`): here the same process instead
//! acts as a local, per-machine relay for a CLI's own `memory.db` outbox
//! against whatever team `server_url` a project is configured with (cloud-api
//! or another `inkentry-server`). Do not conflate the two roles or extend the
//! wrong routes for a P2 change.
//!
//! D5 (ADR-037): this module drains the outbox and holds the pull-catchup
//! network legs only. **It never opens a project's `memory.db`** — there is
//! no such import anywhere in this file, by construction; CLI-side storage
//! code (`crates/inkentry-cli/src/cli/cmd/memory/outbox.rs`) stays the sole
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
//!
//! ## Why the request never picks the destination
//!
//! This module is the daemon's only *outbound* surface, which makes its
//! destination a capability rather than a parameter: a `server_url` read out of
//! a request body would let any process that can reach loopback make the daemon
//! connect to a host of its choosing, from the daemon's network position,
//! carrying a bearer of its choosing, retried for as long as the daemon lives.
//! Every destination therefore comes from [`RelayPolicy`], which resolves it
//! from local configuration; a request may only select among the pairs this
//! machine already declares. See `policy.rs`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use inkentry_core::config::TeamTarget;
use inkentry_core::storage::{BatchPushItem, CloudSyncClient, RemoteEntry};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

mod policy;

pub use policy::RelayPolicy;

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
/// text-only (the vector fast path stays manual-`inkentry sync`-only, which
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

/// Cap on live sessions. [`RelayPolicy`] already bounds the key space to the
/// pairs local configuration declares, which is a handful on a real machine;
/// this is the backstop that keeps the bound a property of this module rather
/// than of whatever config happens to be on disk. Each session costs a
/// long-lived task, an HTTP client and up to
/// [`MAX_BUFFERED_ITEMS_PER_SESSION`] buffered rows, so an uncapped registry
/// was a memory/task-exhaustion primitive.
const MAX_RELAY_SESSIONS: usize = 32;

/// How long a session may go without a single CLI call (`push`/`poll`/`ack`)
/// before it is retired: its pull loop returns and it is dropped from the
/// registry. Nothing else ends that loop — it reconnects forever — so without
/// this every session ever registered lived as long as the daemon.
///
/// Sized well past the interval at which a CLI in use touches the relay (every
/// `memory` write and every read that polls), so an idle session means the
/// project really is idle, not merely between commands.
const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// What `last_error` reports for any failure of the remote hop. The underlying
/// `reqwest` error distinguishes connection-refused from timed-out from
/// TLS-failed per host and port, and this field is readable by any local
/// process: reporting it verbatim turned the relay into a network probe with an
/// oracle. The detail goes to the daemon log, which is the operator's.
const REMOTE_HOP_FAILED: &str =
    "sync with the configured team server failed; see `inkentry server logs` for details";

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
    /// When a CLI last called `push`/`poll`/`ack` for this session. Drives
    /// retirement (see [`SESSION_IDLE_TIMEOUT`]); the session's own background
    /// traffic deliberately does not refresh it, or a session whose team server
    /// keeps emitting would never look idle no matter how long ago its CLI
    /// stopped.
    last_seen: Instant,
}

impl RelayInner {
    fn new() -> Self {
        Self {
            bearer: None,
            cursor: None,
            last_event_id: None,
            push_results: HashMap::new(),
            pulled: HashMap::new(),
            last_synced_at: None,
            last_error: None,
            pull_task_started: false,
            last_seen: Instant::now(),
        }
    }
}

/// Per-(team-server, project) relay state.
pub struct RelaySession {
    key: RelayKey,
    server_url: String,
    project_id: String,
    /// Custom CA trust anchor for this target, from the same local config that
    /// declared it. Threading it here is what makes background convergence work
    /// against an internal-CA team server; hardcoding `None` left `status`
    /// showing a permanent sync error with only manual `inkentry sync` working.
    server_ca: Option<PathBuf>,
    idle_timeout: Duration,
    inner: Mutex<RelayInner>,
}

impl RelaySession {
    fn new(key: RelayKey, target: &TeamTarget, idle_timeout: Duration) -> Self {
        Self {
            key,
            server_url: target.server_url.trim_end_matches('/').to_string(),
            project_id: target.project_id.clone(),
            server_ca: target.server_ca.clone(),
            idle_timeout,
            inner: Mutex::new(RelayInner::new()),
        }
    }

    /// Record CLI contact, holding off retirement.
    async fn touch(&self) {
        self.inner.lock().await.last_seen = Instant::now();
    }

    async fn idle_for(&self) -> Duration {
        self.inner.lock().await.last_seen.elapsed()
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
        CloudSyncClient::new(
            &self.server_url,
            &self.project_id,
            bearer.as_deref(),
            self.server_ca.as_deref(),
        )
    }

    /// Log the real failure for the operator; report only
    /// [`REMOTE_HOP_FAILED`] to the caller.
    async fn record_error(&self, context: &str, err: impl std::fmt::Display) {
        tracing::warn!(
            server_url = %self.server_url,
            project_id = %self.project_id,
            "local relay {context}: {err}"
        );
        self.inner.lock().await.last_error = Some(REMOTE_HOP_FAILED.to_string());
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
                self.record_error("could not build its push client", e)
                    .await;
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
            Err(e) => self.record_error("push failed", e).await,
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
                self.record_error("could not build its pull client", e)
                    .await;
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
            Err(e) => self.record_error("catch-up failed", e).await,
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

/// Why a relay call was refused. Always a fixed string: it travels back over
/// the local HTTP surface, so it may describe the rule that was broken and
/// nothing about the remote host.
#[derive(Debug)]
pub struct RelayRefused(&'static str);

impl std::fmt::Display for RelayRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for RelayRefused {}

const REFUSED_DISABLED: &str =
    "the local relay is not available on this server (it is bound to a non-loopback address)";
const REFUSED_UNDECLARED: &str = "no team target for that server_url/project_id is declared by this machine's \
     local configuration; the relay only syncs projects configured in \
     `.inkentry/config.toml` (or INKENTRY_SERVER_URL/INKENTRY_PROJECT_ID)";
const REFUSED_AT_CAPACITY: &str = "the local relay is already tracking its maximum session count";

/// Registry of active relay sessions, keyed by (team server, project).
/// Cloneable handle (an `Arc` inside) so it lives on [`crate::AppState`].
#[derive(Clone)]
pub struct RelayRegistry(Arc<RegistryInner>);

struct RegistryInner {
    /// `false` disables the whole surface: every call is refused and
    /// [`crate::router`] does not mount the routes at all.
    enabled: bool,
    policy: RelayPolicy,
    idle_timeout: Duration,
    sessions: Mutex<HashMap<RelayKey, Arc<RelaySession>>>,
}

impl RelayRegistry {
    /// A registry for a daemon bound to `host`. The relay surface is
    /// documented local-only and is unauthenticated on the auto-spawned
    /// daemon, so a non-loopback bind gets no relay at all — structurally,
    /// not by asking each handler to check.
    pub fn for_bind(host: &str, policy: RelayPolicy) -> Self {
        if crate::host_is_loopback(host) {
            Self::new(policy)
        } else {
            Self::disabled()
        }
    }

    pub fn new(policy: RelayPolicy) -> Self {
        Self::with_idle_timeout(policy, SESSION_IDLE_TIMEOUT)
    }

    /// No relay: refuses every call and is not routed. What a non-loopback
    /// bind gets, and what a server embedding this crate gets by default.
    pub fn disabled() -> Self {
        Self(Arc::new(RegistryInner {
            enabled: false,
            policy: RelayPolicy::allowing(vec![]),
            idle_timeout: SESSION_IDLE_TIMEOUT,
            sessions: Mutex::new(HashMap::new()),
        }))
    }

    /// [`Self::new`] with an injectable idle timeout, so retirement can be
    /// exercised without waiting out [`SESSION_IDLE_TIMEOUT`].
    pub(crate) fn with_idle_timeout(policy: RelayPolicy, idle_timeout: Duration) -> Self {
        Self(Arc::new(RegistryInner {
            enabled: true,
            policy,
            idle_timeout,
            sessions: Mutex::new(HashMap::new()),
        }))
    }

    /// Whether this daemon serves the local relay routes at all.
    pub fn is_enabled(&self) -> bool {
        self.0.enabled
    }

    /// Number of registered sessions. Item 18: zero means no outbound sync
    /// HTTP traffic and no SSE connections exist anywhere in this process —
    /// a session is only ever created by [`Self::push`], never eagerly.
    pub async fn session_count(&self) -> usize {
        self.0.sessions.lock().await.len()
    }

    /// Resolve a requested pair to a locally-declared target, or refuse. The
    /// only path by which a `server_url` becomes a destination.
    fn resolve(&self, server_url: &str, project_id: &str) -> Result<TeamTarget, RelayRefused> {
        if !self.0.enabled {
            return Err(RelayRefused(REFUSED_DISABLED));
        }
        let target = self
            .0
            .policy
            .resolve(server_url.trim(), project_id.trim())
            .ok_or(RelayRefused(REFUSED_UNDECLARED))?;
        // Belt to `CloudSyncClient`'s braces: a declared target still may not be
        // a plaintext non-loopback URL, and refusing here means the misconfigured
        // project never gets a session or a pull loop in the first place.
        if inkentry_core::config::validate_transport_url(&target.server_url).is_err() {
            return Err(RelayRefused(
                "the configured server_url for this project is plaintext http:// to a \
                 non-loopback host; use https://",
            ));
        }
        Ok(target)
    }

    /// Finds-or-creates and touches the session under the *same* sessions-map
    /// lock acquisition. [`Self::retire_if_idle`] also locks this map before
    /// rechecking `idle_for()`, so touching after releasing this lock (the
    /// old shape) left a gap where retirement could see a still-stale
    /// `last_seen` and remove a session this call had just found or created,
    /// orphaning the `Arc` the caller was about to use.
    async fn session_for(&self, target: &TeamTarget) -> Result<Arc<RelaySession>, RelayRefused> {
        let key = RelayKey::new(&target.server_url, &target.project_id);
        let mut map = self.0.sessions.lock().await;
        if let Some(existing) = map.get(&key) {
            let existing = existing.clone();
            existing.touch().await;
            return Ok(existing);
        }
        if map.len() >= MAX_RELAY_SESSIONS {
            return Err(RelayRefused(REFUSED_AT_CAPACITY));
        }
        let session = Arc::new(RelaySession::new(key.clone(), target, self.0.idle_timeout));
        session.touch().await;
        map.insert(key, session.clone());
        Ok(session)
    }

    /// Finds and touches the session under the same sessions-map lock
    /// acquisition, for the same reason [`Self::session_for`] does: it must
    /// never hand back a session that a racing [`Self::retire_if_idle`] can
    /// still remove on a stale observation.
    async fn lookup(&self, server_url: &str, project_id: &str) -> Option<Arc<RelaySession>> {
        let key = RelayKey::new(server_url.trim(), project_id.trim());
        let map = self.0.sessions.lock().await;
        let session = map.get(&key)?.clone();
        session.touch().await;
        Some(session)
    }

    /// Drop a session that has gone idle, reporting whether it was actually
    /// removed. Re-checks liveness while holding the map lock, so a CLI call
    /// racing the retirement keeps the session its request just touched.
    async fn retire_if_idle(&self, session: &Arc<RelaySession>) -> bool {
        let mut map = self.0.sessions.lock().await;
        if session.idle_for().await < self.0.idle_timeout {
            return false;
        }
        map.remove(&session.key);
        true
    }

    /// Handle `POST /local/relay/push`: register the session (starting its
    /// background pull loop on first sight — items 12/18), update its
    /// bearer/cursor, and drain `entries` in a detached background task so
    /// the HTTP response returns immediately. This is what keeps the actual
    /// remote hop out of the CLI write's own call stack (items 7/9/11): the
    /// CLI's nudge call only reaches this local loopback surface and
    /// returns; the network round trip to the team server happens here,
    /// independent of the CLI process's lifetime.
    ///
    /// The request's `server_url` selects a target; it never becomes one. See
    /// [`Self::resolve`] and the module docs.
    pub async fn push(&self, req: RelayPushRequest) -> Result<(), RelayRefused> {
        let target = self.resolve(&req.server_url, &req.project_id)?;
        let session = self.session_for(&target).await?;
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
        match self.lookup(server_url, project_id).await {
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
        if let Some(s) = self.lookup(server_url, project_id).await {
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
        tokio::spawn(run_pull_loop(self.clone(), session));
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
///
/// The loop ends when its session goes idle for [`SESSION_IDLE_TIMEOUT`],
/// which also drops the session from `registry`. It previously had no
/// termination condition at all: a session, once created, held a task, a
/// client and its buffers for the daemon's lifetime, reconnecting to its team
/// server forever whether or not any CLI still cared.
async fn run_pull_loop(registry: RelayRegistry, session: Arc<RelaySession>) {
    session.catch_up().await;

    let mut attempt: u32 = 0;
    loop {
        // `biased` so an already-idle session is retired without first opening
        // another connection.
        tokio::select! {
            biased;
            _ = wait_until_idle(&session) => {
                if registry.retire_if_idle(&session).await {
                    tracing::debug!(
                        server_url = %session.server_url,
                        project_id = %session.project_id,
                        "retiring idle local relay session"
                    );
                    return;
                }
                continue;
            }
            outcome = stream_once(&session) => match outcome {
                Ok(()) => attempt = 0,
                Err(e) => {
                    session.record_error("stream connection failed", e).await;
                    attempt = attempt.saturating_add(1);
                }
            },
        }
        let backoff = 1u64.checked_shl(attempt.min(5)).unwrap_or(32).min(30);
        tokio::select! {
            biased;
            _ = wait_until_idle(&session) => {
                if registry.retire_if_idle(&session).await {
                    return;
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(backoff)) => {}
        }
    }
}

/// Resolves once the session has had no CLI contact for its idle timeout.
/// Sleeps exactly to the deadline and re-checks rather than polling, so a
/// session touched meanwhile simply extends the wait.
async fn wait_until_idle(session: &RelaySession) {
    loop {
        let remaining = session
            .idle_timeout
            .saturating_sub(session.idle_for().await);
        if remaining.is_zero() {
            return;
        }
        tokio::time::sleep(remaining).await;
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
    let sync_client = CloudSyncClient::new(
        &server_url,
        &project_id,
        bearer.as_deref(),
        session.server_ca.as_deref(),
    )?;
    let url = sync_client.stream_url();

    // The SSE leg needs its own client (`CloudSyncClient` is single-shot), so
    // it needs the custom CA applied here too — a team server behind an
    // internal CA otherwise pulls fine and never streams.
    let http = inkentry_core::config::apply_server_ca(
        reqwest::Client::builder().connect_timeout(Duration::from_secs(10)),
        session.server_ca.as_deref(),
    )?
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
mod tests;
