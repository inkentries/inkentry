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

use tokio::sync::OnceCell;

use crate::config::Config;

mod diagnostics;
mod state;
mod tier;

pub use diagnostics::{ConnFailure, explicit_probe_failure};
use diagnostics::{cert_trust_hint, error_chain, find_rustls_cause, record_explicit_probe_failure};

#[allow(unused_imports)]
pub use state::Capabilities;
pub use state::{EmbedderState, ServerLimits};
pub use tier::Tier;
/// The single state file directory resolver for the whole CLI:
/// `~/.local/state/spelunk/`, or `SPELUNK_STATE_DIR` when set.
///
/// Every reader and writer of runtime state goes through this one function:
/// `spelunk server start/stop/status/logs` (server pid/port/log/db-path
/// files, `cli/cmd/server.rs`), the embed worker's liveness files
/// (`cli/cmd/embed_worker.rs`), and this module's own loopback
/// auto-discovery probe below. A second, independent resolution here was a
/// real bug: it let the override apply to some readers/writers and not
/// others, so a status reader could miss a worker's pid file written to a
/// different directory (or vice versa) and misreport liveness.
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
/// (spelunk#316).
///
/// `SPELUNK_STATE_DIR` is a supported override of the entire path, not
/// dev-only cruft: it is load-bearing on Windows CI, where `dirs::home_dir()`
/// 6.x calls `SHGetKnownFolderPath` (a Windows Registry lookup) rather than
/// reading `USERPROFILE`, making per-process environment overrides of `HOME`
/// ineffective. It is also used directly by end users who want state files
/// somewhere other than the default (e.g. an ephemeral or sandboxed HOME).
///
/// Errors only when the home directory can't be resolved and no override is
/// set.
pub(crate) fn spelunk_state_dir() -> anyhow::Result<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("SPELUNK_STATE_DIR") {
        return Ok(std::path::PathBuf::from(p));
    }
    dirs::home_dir()
        .map(|home| home.join(".local").join("state").join("spelunk"))
        .ok_or_else(|| anyhow::anyhow!("could not determine home directory"))
}
/// Read the port written by `spelunk server start` into
/// `~/.local/state/spelunk/server.port`. Returns `None` if absent or unreadable.
fn read_server_port_file() -> Option<u16> {
    let path = spelunk_state_dir().ok()?.join("server.port");
    let content = std::fs::read_to_string(&path).ok()?;
    content.trim().parse::<u16>().ok()
}
static TIER: OnceCell<Tier> = OnceCell::const_new();
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
    // An *explicit* offline mode (config `mode = "offline"`,
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
    let server_ca = cfg.server_ca.clone();
    TIER.get_or_init(|| async move {
        if explicit_offline {
            tracing::debug!("sync mode is explicitly offline — skipping all server probes");
            return Tier::Offline;
        }
        probe(
            url.as_deref(),
            server_ca.as_deref().map(std::path::Path::new),
        )
        .await
    })
    .await
}
/// One fresh, uncached tier probe, honouring the same explicit-offline
/// short-circuits as [`get_tier`].
///
/// For pollers only: `get_tier`'s process-lifetime cache pins whatever state
/// the first probe saw, so a caller that must observe a *transition* (the
/// detached embed worker waiting for the embedder to finish loading) has to
/// re-probe. Everything else should keep using [`get_tier`].
pub async fn probe_tier_fresh(cfg: &Config) -> Tier {
    let explicit_offline = spelunk_core::config::no_server_env_set()
        || cfg.mode == Some(spelunk_core::config::SyncMode::Offline);
    if explicit_offline {
        return Tier::Offline;
    }
    probe(
        cfg.server_url.as_deref(),
        cfg.server_ca.as_deref().map(std::path::Path::new),
    )
    .await
}
/// Remote-server probe timeout (explicit `server_url` in config/env).
const REMOTE_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
/// Loopback probe timeout (auto-discovery of a locally-running server).
const LOOPBACK_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);
/// Default loopback port for `spelunk-server`.
const DEFAULT_LOOPBACK_PORT: u16 = 7777;
async fn probe(url: Option<&str>, server_ca: Option<&std::path::Path>) -> Tier {
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
        return match probe_url(url, REMOTE_PROBE_TIMEOUT, false, server_ca).await {
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
        // Loopback is plaintext http — a custom CA is irrelevant here.
        let tier = probe_url(&loopback_url, LOOPBACK_PROBE_TIMEOUT, true, None)
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
    let tier = probe_url(&default_url, LOOPBACK_PROBE_TIMEOUT, true, None)
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
    server_ca: Option<&std::path::Path>,
) -> Result<Tier, String> {
    // Non-loopback plaintext http:// is invalid config — reject before sending
    // anything. No opt-out: the fix is always "use https, or loopback".
    spelunk_core::config::validate_transport_url(url)?;

    let builder = match spelunk_core::config::apply_server_ca(reqwest::Client::builder(), server_ca)
    {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("could not load custom CA for server probe: {e}");
            return Ok(Tier::Offline);
        }
    };
    let client = match builder.timeout(timeout).build() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("could not build HTTP client for server probe: {e}");
            return Ok(Tier::Offline);
        }
    };

    // `/v1/health` is an unauthenticated endpoint — do not send a bearer to it.
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
                let chain = error_chain(&e);
                match find_rustls_cause(&e) {
                    Some(cause) => {
                        record_explicit_probe_failure(ConnFailure::Tls(cause.clone()));
                        let hint = if server_ca.is_some() {
                            cert_trust_hint()
                        } else {
                            String::new()
                        };
                        tracing::warn!(
                            "spelunk-server at {url} reachable, but TLS trust failed: {cause}; \
                             running in offline mode.\n  full error chain: {chain}{hint}"
                        );
                    }
                    None => {
                        record_explicit_probe_failure(ConnFailure::Unreachable);
                        tracing::warn!(
                            "spelunk-server at {url} unreachable, running in offline mode: {chain}"
                        );
                    }
                }
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
/// `embedder_state` mirrors the `/v1/health` `embedder.state` field
/// (`embedder: { state, detail }`). It is `Unknown` when the sub-object is
/// absent (older server) or the body is legacy plain-text.
///
/// `server_limits` mirrors `/v1/health`'s `limits` object. `None` when absent —
/// a server that pre-dates the field, which is exactly the version-skew case:
/// it still enforces the old blanket 30s `/index/embed` budget with no
/// exemption, regardless of what the CLI's own calibration would otherwise
/// target.
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
        /// Embedder readiness sub-object. Absent on older servers
        /// → `embedder_state` stays `Unknown`.
        #[serde(default)]
        embedder: Option<EmbedderBody>,
        /// Server-enforced `/index/embed` limits.
        /// Absent on older servers → `server_limits` stays `None`.
        #[serde(default)]
        limits: Option<ServerLimits>,
        /// Whether the server accepts a client-pushed embedding vector on
        /// `POST /memory/batch`. Top-level bool, not a
        /// `capabilities` entry. Absent on servers without the accept side
        /// (older servers, the OSS team server) → defaults false.
        #[serde(default)]
        accepts_pushed_vectors: bool,
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
            let mut caps = Capabilities::from_server_caps(&cap_strs);
            // `accepts_pushed_vectors` is a top-level health bool, not a
            // `capabilities` array entry, so it is applied after the array parse.
            caps.accepts_pushed_vectors = body.accepts_pushed_vectors;
            (caps, body.embedding_dim, embedder_state, body.limits)
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
/// Guidance for an *inference*-backed feature (semantic `memory search`,
/// `memory timeline`, `memory harvest`) that has no reachable server.
///
/// Emitted at client construction, where reachability is unknown: when
/// `server_url` is set, construction always succeeds, so this message only ever
/// fires with `server_url` unset. It therefore carries no configured-server
/// hint; a team-server-unreachable hint, if ever wanted, must be produced at the
/// inference call site where the connection failure is observed. `server_url`
/// advice stays `require_tier1`'s job for the genuinely team-only features.
pub fn inference_server_required_message(feature: &str) -> String {
    format!(
        "'spelunk {feature}' requires spelunk-server.\n\
         Run `spelunk server start` to enable this feature."
    )
}
/// Return `Ok(())` if the tier is `Server`, otherwise return an `anyhow::Error`
/// with the standard locked-feature message format.
///
/// The message is scoped to the actual failure state: with a configured
/// `server_url` the fix is never "set server_url" (it already is), it is that
/// the configured server could not be served from.
///
/// Callers append `?` to propagate the error:
/// ```ignore
/// require_tier1("explore", tier, cfg.server_url.as_deref())?;
/// ```
pub fn require_tier1(feature: &str, tier: &Tier, server_url: Option<&str>) -> anyhow::Result<()> {
    if tier.is_server() {
        return Ok(());
    }
    match server_url {
        Some(url) => anyhow::bail!(
            "'spelunk {feature}' requires spelunk-server.\n\
             The configured server_url ({url}) did not respond to the health probe.\n\
             Check that server and your network; for TLS trust failures see \
             server_ca / SPELUNK_SERVER_CA."
        ),
        None => anyhow::bail!(
            "'spelunk {feature}' requires spelunk-server.\n\
             Set server_url in ~/.config/spelunk/config.toml to enable this feature."
        ),
    }
}
/// Guard for a feature that moves memory to or from an explicitly-configured
/// server (`memory push`, `sync`, `memory pull`): a self-hosted team server or
/// Spelunk Cloud both work identically here. Distinct from features that
/// merely need *an* inference-capable server (covered by [`require_tier1`]).
///
/// `require_tier1` alone is not sufficient for these commands: an
/// auto-discovered loopback inference server makes the tier `Server` while
/// `cfg.server_url` stays `None`, and that loopback server is never a memory
/// store (ADR-004). Callers check `require_tier1` first, which already bails
/// on `Tier::Offline`; this guard then confirms the server was configured
/// explicitly rather than merely auto-discovered.
///
/// Deliberately **explicit-config-only**: it reads `cfg.server_url` and
/// nothing else, never probing reachability or consulting the health-probe
/// `Capabilities` itself. By the time this runs, every current caller has
/// already established reachability via `require_tier1`, so this only ever
/// needs to answer one question: was the server set explicitly? Returns the
/// configured `server_url` on success.
pub fn require_explicit_server_url(feature: &str, cfg: &Config) -> anyhow::Result<String> {
    cfg.server_url.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "'spelunk {feature}' requires a server. Set `server_url` in your spelunk config \
             (e.g. ~/.config/spelunk/config.toml or .spelunk/config.toml)."
        )
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use diagnostics::reset_explicit_probe_failure_for_test;
    // ── inference_server_required_message ────────────────────────────────────

    /// No server reachable AND no `server_url` configured (solo user, no local
    /// server running): the message must point at the zero-setup local server
    /// and must NOT mention `server_url` (the misleading team-infra advice).
    #[test]
    fn inference_msg_no_server_url_points_at_local_start_only() {
        let msg = inference_server_required_message("memory search");
        assert!(msg.contains("'spelunk memory search' requires spelunk-server"));
        assert!(
            msg.contains("spelunk server start"),
            "must point at the local auto-server: {msg}"
        );
        assert!(
            !msg.contains("server_url"),
            "must NOT mention server_url when none is configured: {msg}"
        );
    }

    /// Feature name is interpolated (harvest reuses this via
    /// `harvest_requires_server`, preserving its Tier-0 substring contract).
    #[test]
    fn inference_msg_interpolates_feature_and_keeps_harvest_substring() {
        let msg = inference_server_required_message("memory harvest");
        assert!(msg.contains("'spelunk memory harvest' requires spelunk-server"));
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
        assert!(msg.contains("Set server_url"));
    }

    #[test]
    fn require_tier1_err_for_offline_with_url_names_that_server() {
        // server_url is already configured; the message must name the failing
        // server, never tell the operator to set what is already set.
        let tier = Tier::Offline;
        let err = require_tier1("plan", &tier, Some("https://bad:7777")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("'spelunk plan'"));
        assert!(msg.contains("requires spelunk-server"));
        assert!(msg.contains("https://bad:7777"));
        assert!(
            !msg.contains("Set server_url"),
            "must not suggest setting an already-set server_url: {msg}"
        );
        assert!(
            msg.contains("server_ca"),
            "must point at the TLS-trust knob for untrusted-cert failures: {msg}"
        );
    }

    #[test]
    fn require_tier1_uses_feature_name_in_message() {
        let tier = Tier::Offline;
        let err = require_tier1("memory push", &tier, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("'spelunk memory push'"));
    }

    // require_explicit_server_url: `memory push`, `sync`, and `memory pull` all move
    // memory to/from an explicitly-configured team server; an auto-discovered
    // loopback inference server must never satisfy them (ADR-004: it is never
    // a memory store). `require_tier1` alone is not enough for these commands,
    // since a loopback server makes the tier `Server` while `cfg.server_url`
    // stays `None`. This guard checks explicit configuration only and must
    // never probe reachability, so it stays usable before any network call is
    // made.

    #[test]
    fn require_explicit_server_url_errs_when_unset() {
        let cfg = Config {
            server_url: None,
            ..Default::default()
        };
        let err = require_explicit_server_url("sync", &cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("server_url"));
    }

    #[test]
    fn require_explicit_server_url_ok_regardless_of_reachability() {
        // A garbage, unreachable URL: the guard must still succeed, because it
        // checks configuration presence only, never reachability. Reachability
        // is `require_tier1`'s job at the actual call site.
        let cfg = Config {
            server_url: Some("https://unreachable.invalid:1".to_string()),
            ..Default::default()
        };
        assert_eq!(
            require_explicit_server_url("sync", &cfg).unwrap(),
            "https://unreachable.invalid:1"
        );
    }

    /// The load-bearing regression test: `memory push` and `sync` must refuse
    /// with the exact same message shape (only the feature name differs), so
    /// they can never again drift into the two different messages this guard
    /// was extracted to prevent.
    #[test]
    fn require_explicit_server_url_message_is_identical_in_shape_across_features() {
        let cfg = Config {
            server_url: None,
            ..Default::default()
        };
        let push_msg = require_explicit_server_url("memory push", &cfg)
            .unwrap_err()
            .to_string();
        let sync_msg = require_explicit_server_url("sync", &cfg)
            .unwrap_err()
            .to_string();
        assert_eq!(
            push_msg.replace("memory push", "sync"),
            sync_msg,
            "push and sync messages must differ only in the feature name: \
                 push={push_msg:?} sync={sync_msg:?}"
        );
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
            let tier = probe(None, None).await;
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

        let result = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true, None).await;
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

        let result = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true, None).await;
        assert!(
            matches!(result, Ok(Tier::Server { .. })),
            "auto-discovered loopback with correct dim must return Server; got {result:?}"
        );
    }

    // ── accepts_pushed_vectors (top-level health bool) ──────────────────────────

    /// A server advertising `accepts_pushed_vectors: true` must parse into
    /// `caps.accepts_pushed_vectors == true` — the gate the sync push reads
    /// before attaching a client-computed vector.
    #[tokio::test]
    async fn probe_url_parses_accepts_pushed_vectors_true() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let mut body = health_body(&["memory"], spelunk_core::embeddings::EMBEDDING_DIM);
        body["accepts_pushed_vectors"] = serde_json::json!(true);
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let tier = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true, None)
            .await
            .expect("probe must succeed");
        assert!(
            tier.caps().unwrap().accepts_pushed_vectors,
            "health `accepts_pushed_vectors: true` must set the capability"
        );
    }

    /// A server that omits the field (older server, or the OSS team server)
    /// must default to `false` — the push stays text-only there.
    #[tokio::test]
    async fn probe_url_accepts_pushed_vectors_defaults_false_when_absent() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // `health_body` carries no `accepts_pushed_vectors` field.
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body(
                &["memory"],
                spelunk_core::embeddings::EMBEDDING_DIM,
            )))
            .mount(&server)
            .await;

        let tier = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true, None)
            .await
            .expect("probe must succeed");
        assert!(
            !tier.caps().unwrap().accepts_pushed_vectors,
            "absent `accepts_pushed_vectors` must default to false (text-only)"
        );
    }

    // ── ServerLimits parsing (/v1/health `limits` object) ──────────────────────

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

        let result = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true, None)
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

        let result = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true, None)
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

        let result = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true, None)
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

        let result = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true, None).await;
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
        let result = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, false, None).await;
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

    // ── transport-scheme validation ──────────────────────────────────────────

    /// A non-loopback `http://` URL must be rejected before any request is
    /// sent — no mock is mounted, so a request would fail with "connection
    /// refused" or similar rather than surfacing the validation error; the
    /// assertion on the error message proves the reject happened pre-flight.
    #[tokio::test]
    async fn probe_url_rejects_non_loopback_http_no_request_sent() {
        // Deliberately no MockServer / no listener on this address — if
        // `probe_url` tried to send a request it would get a connection error,
        // not this validation message.
        let result = probe_url("http://team-server:7777", REMOTE_PROBE_TIMEOUT, false, None).await;
        let err = result.expect_err("non-loopback http:// must be a hard error");
        assert!(err.contains("loopback"), "got: {err}");
        assert!(err.contains("https"), "got: {err}");
    }

    /// Same rejection applies to the loopback auto-discovery path (defensive;
    /// auto-discovery URLs are always loopback in practice).
    #[tokio::test]
    async fn probe_url_rejects_non_loopback_http_even_when_auto_discovered() {
        let result = probe_url(
            "http://team-server:7777",
            LOOPBACK_PROBE_TIMEOUT,
            true,
            None,
        )
        .await;
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
        let result = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, false, None).await;
        assert!(
            matches!(result, Ok(Tier::Server { .. })),
            "loopback http:// must be accepted; got {result:?}"
        );
    }

    /// `/v1/health` must never carry an `Authorization` header — it is an
    /// unauthenticated endpoint.
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

        let result = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, false, None).await;
        assert!(matches!(result, Ok(Tier::Server { .. })), "got {result:?}");

        // Assert no request in wiremock's log carried an Authorization header.
        let requests = server.received_requests().await.expect("requests recorded");
        assert_eq!(requests.len(), 1);
        assert!(
            !requests[0].headers.contains_key("authorization"),
            "the /v1/health probe must not send an Authorization header"
        );
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

        let tier = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true, None)
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

        let tier = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true, None)
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

        let tier = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true, None)
            .await
            .expect("probe ok");
        assert_eq!(tier.embedder_state(), Some(EmbedderState::Unknown));
    }

    // ── get_tier process-cache semantics ─────────────────────────────────────

    /// `TIER` is a `OnceCell`: `get_tier` must probe at most once per process
    /// and every later call must return the identical cached `Tier`, not
    /// re-probe. This is what makes `EXPLICIT_PROBE_FAILURE` safe to read from
    /// `Tier::Offline` rendering: there is no later probe in the same process
    /// that could silently swap a fresh success in underneath a stale failure
    /// annotation (or vice versa).
    #[tokio::test]
    #[serial_test::serial(explicit_probe_failure)]
    async fn get_tier_probes_at_most_once_and_caches_the_result() {
        reset_explicit_probe_failure_for_test();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind free port");
        let port = listener.local_addr().expect("local_addr").port();
        drop(listener); // nothing listens on `port` from here on: connection refused.

        let cfg = Config {
            server_url: Some(format!("http://127.0.0.1:{port}")),
            ..Default::default()
        };

        let first = get_tier(&cfg).await;
        assert!(matches!(first, Tier::Offline), "got {first:?}");
        assert_eq!(
            explicit_probe_failure(),
            Some(ConnFailure::Unreachable),
            "connection-refused must classify as Unreachable, not Tls"
        );

        let second = get_tier(&cfg).await;
        assert!(
            std::ptr::eq(first, second),
            "get_tier must return the same cached &'static Tier on a later call, not re-probe"
        );
        assert_eq!(
            explicit_probe_failure(),
            Some(ConnFailure::Unreachable),
            "a cached second get_tier call must not disturb the recorded probe failure"
        );
    }

    // ── classification matrix: real reqwest errors, not hand-built chains ────

    /// A genuine TCP connection-refused error through the real `reqwest`
    /// client must classify as `Unreachable`, never `Tls`: no TLS layer is
    /// ever reached, so `find_rustls_cause` must return `None` on it.
    #[tokio::test]
    #[serial_test::serial(explicit_probe_failure)]
    async fn probe_url_explicit_connection_refused_sets_unreachable_not_tls() {
        reset_explicit_probe_failure_for_test();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind free port");
        let port = listener.local_addr().expect("local_addr").port();
        drop(listener);

        let url = format!("http://127.0.0.1:{port}");
        let result = probe_url(&url, REMOTE_PROBE_TIMEOUT, false, None).await;
        assert!(matches!(result, Ok(Tier::Offline)), "got {result:?}");
        assert_eq!(
            explicit_probe_failure(),
            Some(ConnFailure::Unreachable),
            "connection-refused must not be mislabelled as a TLS trust failure"
        );
    }

    /// A genuine client-side timeout (the peer accepts the TCP connection but
    /// never answers) must also classify as `Unreachable`, not `Tls`: a slow
    /// or hung server is not a certificate problem.
    #[tokio::test]
    #[serial_test::serial(explicit_probe_failure)]
    async fn probe_url_explicit_timeout_sets_unreachable_not_tls() {
        reset_explicit_probe_failure_for_test();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind free port");
        let port = listener.local_addr().expect("local_addr").port();
        std::thread::spawn(move || {
            // Accept and hold every connection open without ever writing a
            // response, forcing the client-side timeout below to fire.
            for stream in listener.incoming().flatten() {
                std::thread::sleep(std::time::Duration::from_secs(5));
                drop(stream);
            }
        });

        let url = format!("http://127.0.0.1:{port}");
        let result = probe_url(&url, std::time::Duration::from_millis(100), false, None).await;
        assert!(matches!(result, Ok(Tier::Offline)), "got {result:?}");
        assert_eq!(
            explicit_probe_failure(),
            Some(ConnFailure::Unreachable),
            "a timeout must not be mislabelled as a TLS trust failure"
        );
    }

    /// A reachable server that answers with a non-2xx status (e.g. a
    /// misconfigured reverse proxy, a 500, garbage) is neither `[tls: ...]`
    /// nor `[unreachable]`: the transport and TLS both worked fine. This
    /// path must leave `EXPLICIT_PROBE_FAILURE` unset entirely.
    #[tokio::test]
    #[serial_test::serial(explicit_probe_failure)]
    async fn probe_url_explicit_non_success_status_does_not_set_any_probe_failure() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Must pass regardless of what other `capability::` test populated
        // EXPLICIT_PROBE_FAILURE earlier in this process, so reset first
        // rather than relying on execution order.
        reset_explicit_probe_failure_for_test();

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let result = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, false, None).await;
        assert!(matches!(result, Ok(Tier::Offline)), "got {result:?}");
        assert_eq!(
            explicit_probe_failure(),
            None,
            "a reachable server answering with a non-2xx status must not populate \
                 EXPLICIT_PROBE_FAILURE: that would render a stale/wrong [tls:] or \
                 [unreachable] label for a request that was neither"
        );
    }

    /// Auto-discovered (loopback) probe failures must never populate
    /// `EXPLICIT_PROBE_FAILURE`: that cache exists only to annotate an
    /// *explicit* `server_url` miss. A common "no local server running"
    /// loopback miss must not leave behind a failure cause that a later
    /// status render could misattribute to an unrelated explicit `server_url`.
    #[tokio::test]
    #[serial_test::serial(explicit_probe_failure)]
    async fn probe_url_auto_discovered_connection_refused_leaves_probe_failure_unset() {
        // Must pass regardless of what other `capability::` test populated
        // EXPLICIT_PROBE_FAILURE earlier in this process, so reset first
        // rather than relying on execution order.
        reset_explicit_probe_failure_for_test();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind free port");
        let port = listener.local_addr().expect("local_addr").port();
        drop(listener);

        let url = format!("http://127.0.0.1:{port}");
        let result = probe_url(&url, LOOPBACK_PROBE_TIMEOUT, true, None).await;
        assert!(matches!(result, Ok(Tier::Offline)), "got {result:?}");
        assert_eq!(
            explicit_probe_failure(),
            None,
            "loopback auto-discovery misses must never populate EXPLICIT_PROBE_FAILURE"
        );
    }
}
