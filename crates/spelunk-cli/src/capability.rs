//! Capability tier detection for the spelunk CLI.
//!
//! Tier 0 (Offline): no server_url configured, or server unreachable.
//! Tier 1 (Server):  server_url set and GET /v1/health succeeds.
//!
//! ## Loopback auto-discovery (spelunk#316 / 0.8.0)
//!
//! When `cfg.server_url` is `None` **and** `SPELUNK_NO_SERVER` is not set, the probe
//! attempts to reach a locally-running spelunk-server before falling through to
//! `Tier::Offline`:
//!
//! 1. Read `~/.local/state/spelunk/server.port` (written by `spelunk server start`);
//!    use `http://127.0.0.1:<port>` if the file exists.
//! 2. Otherwise probe `http://127.0.0.1:7777` with a **250 ms** timeout (distinct from
//!    the 2 s timeout used for explicitly-configured remote URLs).
//! 3. On success, treat as `Tier::Server` with `auto_discovered = true`.
//! 4. On failure, return `Tier::Offline`.
//!
//! `SPELUNK_NO_SERVER=1` short-circuits all loopback probing and forces `Tier::Offline`.
//!
//! The probe runs lazily on the first call that needs Tier 1 and its result
//! is cached for the process lifetime.

use serde::{Deserialize, Serialize};
use tokio::sync::OnceCell;

use crate::config::Config;

/// State file directory: `~/.local/state/spelunk/`.
///
/// On all platforms we use `~/.local/state` rather than the OS-native state dir.
/// This mirrors the deliberate choice made for the config dir
/// (`spelunk_config_dir` in spelunk-core's `config.rs`, which uses `~/.config`
/// on every platform): it keeps the path identical across Linux and macOS, and
/// matches what the CLI documentation and error messages say.
///
/// It also sidesteps a concrete portability bug: `dirs::state_dir()` returns
/// `None` on macOS (dirs v6 has no XDG_STATE_HOME equivalent there), which
/// silently disabled loopback auto-discovery on the primary dev platform
/// (spelunk#316). Returns `None` only when the home directory can't be resolved.
///
/// NOTE for spelunk#317 (writer side, `spelunk server start`): the writer MUST
/// write `server.port` into this exact directory so reader and writer agree.
/// Use the same `~/.local/state/spelunk/` path on every platform.
///
/// `SPELUNK_STATE_DIR` overrides the entire path. Useful in tests and on
/// Windows CI where `dirs::home_dir()` 6.x calls `SHGetKnownFolderPath` (a
/// Windows Registry lookup) rather than reading `USERPROFILE`, making
/// per-process environment overrides ineffective.
fn spelunk_state_dir() -> Option<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("SPELUNK_STATE_DIR") {
        return Some(std::path::PathBuf::from(p));
    }
    dirs::home_dir().map(|home| home.join(".local").join("state").join("spelunk"))
}

/// Read the port written by `spelunk server start` into
/// `~/.local/state/spelunk/server.port`. Returns `None` if absent or unreadable.
fn read_server_port_file() -> Option<u16> {
    let path = spelunk_state_dir()?.join("server.port");
    let content = std::fs::read_to_string(&path).ok()?;
    content.trim().parse::<u16>().ok()
}

static TIER: OnceCell<Tier> = OnceCell::const_new();

/// Server-side embedder readiness, mirrored from the `/v1/health` `embedder.state`
/// field (spelunk-oss^50 PR A). The CLI uses this to distinguish, when semantic
/// search is unavailable, between "no server reachable", "server up but the model
/// is still warming up", and "the model failed to load" — so it can print an
/// actionable one-line notice rather than silently degrading (task item #5).
///
/// Serialized lowercase to match the server's health body and to feed
/// `spelunk status --format json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EmbedderState {
    /// Native embedder build/download in progress — not ready yet, keep polling.
    Loading,
    /// Model loaded; embed endpoints will serve.
    Ready,
    /// Background load failed (download error, OOM, …). Terminal for that process.
    Unavailable,
    /// Server started with no in-process model to load (external embedding URL,
    /// or no embedder feature). Treated as ready.
    Disabled,
    /// Field absent from the health body (server pre-dates PR A). Unknown state.
    #[default]
    Unknown,
}

impl EmbedderState {
    /// Lowercase wire string (matches the server's `embedder.state` field and
    /// feeds `spelunk status --format json`).
    pub fn as_str(&self) -> &'static str {
        match self {
            EmbedderState::Loading => "loading",
            EmbedderState::Ready => "ready",
            EmbedderState::Unavailable => "unavailable",
            EmbedderState::Disabled => "disabled",
            EmbedderState::Unknown => "unknown",
        }
    }
}

/// Server-enforced operative limits relevant to sizing an `/index/embed`
/// request, mirrored from `/v1/health`'s `limits` object (spelunk-oss^71/^73/
/// ^74, PR #513 field-failure follow-up: `crates/spelunk-server/src/handlers.rs`
/// `ServerLimits`).
///
/// `None` on a `Tier::Server` (rather than this struct being absent) means the
/// server pre-dates this field — the embed phase treats that as "assume the
/// legacy 30s / no-embed-exemption profile", which is exactly the
/// version-skew case a newer CLI can hit talking to an older, long-running
/// server (see `embed_phase.rs`'s calibration-vs-server-budget clamping).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerLimits {
    /// Wall-clock budget (seconds) the server allows a single `/index/embed`
    /// request before returning `408`.
    pub embed_request_timeout_secs: u64,
    /// Max chunks accepted in a single `/index/embed` request (`413` above this).
    pub max_batch_chunks: usize,
    /// Per-chunk token truncation cap the embedder enforces, if known.
    pub embedder_token_cap: Option<usize>,
}

/// Feature availability for a server-connected tier.
#[derive(Debug, Clone, Serialize)]
pub struct Capabilities {
    pub search_semantic: bool,
    pub index_embed: bool,
    pub memory_push: bool,
    pub memory_pull: bool,
    pub memory_search: bool,
    pub memory_harvest: bool,
    pub explore: bool,
    pub plan: bool,
}

impl Capabilities {
    fn from_server_caps(caps: &[&str]) -> Self {
        let has = |c: &str| caps.contains(&c);
        let memory = has("memory");
        Self {
            search_semantic: has("search.semantic"),
            index_embed: has("index.embed"),
            memory_push: memory,
            memory_pull: memory,
            memory_search: memory,
            memory_harvest: memory,
            explore: has("explore"),
            plan: has("plan"),
        }
    }

    /// Conservative set assumed when talking to a legacy server that returns
    /// plain-text health ("ok") instead of JSON.
    fn legacy_memory_only() -> Self {
        Self {
            search_semantic: false,
            index_embed: false,
            memory_push: true,
            memory_pull: true,
            memory_search: true,
            memory_harvest: false,
            explore: false,
            plan: false,
        }
    }

    /// Full set for a fully-featured server.
    #[cfg(test)]
    pub fn all() -> Self {
        Self {
            search_semantic: true,
            index_embed: true,
            memory_push: true,
            memory_pull: true,
            memory_search: true,
            memory_harvest: true,
            explore: true,
            plan: true,
        }
    }
}

/// CLI capability tier for this process.
#[derive(Debug, Clone)]
pub enum Tier {
    /// No server configured, or server unreachable. Offline features only.
    Offline,
    /// Server reachable. All `caps`-listed features are available.
    Server {
        url: String,
        caps: Capabilities,
        /// `true` when the URL was discovered automatically (loopback probe),
        /// `false` when it was set explicitly via config / env var.
        /// Used to annotate UX output (e.g. `(local, auto)` in `spelunk status`).
        /// Consumed by `is_auto_discovered()` and sub-issue #324 UX wiring.
        auto_discovered: bool,
        /// Server-side embedder readiness, mirrored from the `/v1/health`
        /// `embedder.state` field (spelunk-oss^50). `Unknown` when the field is
        /// absent (server pre-dates PR A). Lets the CLI distinguish "server up
        /// but model still warming up / failed to load" from a ready server when
        /// semantic search is unavailable (task item #5; rendered by `status`).
        embedder_state: EmbedderState,
        /// Server-enforced `/index/embed` limits, mirrored from `/v1/health`'s
        /// `limits` object (spelunk-oss^71/^73/^74). `None` when the field is
        /// absent — a server that pre-dates this fix and still enforces the
        /// old blanket 30s budget with no `/index/embed` exemption. The embed
        /// phase (`embed_phase.rs`) reads this to clamp its own calibration to
        /// what this particular server actually supports instead of assuming.
        server_limits: Option<ServerLimits>,
    },
}

impl Tier {
    pub fn is_server(&self) -> bool {
        matches!(self, Tier::Server { .. })
    }

    // Used by check.rs / status.rs via pattern matching on the enum variant;
    // also consumed by sub-issues #323/#324 UX wiring.
    #[cfg(test)]
    pub fn server_url(&self) -> Option<&str> {
        match self {
            Tier::Server { url, .. } => Some(url),
            Tier::Offline => None,
        }
    }

    pub fn caps(&self) -> Option<&Capabilities> {
        match self {
            Tier::Server { caps, .. } => Some(caps),
            Tier::Offline => None,
        }
    }

    /// Server-side embedder readiness for a `Server` tier, or `None` when
    /// offline. `EmbedderState::Unknown` is returned for a reachable server that
    /// pre-dates the `embedder.state` health field. Used by the offline notice
    /// (`search`/`index`) and by `spelunk status` to explain why semantic search
    /// is unavailable (spelunk-oss^50).
    pub fn embedder_state(&self) -> Option<EmbedderState> {
        match self {
            Tier::Server { embedder_state, .. } => Some(*embedder_state),
            Tier::Offline => None,
        }
    }

    /// Server-enforced `/index/embed` limits for a `Server` tier, or `None`
    /// when offline *or* when the server pre-dates the `/v1/health` `limits`
    /// field. Used by the embed phase (`embed_phase.rs`) to clamp its own
    /// calibration to what this particular server actually supports —
    /// see spelunk-oss^71/^73/^74 (PR #513 field-failure follow-up).
    pub fn server_limits(&self) -> Option<ServerLimits> {
        match self {
            Tier::Server { server_limits, .. } => *server_limits,
            Tier::Offline => None,
        }
    }

    /// Returns `true` when the server URL was discovered automatically via
    /// the loopback probe rather than set explicitly in config or environment.
    /// Used by `spelunk status` (sub-issue #324) to annotate the URL with `(local, auto)`.
    #[cfg(test)]
    pub fn is_auto_discovered(&self) -> bool {
        matches!(
            self,
            Tier::Server {
                auto_discovered: true,
                ..
            }
        )
    }

    /// Return a `Config` whose server fields reflect this tier, so that
    /// server-backed helpers (`ServerInferenceClient::from_config`,
    /// `open_memory_backend`) work the same whether the server was configured
    /// explicitly or discovered via the loopback probe.
    ///
    /// Loopback auto-discovery sets the capability `Tier` WITHOUT populating
    /// `cfg.server_url`. Commands that route inference through `from_config`
    /// gate on a server URL, so without this bridge they wrongly report
    /// "requires spelunk-server" even though `spelunk status` shows `Server`.
    ///
    /// ## ADR-004: inference vs memory storage are routed separately
    ///
    /// An auto-discovered loopback server is an **inference** backend only; it
    /// is never a memory store. So when the tier is `Server` and `server_url`
    /// is unset (the auto-discovered case), the discovered URL is written to
    /// `inference_url` — NOT `server_url`. `ServerInferenceClient::from_config`
    /// reads `inference_url` (falling back to `server_url`), so inference still
    /// reaches the loopback server; `open_memory_backend` reads only
    /// `server_url`, so memory stays on the project's local `memory.db`. This
    /// is what removes the split-brain where `memory add` wrote `memory.db`
    /// while `memory search` read the server's `server.db`.
    ///
    /// `project_id` is derived (mirroring `embed_phase`, see spelunk#307) so the
    /// inference client can address the project on the server. When an explicit
    /// `cfg.server_url` is already set (a team/remote server), it owns both
    /// inference and memory and the config is returned unchanged.
    pub fn effective_config(&self, cfg: &Config, project_root: &std::path::Path) -> Config {
        let mut out = cfg.clone();
        if let Tier::Server { url, .. } = self
            && out.server_url.is_none()
        {
            // Auto-discovered loopback server: route inference here, but leave
            // `server_url` unset so memory selection stays local (ADR-004).
            out.inference_url = Some(url.clone());
            if out.project_id.is_none() {
                out.project_id = Some(cfg.resolve_project_id(project_root));
            }
        }
        out
    }
}

/// Return the cached capability tier for this process.
///
/// On the first call, probes the server according to the following priority:
///
/// 1. If `SPELUNK_NO_SERVER=1` is set → `Tier::Offline` immediately.
/// 2. If `cfg.server_url` is set → probe that URL with a **2 s** timeout
///    (`auto_discovered = false`).
/// 3. If `cfg.server_url` is `None` → loopback auto-discovery:
///    a. Read `~/.local/state/spelunk/server.port`; probe `127.0.0.1:<port>`.
///    b. Fallback: probe `127.0.0.1:7777`.
///    Both loopback probes use a **250 ms** timeout.
///    On success: `auto_discovered = true`. On failure: `Tier::Offline`.
///
/// Subsequent calls return immediately from the per-process `OnceCell` cache.
///
/// **Per-process cache**: the result is stored in a `static OnceCell` and is fixed
/// for the lifetime of the process. This is correct for CLI invocations (one process
/// = one config), but unsuitable for long-running daemons that may use multiple
/// configs — they would always see the tier determined by the first call.
pub async fn get_tier(cfg: &Config) -> &'static Tier {
    // ADR-037 D1: an *explicit* offline mode (config `mode = "offline"`,
    // `SPELUNK_MODE=offline`, or the `SPELUNK_NO_SERVER=1` kill-switch) skips all
    // server probes — the user has asked for a provable no-cloud run.
    //
    // The *defaulted* offline (no `server_url` and no explicit `mode`) must NOT
    // skip probing: loopback auto-discovery is inference-only (it never owns
    // memory, ADR-004) and is what gives a local-only project semantic search.
    // Conflating the two would silently disable the loopback embedder.
    let explicit_offline = spelunk_core::config::no_server_env_set()
        || cfg.mode == Some(spelunk_core::config::SyncMode::Offline);
    let url = cfg.server_url.clone();
    TIER.get_or_init(|| async move {
        if explicit_offline {
            tracing::debug!("sync mode is explicitly offline — skipping all server probes");
            return Tier::Offline;
        }
        probe(url.as_deref()).await
    })
    .await
}

/// Remote-server probe timeout (explicit `server_url` in config/env).
const REMOTE_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Loopback probe timeout (auto-discovery of a locally-running server).
const LOOPBACK_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// Default loopback port for `spelunk-server`.
const DEFAULT_LOOPBACK_PORT: u16 = 7777;

async fn probe(url: Option<&str>) -> Tier {
    // ── 1. SPELUNK_NO_SERVER short-circuit ───────────────────────────────────
    if matches!(
        std::env::var("SPELUNK_NO_SERVER").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    ) {
        tracing::debug!("SPELUNK_NO_SERVER set — skipping all server probes");
        return Tier::Offline;
    }

    // ── 2. Explicit server_url from config / env ─────────────────────────────
    if let Some(url) = url {
        return match probe_url(url, REMOTE_PROBE_TIMEOUT, false).await {
            Ok(tier) => tier,
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(2);
            }
        };
    }

    // ── 3. Loopback auto-discovery ───────────────────────────────────────────
    // Step 3a: port file written by `spelunk server start`
    if let Some(port) = read_server_port_file() {
        let loopback_url = format!("http://127.0.0.1:{port}");
        tracing::debug!(
            "loopback auto-discovery: found server.port={port}, probing {loopback_url}"
        );
        // Loopback probes never produce hard errors (auto_discovered=true), so unwrap is safe.
        let tier = probe_url(&loopback_url, LOOPBACK_PROBE_TIMEOUT, true)
            .await
            .unwrap_or(Tier::Offline);
        if tier.is_server() {
            return tier;
        }
        tracing::debug!("loopback probe on port {port} failed — falling back to default port");
    }

    // Step 3b: default port 7777
    let default_url = format!("http://127.0.0.1:{DEFAULT_LOOPBACK_PORT}");
    tracing::debug!("loopback auto-discovery: probing default {default_url}");
    let tier = probe_url(&default_url, LOOPBACK_PROBE_TIMEOUT, true)
        .await
        .unwrap_or(Tier::Offline);
    if tier.is_server() {
        return tier;
    }

    tracing::debug!("loopback auto-discovery: no local server found — offline mode");
    Tier::Offline
}

/// Probe a single URL and return the resulting `Tier`, or a hard error string
/// for an explicit-URL dimension mismatch.
///
/// `auto_discovered = true` means the URL was found via the loopback probe rather
/// than set explicitly in config or environment. The distinction controls whether a
/// dimension mismatch is a soft downgrade (loopback) or a hard error (explicit URL).
async fn probe_url(
    url: &str,
    timeout: std::time::Duration,
    auto_discovered: bool,
) -> Result<Tier, String> {
    // Non-loopback plaintext http:// is invalid config — reject before sending
    // anything. No opt-out (Johan, 2026-07-02 — see spelunk-oss^63): the fix is
    // always "use https, or loopback".
    spelunk_core::config::validate_transport_url(url)?;

    let client = match reqwest::Client::builder().timeout(timeout).build() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("could not build HTTP client for server probe: {e}");
            return Ok(Tier::Offline);
        }
    };

    // `/v1/health` is an unauthenticated endpoint (spelunk-oss^56) — do not send
    // a bearer to it (spelunk-oss^63).
    let req = client.get(format!("{}/v1/health", url.trim_end_matches('/')));

    match req.send().await {
        Ok(resp) if resp.status().is_success() => {
            let (caps, server_dim, embedder_state, server_limits) = parse_health(url, resp).await;

            // If the server advertises index.embed, its embedding dimension must match ours.
            if caps.index_embed && server_dim != 0 {
                let expected = spelunk_core::embeddings::EMBEDDING_DIM;
                if server_dim != expected {
                    if auto_discovered {
                        // Loopback auto-discovery: downgrade gracefully — the user did
                        // not explicitly configure this server.
                        tracing::warn!(
                            "spelunk-server at {url} serves {server_dim}-dim embeddings; \
                             this CLI expects {expected}-dim. Ignoring loopback server. \
                             Restart the server (`spelunk server start`) or set \
                             SPELUNK_NO_SERVER=1 to suppress this probe."
                        );
                        return Ok(Tier::Offline);
                    } else {
                        // Explicit server_url: surface as a hard error so the user
                        // gets actionable guidance before any command runs.
                        return Err(format!(
                            "spelunk-server at {url} serves {server_dim}-dim embeddings; \
                             this CLI expects {expected}-dim.\n\
                             Upgrade or replace the server, or remove server_url from \
                             ~/.config/spelunk/config.toml."
                        ));
                    }
                }
            }

            Ok(Tier::Server {
                url: url.to_string(),
                caps,
                auto_discovered,
                embedder_state,
                server_limits,
            })
        }
        Ok(resp) => {
            if !auto_discovered {
                tracing::warn!(
                    "spelunk-server at {url} returned {} — running in offline mode",
                    resp.status()
                );
            }
            Ok(Tier::Offline)
        }
        Err(e) => {
            if !auto_discovered {
                tracing::warn!(
                    "spelunk-server at {url} unreachable — running in offline mode: {e}"
                );
            }
            Ok(Tier::Offline)
        }
    }
}

/// Parse the health response body and return `(Capabilities, embedding_dim,
/// embedder_state, server_limits)`.
///
/// `embedding_dim` is `0` when the field is absent (old server without the field)
/// or when no embedder is loaded. A `0` dim skips the dimension check in `probe_url`
/// for backward compatibility.
///
/// `embedder_state` mirrors the `/v1/health` `embedder.state` field shipped in
/// spelunk-oss^50 PR A (`embedder: { state, detail }`). It is `Unknown` when the
/// sub-object is absent (older server) or the body is legacy plain-text.
///
/// `server_limits` mirrors `/v1/health`'s `limits` object (spelunk-oss^71/^73/
/// ^74). `None` when absent — a server that pre-dates the field, which is
/// exactly the version-skew case: it still enforces the old blanket 30s
/// `/index/embed` budget with no exemption, regardless of what the CLI's own
/// calibration would otherwise target.
async fn parse_health(
    url: &str,
    resp: reqwest::Response,
) -> (Capabilities, usize, EmbedderState, Option<ServerLimits>) {
    #[derive(serde::Deserialize)]
    struct EmbedderBody {
        #[serde(default)]
        state: EmbedderState,
    }

    #[derive(serde::Deserialize)]
    struct HealthBody {
        #[serde(default)]
        capabilities: Vec<String>,
        instance_id: Option<String>,
        started_by: Option<u32>,
        /// Embedding dimension produced by this server's embedder.
        /// Absent on old servers that pre-date this field; defaults to 0 (skip check).
        #[serde(default)]
        embedding_dim: usize,
        /// Embedder readiness sub-object (spelunk-oss^50). Absent on older servers
        /// → `embedder_state` stays `Unknown`.
        #[serde(default)]
        embedder: Option<EmbedderBody>,
        /// Server-enforced `/index/embed` limits (spelunk-oss^71/^73/^74).
        /// Absent on older servers → `server_limits` stays `None`.
        #[serde(default)]
        limits: Option<ServerLimits>,
    }

    match resp.json::<HealthBody>().await {
        Ok(body) => {
            let embedder_state = body
                .embedder
                .as_ref()
                .map(|e| e.state)
                .unwrap_or(EmbedderState::Unknown);
            // Warn if the server was started by a different user on this host.
            if let Some(server_uid) = body.started_by {
                let my_uid = current_uid();
                if let Some(my_uid) = my_uid
                    && my_uid != server_uid
                {
                    tracing::warn!(
                        "spelunk-server at {url} was started by UID {server_uid} \
                         but you are UID {my_uid}; on a multi-user host this may \
                         expose another user's memory — consider running your own server"
                    );
                }
            }
            if let Some(ref id) = body.instance_id {
                tracing::debug!("server instance_id: {id}");
            }
            let cap_strs: Vec<&str> = body.capabilities.iter().map(String::as_str).collect();
            (
                Capabilities::from_server_caps(&cap_strs),
                body.embedding_dim,
                embedder_state,
                body.limits,
            )
        }
        Err(_) => {
            // Legacy server returns plain-text "ok" — conservative fallback.
            // embedding_dim = 0 skips the dimension check; state Unknown; no limits.
            (
                Capabilities::legacy_memory_only(),
                0,
                EmbedderState::Unknown,
                None,
            )
        }
    }
}

/// Return the effective UID of this process (Unix), or `None` on Windows.
fn current_uid() -> Option<u32> {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn geteuid() -> u32;
        }
        Some(unsafe { geteuid() })
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Return `Ok(())` if the tier is `Server`, otherwise return an `anyhow::Error`
/// with the standard locked-feature message format.
///
/// Callers append `?` to propagate the error:
/// ```ignore
/// require_tier1("explore", tier, cfg.server_url.as_deref())?;
/// ```
pub fn require_tier1(feature: &str, tier: &Tier, server_url: Option<&str>) -> anyhow::Result<()> {
    if tier.is_server() {
        return Ok(());
    }
    let tried = server_url
        .map(|u| format!("\n       (Tried: {u} — connection refused)"))
        .unwrap_or_default();
    anyhow::bail!(
        "'spelunk {feature}' requires spelunk-server.\n\
         Set server_url in ~/.config/spelunk/config.toml to enable this feature.{tried}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Capabilities::from_server_caps ──────────────────────────────────────

    #[test]
    fn from_server_caps_empty_returns_all_false() {
        let caps = Capabilities::from_server_caps(&[]);
        assert!(!caps.search_semantic);
        assert!(!caps.index_embed);
        assert!(!caps.memory_push);
        assert!(!caps.memory_pull);
        assert!(!caps.memory_search);
        assert!(!caps.memory_harvest);
        assert!(!caps.explore);
        assert!(!caps.plan);
    }

    #[test]
    fn from_server_caps_full_set() {
        let caps = Capabilities::from_server_caps(&[
            "search.semantic",
            "index.embed",
            "memory",
            "explore",
            "plan",
        ]);
        assert!(caps.search_semantic);
        assert!(caps.index_embed);
        assert!(caps.memory_push);
        assert!(caps.memory_pull);
        assert!(caps.memory_search);
        assert!(caps.memory_harvest);
        assert!(caps.explore);
        assert!(caps.plan);
    }

    #[test]
    fn from_server_caps_memory_only() {
        let caps = Capabilities::from_server_caps(&["memory"]);
        assert!(!caps.search_semantic);
        assert!(!caps.index_embed);
        assert!(!caps.explore);
        assert!(!caps.plan);
        assert!(caps.memory_push);
        assert!(caps.memory_pull);
        assert!(caps.memory_search);
        assert!(caps.memory_harvest);
    }

    #[test]
    fn from_server_caps_partial_set() {
        let caps = Capabilities::from_server_caps(&["search.semantic", "plan"]);
        assert!(caps.search_semantic);
        assert!(!caps.index_embed);
        assert!(!caps.explore);
        assert!(caps.plan);
        assert!(!caps.memory_push);
        assert!(!caps.memory_pull);
        assert!(!caps.memory_search);
        assert!(!caps.memory_harvest);
    }

    #[test]
    fn from_server_caps_unknown_capability_is_ignored() {
        let caps = Capabilities::from_server_caps(&["search.semantic", "unknown.future", "memory"]);
        assert!(caps.search_semantic);
        assert!(!caps.index_embed);
        assert!(caps.memory_push);
        // Unknown capability should not affect any flag.
    }

    // ── Capabilities::legacy_memory_only ─────────────────────────────────────

    #[test]
    fn legacy_memory_only_values() {
        let caps = Capabilities::legacy_memory_only();
        assert!(!caps.search_semantic);
        assert!(!caps.index_embed);
        assert!(!caps.explore);
        assert!(!caps.plan);
        assert!(caps.memory_push);
        assert!(caps.memory_pull);
        assert!(caps.memory_search);
        assert!(!caps.memory_harvest);
    }

    // ── Capabilities::all ────────────────────────────────────────────────────

    #[test]
    fn all_values_are_true() {
        let caps = Capabilities::all();
        assert!(caps.search_semantic);
        assert!(caps.index_embed);
        assert!(caps.memory_push);
        assert!(caps.memory_pull);
        assert!(caps.memory_search);
        assert!(caps.memory_harvest);
        assert!(caps.explore);
        assert!(caps.plan);
    }

    // ── Tier ─────────────────────────────────────────────────────────────────

    #[test]
    fn tier_server_is_server_true() {
        let tier = Tier::Server {
            url: "http://example.com".to_string(),
            caps: Capabilities::all(),
            auto_discovered: false,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        };
        assert!(tier.is_server());
    }

    #[test]
    fn tier_offline_is_server_false() {
        let tier = Tier::Offline;
        assert!(!tier.is_server());
    }

    #[test]
    fn tier_server_returns_url() {
        let tier = Tier::Server {
            url: "http://spelunk.internal:7777".to_string(),
            caps: Capabilities::all(),
            auto_discovered: false,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        };
        assert_eq!(tier.server_url(), Some("http://spelunk.internal:7777"));
    }

    #[test]
    fn tier_offline_returns_none_url() {
        let tier = Tier::Offline;
        assert_eq!(tier.server_url(), None);
    }

    #[test]
    fn tier_server_returns_caps() {
        let caps = Capabilities::all();
        let tier = Tier::Server {
            url: "http://example.com".to_string(),
            caps: caps.clone(),
            auto_discovered: false,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        };
        assert!(tier.caps().is_some());
    }

    #[test]
    fn tier_offline_returns_none_caps() {
        let tier = Tier::Offline;
        assert!(tier.caps().is_none());
    }

    #[test]
    fn tier_auto_discovered_flag() {
        let auto = Tier::Server {
            url: "http://127.0.0.1:7777".to_string(),
            caps: Capabilities::all(),
            auto_discovered: true,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        };
        let explicit = Tier::Server {
            url: "http://server.example.com:7777".to_string(),
            caps: Capabilities::all(),
            auto_discovered: false,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        };
        assert!(auto.is_auto_discovered());
        assert!(!explicit.is_auto_discovered());
        assert!(!Tier::Offline.is_auto_discovered());
    }

    // ── effective_config (ADR-004 inference-vs-memory routing) ───────────────

    #[test]
    fn effective_config_auto_discovered_sets_inference_url_not_server_url() {
        // An auto-discovered loopback server is inference-only: its URL must
        // land in `inference_url` so memory selection (`open_memory_backend`,
        // which reads only `server_url`) stays local. This is the core of the
        // ADR-004 split-brain fix.
        let tier = Tier::Server {
            url: "http://127.0.0.1:7777".to_string(),
            caps: Capabilities::all(),
            auto_discovered: true,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        };
        let cfg = Config::default(); // server_url = None
        let eff = tier.effective_config(&cfg, std::path::Path::new("/tmp/proj"));

        assert_eq!(
            eff.server_url, None,
            "auto-discovered server must NOT populate server_url (memory stays local)"
        );
        assert_eq!(
            eff.inference_url.as_deref(),
            Some("http://127.0.0.1:7777"),
            "auto-discovered server URL must route inference via inference_url"
        );
        assert!(
            eff.project_id.is_some(),
            "project_id should be derived so the inference client can address the project"
        );
        // Inference resolves to the loopback server; memory selection does not.
        assert_eq!(eff.resolve_inference_url(), Some("http://127.0.0.1:7777"));
    }

    #[test]
    fn effective_config_explicit_server_url_left_unchanged() {
        // An explicitly-configured team server owns BOTH inference and memory
        // (team-memory tier). `effective_config` must not touch it.
        let tier = Tier::Server {
            url: "http://team.example.com:7777".to_string(),
            caps: Capabilities::all(),
            auto_discovered: false,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        };
        let cfg = Config {
            server_url: Some("http://team.example.com:7777".to_string()),
            project_id: Some("team/proj".to_string()),
            ..Default::default()
        };
        let eff = tier.effective_config(&cfg, std::path::Path::new("/tmp/proj"));

        assert_eq!(
            eff.server_url.as_deref(),
            Some("http://team.example.com:7777"),
            "explicit team server_url must be preserved (memory stays remote)"
        );
        assert_eq!(
            eff.inference_url, None,
            "explicit server_url path should not synthesise a separate inference_url"
        );
    }

    #[test]
    fn effective_config_offline_tier_is_noop() {
        let cfg = Config::default();
        let eff = Tier::Offline.effective_config(&cfg, std::path::Path::new("/tmp/proj"));
        assert_eq!(eff.server_url, None);
        assert_eq!(eff.inference_url, None);
    }

    // ── require_tier1 ────────────────────────────────────────────────────────

    #[test]
    fn require_tier1_ok_for_server() {
        let tier = Tier::Server {
            url: "http://example.com".to_string(),
            caps: Capabilities::all(),
            auto_discovered: false,
            embedder_state: EmbedderState::Ready,
            server_limits: None,
        };
        assert!(require_tier1("explore", &tier, Some("http://example.com")).is_ok());
    }

    #[test]
    fn require_tier1_err_for_offline_no_url() {
        let tier = Tier::Offline;
        let err = require_tier1("explore", &tier, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("'spelunk explore'"));
        assert!(msg.contains("requires spelunk-server"));
        assert!(msg.contains("server_url"));
    }

    #[test]
    fn require_tier1_err_for_offline_with_url_includes_tried() {
        let tier = Tier::Offline;
        let err = require_tier1("plan", &tier, Some("http://bad:7777")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("'spelunk plan'"));
        assert!(msg.contains("requires spelunk-server"));
        assert!(msg.contains("http://bad:7777"));
        assert!(msg.contains("connection refused"));
    }

    #[test]
    fn require_tier1_uses_feature_name_in_message() {
        let tier = Tier::Offline;
        let err = require_tier1("memory push", &tier, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("'spelunk memory push'"));
    }

    #[test]
    fn require_tier1_no_tried_line_when_url_not_set() {
        let tier = Tier::Offline;
        let err = require_tier1("explore", &tier, None).unwrap_err();
        let msg = err.to_string();
        assert!(!msg.contains("Tried:"));
    }

    // ── read_server_port_file ────────────────────────────────────────────────

    #[test]
    fn read_server_port_file_returns_none_when_absent() {
        // In a temp dir with no server.port file, should return None.
        // We can't control the state dir in a unit test, but we can verify
        // the function doesn't panic and returns a valid Option<u16>.
        // The actual file-read path is exercised by integration tests.
        let _ = read_server_port_file(); // must not panic
    }

    // ── SPELUNK_NO_SERVER and loopback constants ──────────────────────────────

    #[test]
    fn loopback_probe_timeout_is_250ms() {
        assert_eq!(LOOPBACK_PROBE_TIMEOUT.as_millis(), 250);
    }

    #[test]
    fn remote_probe_timeout_is_2s() {
        assert_eq!(REMOTE_PROBE_TIMEOUT.as_secs(), 2);
    }

    #[test]
    fn default_loopback_port_is_7777() {
        assert_eq!(DEFAULT_LOOPBACK_PORT, 7777);
    }

    // ── SPELUNK_NO_SERVER short-circuit behaviour ─────────────────────────────
    //
    // These tests mutate the process-global `SPELUNK_NO_SERVER` env var, so they
    // are serialised against each other to avoid cross-test interference.

    #[tokio::test]
    #[serial_test::serial(spelunk_no_server_env)]
    async fn spelunk_no_server_forces_offline() {
        // SAFETY: serialised via #[serial] so no other test reads/writes this
        // env var concurrently; restored before the guard scope ends.
        for val in ["1", "true", "yes"] {
            unsafe { std::env::set_var("SPELUNK_NO_SERVER", val) };
            // server_url = None so that, absent the short-circuit, the probe would
            // attempt loopback auto-discovery; the short-circuit must win.
            let tier = probe(None).await;
            assert!(
                matches!(tier, Tier::Offline),
                "SPELUNK_NO_SERVER={val} should force Tier::Offline, got {tier:?}"
            );
        }
        unsafe { std::env::remove_var("SPELUNK_NO_SERVER") };
    }

    // ── Embedding-dim pre-flight checks ──────────────────────────────────────

    /// Helper: build a health JSON body with the given capabilities and dim.
    fn health_body(caps: &[&str], dim: usize) -> serde_json::Value {
        serde_json::json!({
            "status": "ok",
            "version": "0.9.0",
            "capabilities": caps,
            "instance_id": "00000000-0000-0000-0000-000000000001",
            "started_by": null,
            "embedding_dim": dim
        })
    }

    /// Auto-discovered loopback server with wrong dim → `Tier::Offline` (soft downgrade).
    #[tokio::test]
    async fn probe_loopback_dim_mismatch_downgrades_to_offline() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // Return a health body claiming 768-dim embeddings — wrong for the current CLI (896).
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body(
                &["memory", "index.embed", "search.semantic"],
                768,
            )))
            .mount(&server)
            .await;

        let result = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true).await;
        assert!(
            matches!(result, Ok(Tier::Offline)),
            "auto-discovered loopback with wrong dim must downgrade to Offline; got {result:?}"
        );
    }

    /// Auto-discovered loopback server with correct dim → `Tier::Server`.
    #[tokio::test]
    async fn probe_loopback_dim_match_returns_server() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body(
                &["memory", "index.embed", "search.semantic"],
                spelunk_core::embeddings::EMBEDDING_DIM,
            )))
            .mount(&server)
            .await;

        let result = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true).await;
        assert!(
            matches!(result, Ok(Tier::Server { .. })),
            "auto-discovered loopback with correct dim must return Server; got {result:?}"
        );
    }

    // ── ServerLimits parsing (spelunk-oss^71/^73/^74, PR #513 field-failure
    //    fix: /v1/health `limits` object) ────────────────────────────────────

    /// A server that DOES advertise `limits` must have it parsed into
    /// `Tier::Server.server_limits`. This is the non-version-skew case: a
    /// current-build server carrying the `/index/embed` timeout exemption.
    #[tokio::test]
    async fn probe_url_parses_server_limits_when_present() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let mut body = health_body(
            &["memory", "index.embed", "search.semantic"],
            spelunk_core::embeddings::EMBEDDING_DIM,
        );
        body["limits"] = serde_json::json!({
            "embed_request_timeout_secs": 1800,
            "max_batch_chunks": 256,
            "embedder_token_cap": 5792,
        });
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let result = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true)
            .await
            .expect("probe must succeed");
        let limits = result
            .server_limits()
            .expect("server_limits must be Some when the health body carries `limits`");
        assert_eq!(limits.embed_request_timeout_secs, 1800);
        assert_eq!(limits.max_batch_chunks, 256);
        assert_eq!(limits.embedder_token_cap, Some(5792));
    }

    /// A server that does NOT advertise `limits` (pre-dates the field) must
    /// leave `Tier::Server.server_limits` as `None` — this is the exact
    /// version-skew case: an old server still enforcing the legacy 30s
    /// `/index/embed` budget with no exemption. `None` must never be
    /// confused with "no limit" by a caller.
    #[tokio::test]
    async fn probe_url_server_limits_none_when_absent_legacy_server() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // health_body() deliberately has no `limits` field (models a server
        // that pre-dates this fix).
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body(
                &["memory", "index.embed", "search.semantic"],
                spelunk_core::embeddings::EMBEDDING_DIM,
            )))
            .mount(&server)
            .await;

        let result = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true)
            .await
            .expect("probe must succeed");
        assert_eq!(
            result.server_limits(),
            None,
            "a server that omits `limits` must be treated as version-skewed, not unlimited"
        );
    }

    /// `embedder_token_cap` specifically must round-trip as `None` when the
    /// server reports it as JSON `null` (e.g. embedder not ready, or an
    /// external non-native backend with no known cap) — distinct from the
    /// whole `limits` object being absent.
    #[tokio::test]
    async fn probe_url_parses_server_limits_with_null_token_cap() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let mut body = health_body(&["memory"], 0);
        body["limits"] = serde_json::json!({
            "embed_request_timeout_secs": 1800,
            "max_batch_chunks": 256,
            "embedder_token_cap": null,
        });
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let result = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true)
            .await
            .expect("probe must succeed");
        let limits = result.server_limits().expect("limits object was present");
        assert_eq!(limits.embedder_token_cap, None);
    }

    /// Auto-discovered loopback server with no embedder (dim 0) → `Tier::Server`
    /// (dim 0 means no `index.embed` check is relevant).
    #[tokio::test]
    async fn probe_loopback_dim_zero_no_embedder_returns_server() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // No index.embed capability, dim 0.
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body(&["memory"], 0)))
            .mount(&server)
            .await;

        let result = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true).await;
        assert!(
            matches!(result, Ok(Tier::Server { .. })),
            "server with no embedder (dim 0) must still return Server; got {result:?}"
        );
    }

    /// Explicit server_url with wrong dim → hard `Err` with an actionable message.
    #[tokio::test]
    async fn probe_explicit_url_dim_mismatch_returns_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body(
                &["memory", "index.embed", "search.semantic"],
                768,
            )))
            .mount(&server)
            .await;

        // auto_discovered = false → explicit server_url path → must be a hard Err.
        let result = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, false).await;
        assert!(
            result.is_err(),
            "explicit server_url with wrong dim must return Err; got {result:?}"
        );
        let msg = result.unwrap_err();
        assert!(
            msg.contains("768"),
            "error must mention the server's dim (768): {msg}"
        );
        let expected = spelunk_core::embeddings::EMBEDDING_DIM;
        assert!(
            msg.contains(&expected.to_string()),
            "error must mention the expected dim ({expected}): {msg}"
        );
        assert!(
            msg.contains("server_url"),
            "error must mention 'server_url' for actionable guidance: {msg}"
        );
    }

    // ── transport-scheme validation (spelunk-oss^63) ─────────────────────────

    /// A non-loopback `http://` URL must be rejected before any request is
    /// sent — no mock is mounted, so a request would fail with "connection
    /// refused" or similar rather than surfacing the validation error; the
    /// assertion on the error message proves the reject happened pre-flight.
    #[tokio::test]
    async fn probe_url_rejects_non_loopback_http_no_request_sent() {
        // Deliberately no MockServer / no listener on this address — if
        // `probe_url` tried to send a request it would get a connection error,
        // not this validation message.
        let result = probe_url("http://team-server:7777", REMOTE_PROBE_TIMEOUT, false).await;
        let err = result.expect_err("non-loopback http:// must be a hard error");
        assert!(err.contains("loopback"), "got: {err}");
        assert!(err.contains("https"), "got: {err}");
    }

    /// Same rejection applies to the loopback auto-discovery path (defensive;
    /// auto-discovery URLs are always loopback in practice).
    #[tokio::test]
    async fn probe_url_rejects_non_loopback_http_even_when_auto_discovered() {
        let result = probe_url("http://team-server:7777", LOOPBACK_PROBE_TIMEOUT, true).await;
        assert!(result.is_err());
    }

    /// Loopback `http://` and `https://` URLs are accepted (proceed to the
    /// actual health request against a mock server).
    #[tokio::test]
    async fn probe_url_accepts_loopback_http_and_https() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body(&["memory"], 0)))
            .mount(&server)
            .await;

        // wiremock serves over http on 127.0.0.1, which is loopback.
        let result = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, false).await;
        assert!(
            matches!(result, Ok(Tier::Server { .. })),
            "loopback http:// must be accepted; got {result:?}"
        );
    }

    /// `/v1/health` must never carry an `Authorization` header — it is an
    /// unauthenticated endpoint (spelunk-oss^56 server-side companion).
    #[tokio::test]
    async fn probe_url_health_request_carries_no_bearer_header() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body(&["memory"], 0)))
            .expect(1)
            .mount(&server)
            .await;

        let result = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, false).await;
        assert!(matches!(result, Ok(Tier::Server { .. })), "got {result:?}");

        // Assert no request in wiremock's log carried an Authorization header.
        let requests = server.received_requests().await.expect("requests recorded");
        assert_eq!(requests.len(), 1);
        assert!(
            !requests[0].headers.contains_key("authorization"),
            "the /v1/health probe must not send an Authorization header"
        );
    }

    // ── EmbedderState (spelunk-oss^50) ───────────────────────────────────────

    #[test]
    fn embedder_state_default_is_unknown() {
        assert_eq!(EmbedderState::default(), EmbedderState::Unknown);
    }

    #[test]
    fn embedder_state_deserializes_lowercase_wire_values() {
        // Must match the server's `#[serde(rename_all = "lowercase")]` values.
        for (wire, want) in [
            ("loading", EmbedderState::Loading),
            ("ready", EmbedderState::Ready),
            ("unavailable", EmbedderState::Unavailable),
            ("disabled", EmbedderState::Disabled),
        ] {
            let got: EmbedderState =
                serde_json::from_value(serde_json::Value::String(wire.to_string())).unwrap();
            assert_eq!(got, want, "wire {wire:?} should deserialize to {want:?}");
            assert_eq!(want.as_str(), wire, "as_str round-trips the wire value");
        }
    }

    #[test]
    fn tier_embedder_state_accessor() {
        let tier = Tier::Server {
            url: "http://127.0.0.1:7777".to_string(),
            caps: Capabilities::all(),
            auto_discovered: true,
            embedder_state: EmbedderState::Loading,
            server_limits: None,
        };
        assert_eq!(tier.embedder_state(), Some(EmbedderState::Loading));
        assert_eq!(Tier::Offline.embedder_state(), None);
    }

    /// Health body carrying the PR-A `embedder: { state, detail }` sub-object.
    fn health_body_with_embedder(state: &str) -> serde_json::Value {
        serde_json::json!({
            "status": "ok",
            "version": "0.9.1",
            "capabilities": ["memory"],
            "instance_id": "00000000-0000-0000-0000-000000000001",
            "started_by": null,
            "embedding_dim": 0,
            "embedder": { "state": state, "detail": null }
        })
    }

    /// `probe_url` must surface the server's `embedder.state` on `Tier::Server`.
    #[tokio::test]
    async fn probe_url_carries_embedder_state_loading() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(health_body_with_embedder("loading")),
            )
            .mount(&server)
            .await;

        let tier = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true)
            .await
            .expect("probe ok");
        assert_eq!(tier.embedder_state(), Some(EmbedderState::Loading));
    }

    #[tokio::test]
    async fn probe_url_carries_embedder_state_unavailable() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(health_body_with_embedder("unavailable")),
            )
            .mount(&server)
            .await;

        let tier = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true)
            .await
            .expect("probe ok");
        assert_eq!(tier.embedder_state(), Some(EmbedderState::Unavailable));
    }

    /// A server that pre-dates the `embedder` field → `Unknown` (not an error).
    #[tokio::test]
    async fn probe_url_absent_embedder_field_is_unknown() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // `health_body` (no `embedder` key) simulates an older server.
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body(&["memory"], 0)))
            .mount(&server)
            .await;

        let tier = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true)
            .await
            .expect("probe ok");
        assert_eq!(tier.embedder_state(), Some(EmbedderState::Unknown));
    }
}
