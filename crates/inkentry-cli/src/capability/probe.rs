//! Server probing: loopback auto-discovery, explicit `server_url` health
//! checks, and the per-process cached `Tier` this crate reads everywhere.

use inkentry_core::config::DEFAULT_SERVER_PORT;
use tokio::sync::OnceCell;

use crate::config::Config;

use super::diagnostics::{
    ConnFailure, OfflineReason, cert_trust_hint, error_chain, find_rustls_cause,
    record_explicit_probe_failure,
};
use super::state::{Capabilities, EmbedderState, ServerLimits};
use super::tier::Tier;

// Every reader and writer of runtime state must resolve through this so
// `INKENTRY_STATE_DIR` applies to all of them. `~/.local/state` on every
// platform because `dirs::state_dir()` is `None` on macOS. The override is
// also load-bearing on Windows, where `dirs::home_dir()` ignores `USERPROFILE`.
pub(crate) fn inkentry_state_dir() -> anyhow::Result<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("INKENTRY_STATE_DIR") {
        return Ok(std::path::PathBuf::from(p));
    }
    dirs::home_dir()
        .map(|home| home.join(".local").join("state").join("inkentry"))
        .ok_or_else(|| anyhow::anyhow!("could not determine home directory"))
}

fn read_server_port_file() -> Option<u16> {
    let path = inkentry_state_dir().ok()?.join("server.port");
    let content = std::fs::read_to_string(&path).ok()?;
    content.trim().parse::<u16>().ok()
}

// The health body is self-reported and confirms only itself, so the recorded
// pid and instance id, written by this CLI, are the independent checks.
#[derive(Debug, PartialEq)]
pub(crate) enum Untrusted {
    NoStateDir,
    NoRecordedPid,
    PidIsNotTheServer(u32),
    NoRecordedInstanceId,
    InstanceIdMismatch,
}

impl std::fmt::Display for Untrusted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Untrusted::NoStateDir => write!(
                f,
                "the state directory holding what this CLI recorded could not be resolved"
            ),
            Untrusted::NoRecordedPid => write!(f, "no server.pid was recorded next to the port"),
            Untrusted::PidIsNotTheServer(pid) => {
                write!(
                    f,
                    "the recorded pid={pid} is not an inkentry-server process"
                )
            }
            Untrusted::NoRecordedInstanceId => write!(
                f,
                "no server.instance_id was recorded, so the instance it reports \
                 cannot be checked against anything"
            ),
            Untrusted::InstanceIdMismatch => write!(
                f,
                "it reports a different instance_id than the one recorded at start"
            ),
        }
    }
}

fn state_dir_for_message() -> String {
    inkentry_state_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "~/.local/state/inkentry".to_string())
}

// Reads via the module that writes these files, so the two cannot drift.
pub(crate) fn untrusted_responder(reported: Option<&str>) -> Option<Untrusted> {
    use crate::cli::cmd::server::{read_instance_id, read_pid};

    let Ok(state_dir) = inkentry_state_dir() else {
        return Some(Untrusted::NoStateDir);
    };
    let recorded_pid = read_pid(&state_dir);
    let pid_matches = recorded_pid.map(recorded_pid_is_server).unwrap_or(false);
    classify_responder(
        recorded_pid,
        pid_matches,
        read_instance_id(&state_dir).as_deref(),
        reported,
    )
}

// The env override skips the OS process query for tests: an in-process server
// has no separate process to record, and Windows offers no way to stage one.
// It relaxes only this check and is never read by `classify_running_server`,
// so it cannot widen which processes a lifecycle command signals.
fn recorded_pid_is_server(pid: u32) -> bool {
    if let Ok(raw) = std::env::var("INKENTRY_TEST_TRUST_RECORDED_RESPONDER") {
        let v = raw.trim();
        if v == "1" || v.eq_ignore_ascii_case("true") {
            return true;
        }
    }
    crate::cli::cmd::server::process_matches_server(pid)
}

// The OS process query arrives as a bool so the policy is testable on every
// platform.
fn classify_responder(
    recorded_pid: Option<u32>,
    pid_matches_server: bool,
    recorded_instance_id: Option<&str>,
    reported_instance_id: Option<&str>,
) -> Option<Untrusted> {
    let Some(pid) = recorded_pid else {
        return Some(Untrusted::NoRecordedPid);
    };
    if !pid_matches_server {
        return Some(Untrusted::PidIsNotTheServer(pid));
    }
    let Some(recorded) = recorded_instance_id else {
        return Some(Untrusted::NoRecordedInstanceId);
    };
    (reported_instance_id != Some(recorded)).then_some(Untrusted::InstanceIdMismatch)
}

static TIER: OnceCell<Tier> = OnceCell::const_new();

// Defaulted offline (no `server_url`, no `mode`) must still probe: loopback
// discovery is inference-only and is what gives a local project semantic
// search. The kill switch is checked first because it makes `mode` and
// `server_url` inert.
fn explicit_offline_reason(cfg: &Config) -> Option<OfflineReason> {
    if inkentry_core::config::no_server_env_set() {
        return Some(OfflineReason::KillSwitch);
    }
    if cfg.mode != Some(inkentry_core::config::SyncMode::Offline) {
        return None;
    }
    // `INKENTRY_MODE` overwrites `cfg.mode` at load; only the environment says
    // which source is in force, and that decides which advice is actionable.
    Some(match std::env::var("INKENTRY_MODE") {
        Ok(_) => OfflineReason::ModeOfflineEnv,
        Err(_) => OfflineReason::ModeOfflineConfig,
    })
}

// Fixed for the process lifetime: right for a one-shot CLI, wrong for a daemon
// serving several configs.
pub async fn get_tier(cfg: &Config) -> &'static Tier {
    let explicit_offline = explicit_offline_reason(cfg);
    let url = cfg.server_url.clone();
    let server_ca = cfg.server_ca.clone();
    TIER.get_or_init(|| async move {
        if let Some(reason) = explicit_offline {
            tracing::debug!("sync mode is explicitly offline: skipping all server probes");
            return Tier::Offline(reason);
        }
        probe(
            url.as_deref(),
            server_ca.as_deref().map(std::path::Path::new),
        )
        .await
    })
    .await
}

// Uncached, for pollers that must observe a transition (embedder loading to
// ready).
pub async fn probe_tier_fresh(cfg: &Config) -> Tier {
    if let Some(reason) = explicit_offline_reason(cfg) {
        return Tier::Offline(reason);
    }
    probe(
        cfg.server_url.as_deref(),
        cfg.server_ca.as_deref().map(std::path::Path::new),
    )
    .await
}

const REMOTE_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

const LOOPBACK_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

async fn probe(url: Option<&str>, server_ca: Option<&std::path::Path>) -> Tier {
    if inkentry_core::config::no_server_env_set() {
        tracing::debug!("INKENTRY_NO_SERVER set: skipping all server probes");
        return Tier::Offline(OfflineReason::KillSwitch);
    }

    if let Some(url) = url {
        return match probe_url(url, REMOTE_PROBE_TIMEOUT, false, server_ca).await {
            Ok(tier) => tier,
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(2);
            }
        };
    }

    probe_loopback().await
}

// Test-only: `INKENTRY_TEST_DISCOVERY_PORT` overrides the default port; `0` or
// an unparseable value disables the fallback. Pointing `INKENTRY_STATE_DIR`
// at an empty dir does not stop the fallback probing the developer's own
// daemon on the default port.
fn discovery_fallback_port() -> Option<u16> {
    let Ok(raw) = std::env::var("INKENTRY_TEST_DISCOVERY_PORT") else {
        return Some(DEFAULT_SERVER_PORT);
    };
    match raw.trim().parse::<u16>() {
        Ok(0) => None,
        Ok(port) => Some(port),
        // Fail closed: a typo like `o` for `0` must not restore the fallback.
        Err(_) => {
            tracing::warn!(
                "INKENTRY_TEST_DISCOVERY_PORT={raw:?} is not a port number; \
                 disabling loopback discovery's fallback rather than restoring it"
            );
            None
        }
    }
}

// Never consults `cfg.server_url`. Probe failures are `Tier::Offline`, not
// errors: no local server is the normal case. Once a port is recorded it
// decides the answer: a responder failing the identity checks must not fall
// through to the default port, where whatever holds the recorded port
// usually answers too and nothing is verified.
async fn probe_loopback() -> Tier {
    if let Some(port) = read_server_port_file() {
        let loopback_url = format!("http://127.0.0.1:{port}");
        tracing::debug!(
            "loopback auto-discovery: found server.port={port}, probing {loopback_url}"
        );
        // Plaintext http on loopback, so no custom CA applies.
        let (tier, reported_instance_id) =
            probe_url_reporting_instance_id(&loopback_url, LOOPBACK_PROBE_TIMEOUT, true, None)
                .await
                .unwrap_or((Tier::Offline(OfflineReason::NoLocalServer), None));

        // Announced rather than logged: the user started a local server and
        // this run is not using it, which a dropped `tracing` line hides.
        let refused = match tier {
            // Keeps its own reason and advice: that daemon answered.
            Tier::Offline(OfflineReason::LocalServerUnusable) => return tier,
            Tier::Offline(_) => format!(
                "the local server recorded in {} did not answer on 127.0.0.1:{port}",
                state_dir_for_message()
            ),
            Tier::Server { .. } => match untrusted_responder(reported_instance_id.as_deref()) {
                Some(why) => format!(
                    "the process answering 127.0.0.1:{port} is not the server recorded in \
                     {}: {why}. Nothing was sent to it",
                    state_dir_for_message()
                ),
                None => return tier,
            },
        };

        crate::notice::enotice!(
            "warning: {refused}. Embeddings are offline for this run: run \
             `inkentry server stop`, then `inkentry server start`."
        );
        return Tier::Offline(OfflineReason::RecordedServerUnreachable);
    }

    let Some(port) = discovery_fallback_port() else {
        tracing::debug!("loopback auto-discovery: fallback disabled: offline mode");
        return Tier::Offline(OfflineReason::NoLocalServer);
    };
    let default_url = format!("http://127.0.0.1:{port}");
    tracing::debug!("loopback auto-discovery: probing default {default_url}");
    probe_url(&default_url, LOOPBACK_PROBE_TIMEOUT, true, None)
        .await
        .unwrap_or(Tier::Offline(OfflineReason::NoLocalServer))
}

// Routes inference, which can differ from `get_tier`. Under `local_first` an
// explicit `server_url` is only a memory replica, so inference goes to the
// loopback embedder; only `cloud_first` serves inference from `server_url`.
pub async fn get_inference_tier(cfg: &Config) -> Tier {
    inference_tier(cfg, CloudBranchProbe::Cached).await
}

// As `get_inference_tier`, but re-probes `server_url` on every call so a
// poller can observe the embedder go from loading to ready.
pub async fn get_inference_tier_fresh(cfg: &Config) -> Tier {
    inference_tier(cfg, CloudBranchProbe::Fresh).await
}

enum CloudBranchProbe {
    Cached,
    Fresh,
}

async fn inference_tier(cfg: &Config, cloud_branch: CloudBranchProbe) -> Tier {
    if let Some(reason) = explicit_offline_reason(cfg) {
        return Tier::Offline(reason);
    }
    if cfg.resolve_mode() == inkentry_core::config::SyncMode::CloudFirst {
        return match cloud_branch {
            CloudBranchProbe::Cached => get_tier(cfg).await.clone(),
            CloudBranchProbe::Fresh => probe_tier_fresh(cfg).await,
        };
    }
    probe_loopback().await
}

async fn probe_url(
    url: &str,
    timeout: std::time::Duration,
    auto_discovered: bool,
    server_ca: Option<&std::path::Path>,
) -> Result<Tier, String> {
    probe_url_reporting_instance_id(url, timeout, auto_discovered, server_ca)
        .await
        .map(|(tier, _)| tier)
}

async fn probe_url_reporting_instance_id(
    url: &str,
    timeout: std::time::Duration,
    auto_discovered: bool,
    server_ca: Option<&std::path::Path>,
) -> Result<(Tier, Option<String>), String> {
    inkentry_core::config::validate_transport_url(url)?;

    let unreached = if auto_discovered {
        OfflineReason::NoLocalServer
    } else {
        OfflineReason::ExplicitServerUnavailable
    };

    // Latency shortcut only: the probe would spend a connect timeout to return
    // this same tier.
    if inkentry_core::reachability::connect_already_failed(url) {
        return Ok((Tier::Offline(unreached), None));
    }

    let builder =
        match inkentry_core::config::apply_server_ca(reqwest::Client::builder(), server_ca) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("could not load custom CA for server probe: {e}");
                return Ok((Tier::Offline(unreached), None));
            }
        };
    // Half the budget for connect, so a host that never answers is
    // distinguishable from a slow server; only the former may be memoised as
    // unreachable.
    let client = match builder
        .connect_timeout(timeout / 2)
        .timeout(timeout)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("could not build HTTP client for server probe: {e}");
            return Ok((Tier::Offline(unreached), None));
        }
    };

    // Unauthenticated endpoint: never send a bearer.
    let req = client.get(format!("{}/v1/health", url.trim_end_matches('/')));

    match req.send().await {
        Ok(resp) if resp.status().is_success() => {
            let health = parse_health(url, resp).await;

            let server_dim = health.embedding_dim;
            if health.caps.index_embed && server_dim != 0 {
                let expected = inkentry_core::embeddings::EMBEDDING_DIM;
                if server_dim != expected {
                    if auto_discovered {
                        // Soft downgrade: the user did not configure this server.
                        tracing::warn!(
                            "inkentry-server at {url} serves {server_dim}-dim embeddings; \
                             this CLI expects {expected}-dim. Ignoring loopback server. \
                             Restart the server (`inkentry server start`) or set \
                             INKENTRY_NO_SERVER=1 to suppress this probe."
                        );
                        return Ok((Tier::Offline(OfflineReason::LocalServerUnusable), None));
                    } else {
                        return Err(format!(
                            "inkentry-server at {url} serves {server_dim}-dim embeddings; \
                             this CLI expects {expected}-dim.\n\
                             Upgrade or replace the server, or remove server_url from \
                             ~/.config/inkentry/config.toml."
                        ));
                    }
                }
            }

            Ok((
                Tier::Server {
                    url: url.to_string(),
                    caps: health.caps,
                    auto_discovered,
                    embedder_state: health.embedder_state,
                    server_limits: health.server_limits,
                },
                health.instance_id,
            ))
        }
        Ok(resp) => {
            if !auto_discovered {
                tracing::warn!(
                    "inkentry-server at {url} returned {}: running in offline mode",
                    resp.status()
                );
            }
            Ok((Tier::Offline(unreached), None))
        }
        Err(e) => {
            // Memoise genuine connect misses so later requests skip their own connect
            // timeout. Not TLS failures: that server answered, and a later handshake
            // must report the certificate cause.
            if e.is_connect() && find_rustls_cause(&e).is_none() {
                inkentry_core::reachability::record_connect_failure(url);
            }
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
                            "inkentry-server at {url} reachable, but TLS trust failed: {cause}; \
                             running in offline mode.\n  full error chain: {chain}{hint}"
                        );
                    }
                    None => {
                        record_explicit_probe_failure(ConnFailure::Unreachable);
                        tracing::warn!(
                            "inkentry-server at {url} unreachable, running in offline mode: {chain}"
                        );
                    }
                }
            }
            Ok((Tier::Offline(unreached), None))
        }
    }
}

// Degrades an unreadable value to the field's default instead of failing the
// body: `parse_health` treats any deserialize error as a legacy plain-text
// server, so one strict field would discard capabilities, dim and limits
// together. The value is never logged: it is peer-controlled and unbounded,
// and serde's own error quotes it in full; only its JSON kind is.
fn lenient_or_default<'de, D, T>(de: D, field: &str, expected: &str) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned + Default,
{
    use serde::Deserialize;

    let raw = serde_json::Value::deserialize(de)?;
    match T::deserialize(&raw) {
        Ok(value) => Ok(value),
        Err(_) => {
            let kind = json_kind(&raw);
            tracing::warn!(
                "ignoring unreadable /v1/health field `{field}`: expected {expected}, \
                 got {kind}. Falling back to this CLI's default for it and keeping the \
                 rest of the body. The value is not logged"
            );
            Ok(T::default())
        }
    }
}

fn json_kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

macro_rules! lenient_health_field {
    ($fn_name:ident, $ty:ty, $wire_name:literal, $expected:literal) => {
        fn $fn_name<'de, D>(de: D) -> Result<$ty, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            lenient_or_default::<D, $ty>(de, $wire_name, $expected)
        }
    };
}

lenient_health_field!(
    lenient_capabilities,
    Vec<String>,
    "capabilities",
    "an array of capability strings"
);
lenient_health_field!(
    lenient_instance_id,
    Option<String>,
    "instance_id",
    "a string"
);
lenient_health_field!(
    lenient_started_by,
    Option<u32>,
    "started_by",
    "a non-negative integer uid"
);
lenient_health_field!(
    lenient_embedding_dim,
    usize,
    "embedding_dim",
    "a non-negative integer"
);
lenient_health_field!(
    lenient_embedder,
    Option<EmbedderBody>,
    "embedder",
    "an object carrying a `state`"
);
lenient_health_field!(
    lenient_embedder_state,
    EmbedderState,
    "embedder.state",
    "a known embedder state string"
);
lenient_health_field!(
    lenient_limits,
    Option<ServerLimitsBody>,
    "limits",
    "an object of server-enforced limits"
);
lenient_health_field!(
    lenient_embed_request_timeout_secs,
    Option<u64>,
    "limits.embed_request_timeout_secs",
    "a non-negative integer number of seconds"
);
lenient_health_field!(
    lenient_max_batch_chunks,
    Option<usize>,
    "limits.max_batch_chunks",
    "a non-negative integer number of chunks"
);
lenient_health_field!(
    lenient_embedder_token_cap,
    Option<usize>,
    "limits.embedder_token_cap",
    "a non-negative integer number of tokens"
);
lenient_health_field!(
    lenient_embed_threads,
    Option<usize>,
    "limits.embed_threads",
    "a non-negative integer number of threads"
);
lenient_health_field!(
    lenient_accepts_pushed_vectors,
    bool,
    "accepts_pushed_vectors",
    "a boolean"
);

#[derive(serde::Deserialize)]
struct EmbedderBody {
    #[serde(default, deserialize_with = "lenient_embedder_state")]
    state: EmbedderState,
}

// Members are read one at a time: losing an advertised `max_batch_chunks` to
// an unreadable sibling would make the embed phase plan around 256 and draw a
// `413`.
#[derive(serde::Deserialize)]
struct ServerLimitsBody {
    #[serde(default, deserialize_with = "lenient_embed_request_timeout_secs")]
    embed_request_timeout_secs: Option<u64>,
    #[serde(default, deserialize_with = "lenient_max_batch_chunks")]
    max_batch_chunks: Option<usize>,
    #[serde(default, deserialize_with = "lenient_embedder_token_cap")]
    embedder_token_cap: Option<usize>,
    #[serde(default, deserialize_with = "lenient_embed_threads")]
    embed_threads: Option<usize>,
}

impl From<ServerLimitsBody> for ServerLimits {
    fn from(body: ServerLimitsBody) -> Self {
        ServerLimits {
            embed_request_timeout_secs: body.embed_request_timeout_secs,
            max_batch_chunks: body.max_batch_chunks,
            embedder_token_cap: body.embedder_token_cap,
            embed_threads: body.embed_threads,
        }
    }
}

#[derive(serde::Deserialize)]
struct HealthBody {
    #[serde(default, deserialize_with = "lenient_capabilities")]
    capabilities: Vec<String>,
    #[serde(default, deserialize_with = "lenient_instance_id")]
    instance_id: Option<String>,
    #[serde(default, deserialize_with = "lenient_started_by")]
    started_by: Option<u32>,
    #[serde(default, deserialize_with = "lenient_embedding_dim")]
    embedding_dim: usize,
    #[serde(default, deserialize_with = "lenient_embedder")]
    embedder: Option<EmbedderBody>,
    #[serde(default, deserialize_with = "lenient_limits")]
    limits: Option<ServerLimitsBody>,
    #[serde(default, deserialize_with = "lenient_accepts_pushed_vectors")]
    accepts_pushed_vectors: bool,
}

struct HealthFacts {
    caps: Capabilities,
    // 0 when absent or no embedder is loaded, which skips the dimension check.
    embedding_dim: usize,
    embedder_state: EmbedderState,
    // `None` means a server predating the field, not "unlimited".
    server_limits: Option<ServerLimits>,
    // Peer-reported, so it identifies nothing alone.
    instance_id: Option<String>,
}

fn legacy_plain_text_health() -> HealthFacts {
    HealthFacts {
        caps: Capabilities::legacy_memory_only(),
        embedding_dim: 0,
        embedder_state: EmbedderState::Unknown,
        server_limits: None,
        instance_id: None,
    }
}

fn bounded_for_log(text: &str) -> String {
    let head: String = text.chars().take(200).collect();
    if head.len() < text.len() {
        format!("{head}...")
    } else {
        head
    }
}

fn health_body_snippet(raw: &[u8]) -> String {
    bounded_for_log(&String::from_utf8_lossy(raw))
}

async fn parse_health(url: &str, resp: reqwest::Response) -> HealthFacts {
    let raw = match resp.bytes().await {
        Ok(raw) => raw,
        Err(e) => {
            tracing::warn!(
                "could not read the /v1/health body from inkentry-server at {url} ({e}): \
                 treating it as a legacy plain-text server, so semantic search, \
                 index embed and harvest will be reported unavailable"
            );
            return legacy_plain_text_health();
        }
    };

    // Two steps so the failure can be described without rendering serde's
    // error, which quotes the whole body back. A `serde_json` syntax error
    // carries no input, so that one is safe (and bounded anyway).
    let value = match serde_json::from_slice::<serde_json::Value>(&raw) {
        Ok(value) => value,
        Err(e) => {
            tracing::warn!(
                "could not parse the /v1/health body from inkentry-server at {url} as \
                 JSON ({}): treating it as a legacy plain-text server, so semantic \
                 search, index embed and harvest will be reported unavailable and any \
                 advertised limits ignored. body: {}",
                bounded_for_log(&e.to_string()),
                health_body_snippet(&raw)
            );
            return legacy_plain_text_health();
        }
    };

    match <HealthBody as serde::Deserialize>::deserialize(&value) {
        Ok(body) => {
            let embedder_state = body
                .embedder
                .as_ref()
                .map(|e| e.state)
                .unwrap_or(EmbedderState::Unknown);
            // `eprintln` rather than `tracing`: the one health fact the user must act
            // on, and `warn!` is off at the default log level.
            if let Some(server_uid) = body.started_by {
                let my_uid = current_uid();
                if let Some(my_uid) = my_uid
                    && my_uid != server_uid
                {
                    let warning = format!(
                        "inkentry-server at {url} was started by UID {server_uid} \
                         but you are UID {my_uid}; on a multi-user host this may \
                         expose another user's memory: consider running your own server"
                    );
                    // Bypasses the notice sink so no output flag can hide a possible
                    // exposure of another user's memory.
                    eprintln!("warning: {warning}");
                    tracing::warn!("{warning}");
                }
            }
            if let Some(ref id) = body.instance_id {
                tracing::debug!("server instance_id: {id}");
            }
            let cap_strs: Vec<&str> = body.capabilities.iter().map(String::as_str).collect();
            let mut caps = Capabilities::from_server_caps(&cap_strs);
            caps.accepts_pushed_vectors = body.accepts_pushed_vectors;
            HealthFacts {
                caps,
                embedding_dim: body.embedding_dim,
                embedder_state,
                server_limits: body.limits.map(ServerLimits::from),
                instance_id: body.instance_id,
            }
        }
        Err(_) => {
            // Each field degrades on its own, so reaching here means valid JSON that
            // is not a health object at all.
            tracing::warn!(
                "could not parse the /v1/health body from inkentry-server at {url}: it \
                 is {}, not a health object. Treating it as a legacy plain-text \
                 server, so semantic search, index embed and harvest will be reported \
                 unavailable and any advertised limits ignored. body: {}",
                json_kind(&value),
                health_body_snippet(&raw)
            );
            legacy_plain_text_health()
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::super::diagnostics::{
        explicit_probe_failure, reset_explicit_probe_failure_for_test,
    };
    use super::*;

    #[test]
    fn read_server_port_file_returns_none_when_absent() {
        let _ = read_server_port_file(); // must not panic
    }

    #[test]
    fn loopback_probe_timeout_is_250ms() {
        assert_eq!(LOOPBACK_PROBE_TIMEOUT.as_millis(), 250);
    }

    #[test]
    fn remote_probe_timeout_is_2s() {
        assert_eq!(REMOTE_PROBE_TIMEOUT.as_secs(), 2);
    }

    #[test]
    fn default_loopback_port_is_4655() {
        assert_eq!(DEFAULT_SERVER_PORT, 4655);
    }

    #[test]
    #[serial_test::serial(inkentry_test_discovery_port_env)]
    fn discovery_fallback_port_defaults_to_the_server_port() {
        // SAFETY: serialised via #[serial]; restored before the test ends.
        unsafe { std::env::remove_var("INKENTRY_TEST_DISCOVERY_PORT") };
        assert_eq!(discovery_fallback_port(), Some(DEFAULT_SERVER_PORT));
    }

    #[test]
    #[serial_test::serial(inkentry_test_discovery_port_env)]
    fn discovery_fallback_port_zero_disables_the_fallback() {
        unsafe { std::env::set_var("INKENTRY_TEST_DISCOVERY_PORT", "0") };
        assert_eq!(discovery_fallback_port(), None);
        unsafe { std::env::remove_var("INKENTRY_TEST_DISCOVERY_PORT") };
    }

    #[test]
    #[serial_test::serial(inkentry_test_discovery_port_env)]
    fn discovery_fallback_port_honours_an_explicit_port() {
        unsafe { std::env::set_var("INKENTRY_TEST_DISCOVERY_PORT", " 49999 ") };
        assert_eq!(discovery_fallback_port(), Some(49999));
        unsafe { std::env::remove_var("INKENTRY_TEST_DISCOVERY_PORT") };
    }

    #[test]
    #[serial_test::serial(inkentry_test_discovery_port_env)]
    fn discovery_fallback_port_fails_closed_on_an_unparseable_value() {
        for typo in ["o", "not-a-port", "-1", "99999", ""] {
            unsafe { std::env::set_var("INKENTRY_TEST_DISCOVERY_PORT", typo) };
            assert_eq!(
                discovery_fallback_port(),
                None,
                "a malformed override ({typo:?}) must disable the fallback, not restore it"
            );
        }
        unsafe { std::env::remove_var("INKENTRY_TEST_DISCOVERY_PORT") };
    }

    #[tokio::test]
    #[serial_test::serial(inkentry_no_server_env)]
    async fn inkentry_no_server_forces_offline() {
        // SAFETY: serialised via #[serial] so no other test reads/writes this
        // env var concurrently; restored before the guard scope ends.
        for val in ["1", "true", "yes"] {
            unsafe { std::env::set_var("INKENTRY_NO_SERVER", val) };
            // No server_url, so without the short-circuit this would try loopback
            // discovery.
            let tier = probe(None, None).await;
            assert!(
                matches!(tier, Tier::Offline(OfflineReason::KillSwitch)),
                "INKENTRY_NO_SERVER={val} should force the kill-switch reason, got {tier:?}"
            );
        }
        unsafe { std::env::remove_var("INKENTRY_NO_SERVER") };
    }

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

    #[tokio::test]
    async fn probe_loopback_dim_mismatch_downgrades_to_offline() {
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

        let result = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true, None).await;
        assert!(
            matches!(
                result,
                Ok(Tier::Offline(OfflineReason::LocalServerUnusable))
            ),
            "a loopback server with the wrong dim is a daemon to restart, not a missing \
             one; got {result:?}"
        );
    }

    #[tokio::test]
    async fn probe_loopback_dim_match_returns_server() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body(
                &["memory", "index.embed", "search.semantic"],
                inkentry_core::embeddings::EMBEDDING_DIM,
            )))
            .mount(&server)
            .await;

        let result = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true, None).await;
        assert!(
            matches!(result, Ok(Tier::Server { .. })),
            "auto-discovered loopback with correct dim must return Server; got {result:?}"
        );
    }

    #[tokio::test]
    async fn probe_url_parses_accepts_pushed_vectors_true() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let mut body = health_body(&["memory"], inkentry_core::embeddings::EMBEDDING_DIM);
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
                inkentry_core::embeddings::EMBEDDING_DIM,
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

    // Neither end mocked: a mock is written from one side's expectations and
    // cannot catch the two sides disagreeing.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_real_server_advertises_the_capability_and_a_real_push_skips_its_embedder() {
        use std::sync::OnceLock;
        use std::sync::atomic::{AtomicUsize, Ordering};

        static SQLITE_VEC: OnceLock<()> = OnceLock::new();
        SQLITE_VEC.get_or_init(|| {
            #[allow(clippy::missing_transmute_annotations)]
            unsafe {
                rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                    sqlite_vec::sqlite3_vec_init as *const (),
                )));
            }
        });

        let dim = inkentry_core::embeddings::EMBEDDING_DIM;

        struct CountingEmbedder {
            dim: usize,
            calls: std::sync::Arc<AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl inkentry_core::embeddings::EmbeddingBackend for CountingEmbedder {
            async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
                self.calls.fetch_add(texts.len(), Ordering::SeqCst);
                Ok(texts.iter().map(|_| vec![0.0_f32; self.dim]).collect())
            }

            fn dimension(&self) -> usize {
                self.dim
            }
        }

        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let db = inkentry_server::db::ServerDb::open(
            std::path::Path::new(":memory:"),
            dim,
            inkentry_core::embeddings::MODEL_ID,
        )
        .expect("open server db");
        let instance_id = db.get_or_create_instance_id().expect("instance_id");
        let state = inkentry_server::AppState {
            db: std::sync::Arc::new(tokio::sync::Mutex::new(db)),
            auth: std::sync::Arc::new(inkentry_server::auth::ApiKeyAuth::new(None)),
            conflict_threshold: inkentry_server::default_conflict_threshold(),
            embedder: inkentry_server::EmbedderSlot::ready(std::sync::Arc::new(CountingEmbedder {
                dim,
                calls: calls.clone(),
            })),
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
            relay: inkentry_server::relay::RelayRegistry::disabled(),
            repair_signal: inkentry_server::repair::RepairSignal::new(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral loopback port");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                inkentry_server::router(state)
                    .into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await;
        });
        let base_url = format!("http://{addr}");

        let tier = probe_url(&base_url, REMOTE_PROBE_TIMEOUT, false, None)
            .await
            .expect("probe must succeed against a real server");
        let caps = tier
            .caps()
            .expect("a reachable server must report capabilities");
        assert!(
            caps.accepts_pushed_vectors,
            "a real server with a ready embedder must advertise the capability the \
             push gate reads"
        );

        let client =
            inkentry_core::storage::CloudSyncClient::new(&base_url, "acme-widget", None, None)
                .expect("build push client");
        let item = inkentry_core::storage::BatchPushItem {
            id: None,
            kind: "decision".into(),
            title: "pushed with its own vector".into(),
            body: Some("b".into()),
            external_id: "ext-pushed-1".into(),
            source_commit: None,
            vector: None,
            vector_model: None,
            vector_precision: None,
        }
        .maybe_attach_vector(
            caps.accepts_pushed_vectors,
            Some(vec![1.0 / (dim as f32).sqrt(); dim]),
        );
        assert!(
            item.vector.is_some(),
            "the advertised capability must actually open the push gate"
        );

        let result = client
            .push_batch(vec![item])
            .await
            .expect("push must succeed");
        assert_eq!(
            result.created, 1,
            "the pushed entry must be stored: {result:?}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "an entry that arrived with its own vector must never be re-embedded"
        );
    }

    #[tokio::test]
    async fn probe_url_parses_server_limits_when_present() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let mut body = health_body(
            &["memory", "index.embed", "search.semantic"],
            inkentry_core::embeddings::EMBEDDING_DIM,
        );
        body["limits"] = serde_json::json!({
            "embed_request_timeout_secs": 1800,
            "max_batch_chunks": 256,
            "embedder_token_cap": 5792,
            "embed_threads": 1,
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
        assert_eq!(limits.embed_request_timeout_secs, Some(1800));
        assert_eq!(limits.max_batch_chunks, Some(256));
        assert_eq!(limits.embedder_token_cap, Some(5792));
        assert_eq!(
            limits.embed_threads,
            Some(1),
            "a single-threaded budget must reach the CLI, since that is the one \
             value status turns into advice"
        );
    }

    #[tokio::test]
    async fn probe_url_server_limits_none_when_absent_legacy_server() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body(
                &["memory", "index.embed", "search.semantic"],
                inkentry_core::embeddings::EMBEDDING_DIM,
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

    #[tokio::test]
    async fn probe_loopback_dim_zero_no_embedder_returns_server() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
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
        let expected = inkentry_core::embeddings::EMBEDDING_DIM;
        assert!(
            msg.contains(&expected.to_string()),
            "error must mention the expected dim ({expected}): {msg}"
        );
        assert!(
            msg.contains("server_url"),
            "error must mention 'server_url' for actionable guidance: {msg}"
        );
    }

    #[tokio::test]
    async fn probe_url_rejects_non_loopback_http_no_request_sent() {
        // No listener: a sent request would give a connection error, not this
        // validation message.
        let result = probe_url("http://team-server:4655", REMOTE_PROBE_TIMEOUT, false, None).await;
        let err = result.expect_err("non-loopback http:// must be a hard error");
        assert!(err.contains("loopback"), "got: {err}");
        assert!(err.contains("https"), "got: {err}");
    }

    #[tokio::test]
    async fn probe_url_rejects_non_loopback_http_even_when_auto_discovered() {
        let result = probe_url(
            "http://team-server:4655",
            LOOPBACK_PROBE_TIMEOUT,
            true,
            None,
        )
        .await;
        assert!(result.is_err());
    }

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

        let result = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, false, None).await;
        assert!(
            matches!(result, Ok(Tier::Server { .. })),
            "loopback http:// must be accepted; got {result:?}"
        );
    }

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

        let requests = server.received_requests().await.expect("requests recorded");
        assert_eq!(requests.len(), 1);
        assert!(
            !requests[0].headers.contains_key("authorization"),
            "the /v1/health probe must not send an Authorization header"
        );
    }

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

    #[tokio::test]
    async fn probe_url_absent_embedder_field_is_unknown() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
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

    // A later probe must not swap a fresh result in under a stale
    // `EXPLICIT_PROBE_FAILURE` annotation, so `get_tier` may probe only once.
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
        assert!(
            matches!(
                first,
                Tier::Offline(OfflineReason::ExplicitServerUnavailable)
            ),
            "got {first:?}"
        );
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

    #[tokio::test]
    #[serial_test::serial(explicit_probe_failure)]
    async fn probe_url_explicit_connection_refused_sets_unreachable_not_tls() {
        reset_explicit_probe_failure_for_test();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind free port");
        let port = listener.local_addr().expect("local_addr").port();
        drop(listener);

        let url = format!("http://127.0.0.1:{port}");
        let result = probe_url(&url, REMOTE_PROBE_TIMEOUT, false, None).await;
        assert!(
            matches!(
                result,
                Ok(Tier::Offline(OfflineReason::ExplicitServerUnavailable))
            ),
            "got {result:?}"
        );
        assert_eq!(
            explicit_probe_failure(),
            Some(ConnFailure::Unreachable),
            "connection-refused must not be mislabelled as a TLS trust failure"
        );
    }

    #[tokio::test]
    #[serial_test::serial(explicit_probe_failure)]
    async fn probe_url_explicit_timeout_sets_unreachable_not_tls() {
        reset_explicit_probe_failure_for_test();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind free port");
        let port = listener.local_addr().expect("local_addr").port();
        std::thread::spawn(move || {
            // Hold connections open without replying so the client timeout fires.
            for stream in listener.incoming().flatten() {
                std::thread::sleep(std::time::Duration::from_secs(5));
                drop(stream);
            }
        });

        let url = format!("http://127.0.0.1:{port}");
        let result = probe_url(&url, std::time::Duration::from_millis(100), false, None).await;
        assert!(
            matches!(
                result,
                Ok(Tier::Offline(OfflineReason::ExplicitServerUnavailable))
            ),
            "got {result:?}"
        );
        assert_eq!(
            explicit_probe_failure(),
            Some(ConnFailure::Unreachable),
            "a timeout must not be mislabelled as a TLS trust failure"
        );
    }

    #[tokio::test]
    #[serial_test::serial(explicit_probe_failure)]
    async fn probe_url_explicit_non_success_status_does_not_set_any_probe_failure() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Reset rather than rely on execution order.
        reset_explicit_probe_failure_for_test();

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let result = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, false, None).await;
        assert!(
            matches!(
                result,
                Ok(Tier::Offline(OfflineReason::ExplicitServerUnavailable))
            ),
            "got {result:?}"
        );
        assert_eq!(
            explicit_probe_failure(),
            None,
            "a reachable server answering with a non-2xx status must not populate \
             EXPLICIT_PROBE_FAILURE: that would render a stale/wrong [tls:] or \
             [unreachable] label for a request that was neither"
        );
    }

    #[tokio::test]
    #[serial_test::serial(explicit_probe_failure)]
    async fn probe_url_auto_discovered_connection_refused_leaves_probe_failure_unset() {
        // Reset rather than rely on execution order.
        reset_explicit_probe_failure_for_test();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind free port");
        let port = listener.local_addr().expect("local_addr").port();
        drop(listener);

        let url = format!("http://127.0.0.1:{port}");
        let result = probe_url(&url, LOOPBACK_PROBE_TIMEOUT, true, None).await;
        assert!(
            matches!(result, Ok(Tier::Offline(OfflineReason::NoLocalServer))),
            "got {result:?}"
        );
        assert_eq!(
            explicit_probe_failure(),
            None,
            "loopback auto-discovery misses must never populate EXPLICIT_PROBE_FAILURE"
        );
    }

    // `get_inference_tier` reads `INKENTRY_NO_SERVER`, so these share the serial
    // group of the tests that set it.

    // The mock is found via the default-port fallback; the recorded-state path
    // also needs a live `inkentry-server` pid, pinned separately below.
    #[tokio::test]
    #[serial_test::serial(inkentry_no_server_env)]
    async fn get_inference_tier_local_first_prefers_loopback_over_explicit_server_url() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        unsafe { std::env::remove_var("INKENTRY_NO_SERVER") };

        let loopback = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body(&["memory"], 0)))
            .mount(&loopback)
            .await;

        let loopback_port: u16 = loopback
            .uri()
            .rsplit(':')
            .next()
            .expect("uri has a port")
            .trim_end_matches('/')
            .parse()
            .expect("uri port is numeric");

        let tmp = tempfile::TempDir::new().unwrap();
        let state_dir = tmp.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();

        let prev_state_dir = std::env::var_os("INKENTRY_STATE_DIR");
        let prev_discovery_port = std::env::var_os("INKENTRY_TEST_DISCOVERY_PORT");
        unsafe {
            std::env::set_var("INKENTRY_STATE_DIR", &state_dir);
            std::env::set_var("INKENTRY_TEST_DISCOVERY_PORT", loopback_port.to_string());
        }

        let cfg = Config {
            // Never mocked: a fallback to it would surface as a connection error, not
            // a silent pass.
            server_url: Some("https://cloud.invalid.example:1".to_string()),
            project_id: Some("team/proj".to_string()),
            mode: None, // defaults to local_first because server_url is set
            ..Default::default()
        };
        assert_eq!(
            cfg.resolve_mode(),
            inkentry_core::config::SyncMode::LocalFirst
        );

        let tier = get_inference_tier(&cfg).await;

        unsafe {
            match prev_state_dir {
                Some(v) => std::env::set_var("INKENTRY_STATE_DIR", v),
                None => std::env::remove_var("INKENTRY_STATE_DIR"),
            }
            match prev_discovery_port {
                Some(v) => std::env::set_var("INKENTRY_TEST_DISCOVERY_PORT", v),
                None => std::env::remove_var("INKENTRY_TEST_DISCOVERY_PORT"),
            }
        }

        assert_eq!(
            tier.server_url(),
            Some(format!("http://127.0.0.1:{loopback_port}")).as_deref(),
            "local_first must route inference to the loopback server, not the \
             configured (and unreachable) server_url; got {tier:?}"
        );
    }

    // Uses `cfg.mode` rather than `INKENTRY_NO_SERVER`: that variable is
    // process-global and read by concurrent tests outside this serial group.
    #[tokio::test]
    async fn get_inference_tier_explicit_offline_short_circuits() {
        let cfg = Config {
            server_url: Some("https://cloud.invalid.example:1".to_string()),
            project_id: Some("team/proj".to_string()),
            mode: Some(inkentry_core::config::SyncMode::Offline),
            ..Default::default()
        };
        let tier = get_inference_tier(&cfg).await;
        assert!(
            matches!(tier, Tier::Offline(OfflineReason::ModeOfflineConfig)),
            "got {tier:?}"
        );
    }

    // Re-probing is the one difference from `get_inference_tier`, whose
    // `cloud_first` branch reuses `get_tier`'s cache. Two calls against a
    // changing mock, since a single call cannot tell the two apart. Avoids the
    // shared `TIER` cell, which has no reset hook.
    #[tokio::test]
    #[serial_test::serial(inkentry_no_server_env)]
    async fn get_inference_tier_fresh_cloud_first_reprobes_every_call() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        unsafe { std::env::remove_var("INKENTRY_NO_SERVER") };

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(health_body_with_embedder("loading")),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(health_body(
                &["memory", "index.embed", "search.semantic"],
                inkentry_core::embeddings::EMBEDDING_DIM,
            )))
            .mount(&server)
            .await;

        let cfg = Config {
            server_url: Some(server.uri()),
            project_id: Some("team/proj".to_string()),
            mode: Some(inkentry_core::config::SyncMode::CloudFirst),
            ..Default::default()
        };

        let first = get_inference_tier_fresh(&cfg).await;
        assert_eq!(
            first.embedder_state(),
            Some(EmbedderState::Loading),
            "first call must observe the first mock response; got {first:?}"
        );

        let second = get_inference_tier_fresh(&cfg).await;
        assert!(
            matches!(second.caps(), Some(c) if c.index_embed),
            "second call must re-probe and observe the loading -> ready \
             transition, not return a value pinned by the first call; got {second:?}"
        );
    }

    // Bodies recorded from released `inkentry-server` binaries, which can
    // contradict the shape `health_body()` assumes.

    fn recorded_health(name: &str) -> serde_json::Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/skew")
            .join(name);
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read recorded fixture {}: {e}", path.display()));
        serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("parse recorded fixture {}: {e}", path.display()))
    }

    async fn probe_recorded(body: serde_json::Value) -> Tier {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true, None)
            .await
            .expect("probe of a recorded peer body must succeed")
    }

    // The older recorded bodies omit `embedder`, `embedding_dim` and `limits`;
    // each must take its conservative default, not read as unlimited.
    #[tokio::test]
    async fn recorded_legacy_peers_degrade_to_documented_defaults() {
        for name in ["health-v0.8.0.json", "health-v0.9.0.json"] {
            let body = recorded_health(name);
            assert!(
                body.get("limits").is_none() && body.get("embedder").is_none(),
                "{name} is supposed to be the absent-optionals fixture, but it \
                 carries those fields; re-record it or fix the test's premise"
            );

            let tier = probe_recorded(body).await;
            assert_eq!(
                tier.embedder_state(),
                Some(EmbedderState::Unknown),
                "{name}: an absent `embedder` object must read as Unknown"
            );
            assert_eq!(
                tier.server_limits(),
                None,
                "{name}: absent `limits` must stay None, never be confused with \
                 an absence of limits"
            );
        }
    }

    // Counterpart of the above: a parser that dropped every optional would pass
    // the legacy test.
    #[tokio::test]
    async fn recorded_current_peers_parse_their_optional_objects() {
        for name in ["health-v0.9.4-ready.json", "health-v0.9.5-ready.json"] {
            let tier = probe_recorded(recorded_health(name)).await;
            assert_eq!(
                tier.embedder_state(),
                Some(EmbedderState::Ready),
                "{name}: a ready embedder must be read as Ready"
            );
            let limits = tier
                .server_limits()
                .unwrap_or_else(|| panic!("{name}: `limits` was sent and must parse"));
            assert_eq!(
                limits.max_batch_chunks,
                Some(256),
                "{name}: max_batch_chunks"
            );
            assert_eq!(
                limits.embed_request_timeout_secs,
                Some(1800),
                "{name}: embed_request_timeout_secs"
            );
            assert!(
                matches!(tier.caps(), Some(c) if c.search_semantic && c.index_embed),
                "{name}: a ready peer advertising semantic capabilities must \
                 surface them"
            );
        }
    }

    // A newer peer sends fields this CLI has never heard of; they must be ignored.
    #[tokio::test]
    async fn unknown_fields_from_a_newer_peer_are_ignored() {
        let baseline = probe_recorded(recorded_health("health-v0.9.5-ready.json")).await;

        let mut body = recorded_health("health-v0.9.5-ready.json");
        let obj = body.as_object_mut().expect("health body is an object");
        obj.insert("a_field_from_the_future".into(), serde_json::json!("hello"));
        obj.insert(
            "nested_future_object".into(),
            serde_json::json!({ "deep": [1, 2, 3] }),
        );
        obj.insert("limits_v2".into(), serde_json::json!({ "unknown": true }));
        // A new enum member is the additive change most likely mistaken for a parse
        // error.
        obj["embedder"]["state"] = serde_json::json!("recalibrating");

        let with_unknowns = probe_recorded(body).await;

        assert_eq!(
            with_unknowns.server_limits(),
            baseline.server_limits(),
            "unknown sibling fields must not disturb the fields this CLI does read"
        );
        assert_eq!(
            with_unknowns.embedder_state(),
            Some(EmbedderState::Unknown),
            "an unrecognised `embedder.state` must fall back to Unknown rather \
             than failing the whole probe"
        );
    }

    // `parse_health` maps any deserialize error onto the legacy plain-text
    // branch, discarding the whole body, so one unreadable field must not take
    // its siblings with it. Asserts on the siblings, never the mutated field,
    // which may degrade to its default; `capabilities` is the unambiguous one
    // because the fallback replaces it with `legacy_memory_only()`.
    async fn assert_capabilities_survived(label: &str, mutated: serde_json::Value) {
        let tier = probe_recorded(mutated).await;
        assert!(
            matches!(tier.caps(), Some(c) if c.search_semantic && c.index_embed),
            "{label}: the whole health body was discarded, not just the field \
             under test; every advertised capability was lost with it, and \
             nothing was logged to say so"
        );
    }

    // This server family emits explicit `null` for unknown optionals
    // (`embedder.detail`, `limits.embedder_token_cap`), and `#[serde(default)]`
    // covers only a missing key, not a present `null`.
    #[tokio::test]
    async fn null_embedder_state_does_not_discard_the_rest_of_the_health_body() {
        let mut body = recorded_health("health-v0.9.5-ready.json");
        body["embedder"]["state"] = serde_json::Value::Null;
        assert_capabilities_survived("embedder.state: null", body).await;
    }

    #[tokio::test]
    async fn a_null_limit_does_not_discard_the_rest_of_the_health_body() {
        let mut body = recorded_health("health-v0.9.5-ready.json");
        body["limits"]["max_batch_chunks"] = serde_json::Value::Null;
        assert_capabilities_survived("limits.max_batch_chunks: null", body).await;
    }

    // For when `capabilities` is the field under test and may degrade to its own
    // default, so `limits` shows the rest of the body survived.
    async fn assert_limits_survived(label: &str, mutated: serde_json::Value) {
        let tier = probe_recorded(mutated).await;
        assert!(
            tier.server_limits().is_some(),
            "{label}: the whole health body was discarded, not just the field \
             under test; the server's advertised limits were lost with it"
        );
    }

    // Each mutation may lose its own field and nothing else.
    #[tokio::test]
    async fn every_health_field_degrades_alone_rather_than_taking_the_body_down() {
        let mut null_dim = recorded_health("health-v0.9.5-ready.json");
        null_dim["embedding_dim"] = serde_json::Value::Null;
        assert_capabilities_survived("embedding_dim: null", null_dim).await;

        let mut null_bool = recorded_health("health-v0.9.5-ready.json");
        null_bool["accepts_pushed_vectors"] = serde_json::Value::Null;
        assert_capabilities_survived("accepts_pushed_vectors: null", null_bool).await;

        // Informational only: a peer widening either type must not break the probe.
        let mut string_uid = recorded_health("health-v0.9.5-ready.json");
        string_uid["started_by"] = serde_json::json!("501");
        assert_capabilities_survived("started_by as a string", string_uid).await;

        let mut numeric_instance = recorded_health("health-v0.9.5-ready.json");
        numeric_instance["instance_id"] = serde_json::json!(12345);
        assert_capabilities_survived("instance_id as a number", numeric_instance).await;

        let mut limits_array = recorded_health("health-v0.9.5-ready.json");
        limits_array["limits"] = serde_json::json!([]);
        assert_capabilities_survived("limits sent as an array", limits_array).await;

        // The shape a peer that retires a limit would send.
        let mut partial_limits = recorded_health("health-v0.9.5-ready.json");
        partial_limits["limits"]
            .as_object_mut()
            .expect("limits is an object")
            .remove("embed_request_timeout_secs");
        assert_capabilities_survived("limits without embed_request_timeout_secs", partial_limits)
            .await;

        let mut scalar_embedder = recorded_health("health-v0.9.5-ready.json");
        scalar_embedder["embedder"] = serde_json::json!(5);
        assert_capabilities_survived("embedder sent as a scalar", scalar_embedder).await;

        let mut null_caps = recorded_health("health-v0.9.5-ready.json");
        null_caps["capabilities"] = serde_json::Value::Null;
        assert_limits_survived("capabilities: null", null_caps).await;
    }

    #[tokio::test]
    async fn tolerated_health_body_shapes_keep_the_rest_of_the_body() {
        let mut absent_token_cap = recorded_health("health-v0.9.5-ready.json");
        absent_token_cap["limits"]
            .as_object_mut()
            .expect("limits is an object")
            .remove("embedder_token_cap");
        assert_capabilities_survived("limits without embedder_token_cap", absent_token_cap).await;

        let mut reshaped_embedder = recorded_health("health-v0.9.5-ready.json");
        reshaped_embedder["embedder"] = serde_json::json!({ "states": [{ "name": "ready" }] });
        assert_capabilities_survived("embedder reshaped by a newer peer", reshaped_embedder).await;

        let mut null_embedder = recorded_health("health-v0.9.5-ready.json");
        null_embedder["embedder"] = serde_json::Value::Null;
        assert_capabilities_survived("embedder: null", null_embedder).await;

        let mut extra_capability = recorded_health("health-v0.9.5-ready.json");
        extra_capability["capabilities"]
            .as_array_mut()
            .expect("capabilities is an array")
            .push(serde_json::json!("a.capability.from.the.future"));
        assert_capabilities_survived("an unrecognised capability string", extra_capability).await;
    }

    // Re-derives the members from a real peer body (plus the one no peer sends
    // yet) so a member added to `HealthBody` without a lenient read is caught by
    // the same loop.

    fn unreadable_shapes() -> Vec<(&'static str, serde_json::Value)> {
        vec![
            ("a present null", serde_json::Value::Null),
            (
                "an unknown enum variant",
                serde_json::json!("a_variant_from_the_future"),
            ),
            ("a wrong scalar type", serde_json::json!(-7)),
            (
                "a malformed nested object",
                serde_json::json!({ "nested": { "deeply": [null, { "a": -1 }] } }),
            ),
        ]
    }

    // A discarded body shows as legacy capabilities, no limits and an Unknown
    // embedder; every signal except the mutated field's own is asserted.
    async fn assert_only_the_mutated_field_degraded(
        field: &str,
        shape: &str,
        mutated: serde_json::Value,
    ) {
        let tier = probe_recorded(mutated).await;
        let caps_semantic = matches!(tier.caps(), Some(c) if c.search_semantic && c.index_embed);
        let limits_read = tier.server_limits().is_some();

        assert!(
            caps_semantic || limits_read,
            "`{field}` holding {shape}: the whole health body was discarded, \
             not just that field"
        );
        if field != "capabilities" {
            assert!(
                caps_semantic,
                "`{field}` holding {shape}: the advertised capabilities went \
                 with it"
            );
        }
        if field != "limits" {
            assert!(
                limits_read,
                "`{field}` holding {shape}: the advertised limits went with it"
            );
        }
        if field != "embedder" {
            assert_eq!(
                tier.embedder_state(),
                Some(EmbedderState::Ready),
                "`{field}` holding {shape}: the embedder state went with it"
            );
        }
    }

    #[tokio::test]
    async fn every_member_of_the_recorded_health_body_degrades_alone() {
        let template = recorded_health("health-v0.9.5-ready.json");
        let mut members: std::collections::BTreeSet<String> = template
            .as_object()
            .expect("health body is an object")
            .keys()
            .cloned()
            .collect();
        // Modelled by this CLI but sent by no recorded body.
        members.insert("accepts_pushed_vectors".to_string());

        assert!(
            members.len() >= 9,
            "the recorded body no longer carries the members this test exists \
             to mutate: {members:?}"
        );

        for field in members {
            for (shape, value) in unreadable_shapes() {
                let mut body = recorded_health("health-v0.9.5-ready.json");
                body[&field] = value;
                assert_only_the_mutated_field_degraded(&field, shape, body).await;
            }
        }
    }

    // Nested members: an unreadable one must cost at most its own parent object.
    #[tokio::test]
    async fn a_malformed_nested_member_costs_at_most_its_own_parent() {
        for (parent, member) in [
            ("limits", "embed_request_timeout_secs"),
            ("limits", "max_batch_chunks"),
            ("limits", "embedder_token_cap"),
            ("embedder", "state"),
            ("embedder", "detail"),
        ] {
            for (shape, value) in unreadable_shapes() {
                let mut body = recorded_health("health-v0.9.5-ready.json");
                body[parent][member] = value;
                let tier = probe_recorded(body).await;

                assert!(
                    matches!(tier.caps(), Some(c) if c.search_semantic && c.index_embed),
                    "`{parent}.{member}` holding {shape}: the whole health body \
                     was discarded, not just `{parent}`"
                );
                if parent == "limits" {
                    assert_eq!(
                        tier.embedder_state(),
                        Some(EmbedderState::Ready),
                        "`{parent}.{member}` holding {shape}: the embedder \
                         state went with it"
                    );
                } else {
                    assert!(
                        tier.server_limits().is_some(),
                        "`{parent}.{member}` holding {shape}: the advertised \
                         limits went with it"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn one_unreadable_limit_keeps_the_readable_limits_beside_it() {
        let mut body = recorded_health("health-v0.9.5-ready.json");
        assert_eq!(
            body["limits"]["max_batch_chunks"],
            serde_json::json!(256),
            "this test needs a readable sibling limit to keep"
        );
        body["limits"]["max_batch_chunks"] = serde_json::json!(16);
        body["limits"]["embed_request_timeout_secs"] = serde_json::Value::Null;

        let limits = probe_recorded(body)
            .await
            .server_limits()
            .expect("a partly unreadable `limits` must still yield the object");
        assert_eq!(
            limits.max_batch_chunks,
            Some(16),
            "the advertised chunk cap was discarded because a sibling member \
             was unreadable"
        );
        assert_eq!(
            limits.embed_request_timeout_secs, None,
            "the unreadable member itself must degrade to not-advertised"
        );
    }

    #[tokio::test]
    async fn a_partly_unreadable_limits_object_still_lowers_the_chunk_ceiling() {
        let mut body = recorded_health("health-v0.9.5-ready.json");
        body["limits"]["max_batch_chunks"] = serde_json::json!(16);
        body["limits"]["embed_request_timeout_secs"] = serde_json::Value::Null;

        let tier = probe_recorded(body).await;
        let advertised = tier
            .server_limits()
            .and_then(|l| l.max_batch_chunks)
            .expect("the advertised chunk cap must survive its sibling");
        assert!(
            advertised < 256,
            "a peer capping batches at 16 must not leave this CLI planning \
             around a larger number ({advertised})"
        );
    }

    #[derive(Clone, Default)]
    struct CapturedLogs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl CapturedLogs {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().expect("captured logs mutex")).into_owned()
        }
    }

    impl std::io::Write for CapturedLogs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("captured logs mutex")
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
        type Writer = CapturedLogs;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    // The subscriber default is thread-local and `#[tokio::test]` runs on one
    // thread, so the guard covers the awaits without affecting parallel tests.
    async fn probe_capturing_warnings(body: serde_json::Value) -> (Tier, String) {
        let logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        let tier = probe_recorded(body).await;
        drop(guard);
        (tier, logs.text())
    }

    async fn probe_raw_capturing_warnings(raw: &str) -> (Tier, String) {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_string(raw))
            .mount(&server)
            .await;
        let tier = probe_url(&server.uri(), REMOTE_PROBE_TIMEOUT, true, None)
            .await
            .expect("probe of a raw peer body must succeed");
        drop(guard);
        (tier, logs.text())
    }

    #[tokio::test]
    async fn every_degraded_field_names_itself_in_a_warning() {
        for field in [
            "capabilities",
            "instance_id",
            "started_by",
            "embedding_dim",
            "embedder",
            "limits",
            "accepts_pushed_vectors",
        ] {
            let mut body = recorded_health("health-v0.9.5-ready.json");
            // A negative integer is unreadable for every member; an unknown object is
            // not (`embedder` tolerates a reshape).
            body[field] = serde_json::json!(-7);
            let (_, logs) = probe_capturing_warnings(body).await;
            assert!(
                logs.contains(&format!("field `{field}`")),
                "degrading `{field}` logged nothing that names it, so the \
                 failure is as silent as it was before: {logs}"
            );
        }

        let mut nested = recorded_health("health-v0.9.5-ready.json");
        nested["embedder"]["state"] = serde_json::Value::Null;
        let (_, logs) = probe_capturing_warnings(nested).await;
        assert!(
            logs.contains("field `embedder.state`"),
            "a degraded nested member must name itself too: {logs}"
        );
    }

    #[tokio::test]
    async fn a_body_that_is_not_a_health_object_warns_and_bounds_the_snippet() {
        let (tier, logs) = probe_raw_capturing_warnings("ok").await;
        assert!(
            matches!(tier.caps(), Some(c) if !c.search_semantic),
            "a plain-text body must still take the legacy reading"
        );
        assert!(
            logs.contains("could not parse the /v1/health body"),
            "the fallback arm must not be silent: {logs}"
        );

        let payload = "A".repeat(100_000);
        let (_, big_logs) = probe_raw_capturing_warnings(&payload).await;
        assert!(
            big_logs.len() < 4_000,
            "the fallback warning grew with the body it was reporting on \
             ({} bytes logged for a 100 kB body)",
            big_logs.len()
        );
    }

    // Valid JSON that is not an object reaches the struct error, which quotes
    // the whole body; the syntax-error arm carries no input.
    #[tokio::test]
    async fn a_valid_json_body_that_is_not_an_object_is_bounded_too() {
        let payload = serde_json::json!("C".repeat(100_000)).to_string();
        let (_, big_logs) = probe_raw_capturing_warnings(&payload).await;
        assert!(
            big_logs.len() < 4_000,
            "the fallback warning grew with a valid-JSON body it could not read \
             ({} bytes logged for a 100 kB body)",
            big_logs.len()
        );
    }

    // serde quotes a wrong-typed value in full, and the `/v1/health` body is
    // peer-controlled.
    #[tokio::test]
    async fn a_degraded_field_does_not_echo_its_own_value_into_the_log() {
        let secret = format!("ghp_{}", "s3cr3t".repeat(4));
        let mut body = recorded_health("health-v0.9.5-ready.json");
        body["started_by"] = serde_json::json!(secret);

        let (_, logs) = probe_capturing_warnings(body).await;
        assert!(
            !logs.contains(&secret),
            "the warning for an unreadable field quoted the field's own value \
             into the log: {logs}"
        );
    }

    #[tokio::test]
    async fn a_degraded_field_warning_is_bounded_by_the_field_it_reports_on() {
        let mut body = recorded_health("health-v0.9.5-ready.json");
        body["started_by"] = serde_json::json!("B".repeat(100_000));

        let (_, logs) = probe_capturing_warnings(body).await;
        assert!(
            logs.len() < 4_000,
            "the per-field warning grew with the value it was reporting on \
             ({} bytes logged for one 100 kB field)",
            logs.len()
        );
    }

    #[test]
    fn the_body_snippet_bounds_a_multibyte_body_without_splitting_a_character() {
        let raw = "\u{1f600}".repeat(10_000);
        let snippet = health_body_snippet(raw.as_bytes());
        assert!(snippet.ends_with("..."), "a long body must be marked cut");
        assert!(
            snippet.chars().count() <= 203,
            "snippet ran to {} chars",
            snippet.chars().count()
        );
    }

    // A body reaches a single arm, so per-arm tests miss regressions in the
    // others; this drives every wire shape through the same assertions.
    const CREDENTIAL_SHAPED: &str = "ghp_A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8";

    fn hostile_health_bodies() -> Vec<(&'static str, String)> {
        // Every credential sits past the snippet's 200-char bound, so a hit means
        // the body was rendered further than the bound allows.
        let filler = "z".repeat(100_000);
        let hostile_object = serde_json::json!({
            "status": "ok",
            "version": filler,
            "capabilities": CREDENTIAL_SHAPED,
            "instance_id": -1,
            "started_by": CREDENTIAL_SHAPED,
            "embedding_dim": CREDENTIAL_SHAPED,
            "embedder": CREDENTIAL_SHAPED,
            "limits": {
                "embed_request_timeout_secs": CREDENTIAL_SHAPED,
                "max_batch_chunks": CREDENTIAL_SHAPED,
                "embedder_token_cap": CREDENTIAL_SHAPED,
            },
            "accepts_pushed_vectors": CREDENTIAL_SHAPED,
        })
        .to_string();

        vec![
            (
                "valid JSON string",
                serde_json::json!(format!("{filler}{CREDENTIAL_SHAPED}")).to_string(),
            ),
            ("valid JSON number", format!("1{}", "0".repeat(100_000))),
            (
                "valid JSON array",
                serde_json::json!([filler, CREDENTIAL_SHAPED]).to_string(),
            ),
            ("valid JSON boolean", "true".to_string()),
            ("valid JSON null", "null".to_string()),
            ("valid JSON object with hostile values", hostile_object),
            (
                "invalid JSON carrying a credential",
                format!("{{\"capabilities\": [\"{filler}{CREDENTIAL_SHAPED}\""),
            ),
            (
                "deeply nested",
                format!("{}{}", "[".repeat(4096), "]".repeat(4096)),
            ),
            (
                "plain text carrying a credential",
                format!("{filler}{CREDENTIAL_SHAPED}"),
            ),
        ]
    }

    #[tokio::test]
    async fn no_health_warning_renders_a_hostile_peer_body_unbounded() {
        for (label, body) in hostile_health_bodies() {
            let (_, logs) = probe_raw_capturing_warnings(&body).await;
            assert!(
                logs.len() < 8_000,
                "the warning for `{label}` grew with the {}-byte body it was \
                 reporting on ({} bytes logged)",
                body.len(),
                logs.len()
            );
            assert!(
                !logs.contains(CREDENTIAL_SHAPED),
                "the warning for `{label}` rendered peer bytes from past the \
                 snippet bound, so a credential in the body reached the log: {logs}"
            );
        }
    }

    // The head of a non-object body is rendered on purpose as the only
    // diagnostic; it must stop at its bound.
    #[tokio::test]
    async fn the_deliberate_body_snippet_stops_at_its_bound() {
        let past_the_bound = format!("{}{CREDENTIAL_SHAPED}", "h".repeat(300));
        let (_, logs) = probe_raw_capturing_warnings(&past_the_bound).await;
        assert!(
            !logs.contains(CREDENTIAL_SHAPED),
            "content 300 characters into the body was rendered, so the snippet \
             bound is not the limit of what a peer can put in the log: {logs}"
        );
        assert!(
            logs.contains("hhh"),
            "the snippet rendered none of the body, which leaves the operator \
             with no sample of what the peer actually sent: {logs}"
        );
    }

    // No mock is mounted: reaching the network would be the failure.
    #[tokio::test]
    async fn probe_url_rejects_spoofed_loopback_authorities() {
        for url in [
            "http://127.0.0.1.evil.example",
            "http://127.0.0.1@evil.example",
            "http://127.0.0.1:1234@evil.example",
        ] {
            let err = probe_url(url, std::time::Duration::from_millis(1), false, None)
                .await
                .expect_err("a host that only looks like loopback must be rejected");
            assert!(err.contains("loopback"), "{url}: {err}");
        }
    }

    // The OS process query is passed as a bool, which the live-process tests
    // below cannot cover on Windows.

    #[test]
    fn classify_responder_refuses_when_no_pid_recorded() {
        assert_eq!(
            classify_responder(None, false, Some("id"), Some("id")),
            Some(Untrusted::NoRecordedPid)
        );
    }

    #[test]
    fn classify_responder_refuses_a_pid_that_is_not_the_server() {
        assert_eq!(
            classify_responder(Some(4711), false, Some("id"), Some("id")),
            Some(Untrusted::PidIsNotTheServer(4711))
        );
    }

    #[test]
    fn classify_responder_refuses_when_no_instance_id_recorded() {
        assert_eq!(
            classify_responder(Some(4711), true, None, Some("id")),
            Some(Untrusted::NoRecordedInstanceId)
        );
    }

    #[test]
    fn classify_responder_refuses_a_mismatched_instance_id() {
        assert_eq!(
            classify_responder(Some(4711), true, Some("recorded"), Some("other")),
            Some(Untrusted::InstanceIdMismatch)
        );
    }

    #[test]
    fn classify_responder_refuses_a_responder_that_reports_no_instance_id() {
        assert_eq!(
            classify_responder(Some(4711), true, Some("recorded"), None),
            Some(Untrusted::InstanceIdMismatch)
        );
    }

    // The branch that must stay trusted, so the check cannot be satisfied by
    // refusing everything.
    #[test]
    fn classify_responder_trusts_a_fully_matching_daemon() {
        assert_eq!(
            classify_responder(Some(4711), true, Some("id"), Some("id")),
            None
        );
    }

    // Bypasses only the un-fakeable OS process query, and only for `1`/`true`;
    // any other value runs the real query (a ghost pid, so a definite `false`).
    // Also in `server_state_dir_env`: the outbox and status relay tests set the
    // same variable.
    #[test]
    #[serial_test::serial(inkentry_no_server_env, server_state_dir_env)]
    fn recorded_pid_is_server_seam_is_fail_safe() {
        let ghost = u32::MAX;
        let case = |value: Option<&str>| -> bool {
            // SAFETY: the `inkentry_no_server_env` serial group makes this the
            // only test touching this variable at a time.
            unsafe {
                match value {
                    Some(v) => std::env::set_var("INKENTRY_TEST_TRUST_RECORDED_RESPONDER", v),
                    None => std::env::remove_var("INKENTRY_TEST_TRUST_RECORDED_RESPONDER"),
                }
            }
            recorded_pid_is_server(ghost)
        };

        let enables = ["1", "true", "TRUE", " true "].map(|v| case(Some(v)));
        let real_query = [
            case(Some("0")),
            case(Some("yes")),
            case(Some("2")),
            case(Some("")),
            case(None),
        ];

        // Unset before asserting, so a failing assert cannot leak `trust = true`
        // into the discovery tests sharing this serial group.
        unsafe { std::env::remove_var("INKENTRY_TEST_TRUST_RECORDED_RESPONDER") };

        assert!(
            enables.iter().all(|&t| t),
            "1/true (any case, trimmed) must force trust"
        );
        assert!(
            real_query.iter().all(|&t| !t),
            "any other value must fall back to the real query, which is false for a ghost pid"
        );
    }

    // Unix only: the positive pid case needs a live process whose command line
    // reads `inkentry-server`, found via `ps`. Windows matches an image name that
    // a test cannot fabricate; cross-platform coverage is the pure policy tests
    // above and `security_tests/loopback_discovery_trust.rs`.

    #[cfg(unix)]
    const RECORDED_INSTANCE_ID: &str = "00000000-0000-0000-0000-000000000001";

    // The symlink puts the name in `ps -o args=`; the shell loop keeps the shell
    // alive, since a shell running a single command execs it and loses the name.
    #[cfg(unix)]
    struct Placeholder {
        child: std::process::Child,
        _dir: tempfile::TempDir,
    }

    #[cfg(unix)]
    impl Placeholder {
        fn spawn(named_like_the_server: bool) -> Self {
            let dir = tempfile::TempDir::new().expect("temp dir for the placeholder process");
            let exe = dir.path().join(if named_like_the_server {
                "inkentry-server"
            } else {
                "unrelated-process"
            });
            std::os::unix::fs::symlink("/bin/sh", &exe).expect("symlink /bin/sh");
            let child = std::process::Command::new(&exe)
                .arg("-c")
                .arg("while :; do sleep 1; done")
                // The shell's `sleep` children would inherit the harness's output
                // handles, which nextest reports as a leaky test.
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn the placeholder process");
            Self { child, _dir: dir }
        }

        fn pid(&self) -> u32 {
            self.child.id()
        }
    }

    #[cfg(unix)]
    impl Drop for Placeholder {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[cfg(unix)]
    struct EnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);

    #[cfg(unix)]
    impl EnvGuard {
        fn set(pairs: &[(&'static str, std::ffi::OsString)]) -> Self {
            let saved = pairs
                .iter()
                .map(|(k, v)| {
                    let prev = std::env::var_os(k);
                    // SAFETY: every test using this guard is in both the
                    // `inkentry_no_server_env` and `server_state_dir_env`
                    // serial groups, so no other test reads or writes these
                    // variables concurrently. The second group matters: the
                    // outbox and status relay tests set the same trust seam.
                    unsafe { std::env::set_var(k, v) };
                    (*k, prev)
                })
                .collect();
            Self(saved)
        }
    }

    #[cfg(unix)]
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, prev) in &self.0 {
                unsafe {
                    match prev {
                        Some(v) => std::env::set_var(key, v),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    #[cfg(unix)]
    fn write_discovery_state(dir: &std::path::Path, port: u16, pid: u32, instance_id: &str) {
        std::fs::create_dir_all(dir).expect("create the state dir");
        std::fs::write(dir.join("server.port"), format!("{port}\n")).expect("write server.port");
        std::fs::write(dir.join("server.pid"), format!("{pid}\n")).expect("write server.pid");
        std::fs::write(dir.join("server.instance_id"), format!("{instance_id}\n"))
            .expect("write server.instance_id");
    }

    #[cfg(unix)]
    async fn mock_daemon(instance_id: &str) -> (wiremock::MockServer, u16) {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let mut body = health_body(&["memory"], 0);
        body["instance_id"] = serde_json::json!(instance_id);
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let port = server.address().port();
        (server, port)
    }

    // Guards against verification that passes by refusing everything.
    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial(inkentry_no_server_env, server_state_dir_env)]
    async fn a_recorded_daemon_that_still_matches_is_discovered() {
        let (_server, port) = mock_daemon(RECORDED_INSTANCE_ID).await;
        let daemon = Placeholder::spawn(true);
        let tmp = tempfile::TempDir::new().unwrap();
        write_discovery_state(tmp.path(), port, daemon.pid(), RECORDED_INSTANCE_ID);

        let _env = EnvGuard::set(&[
            ("INKENTRY_STATE_DIR", tmp.path().into()),
            // Disable the default-port fallback so only the state file is exercised.
            ("INKENTRY_TEST_DISCOVERY_PORT", "0".into()),
        ]);
        unsafe { std::env::remove_var("INKENTRY_NO_SERVER") };

        let tier = probe_loopback().await;
        assert_eq!(
            tier.server_url(),
            Some(format!("http://127.0.0.1:{port}")).as_deref(),
            "a daemon whose recorded PID and instance_id both still match must \
             stay discoverable; got {tier:?}"
        );
    }

    // The fallback port points at the squatter too, so falling through to it
    // would re-discover it.
    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial(inkentry_no_server_env, server_state_dir_env)]
    async fn a_squatter_on_the_recorded_port_is_not_the_embedding_backend() {
        let (_server, port) = mock_daemon(RECORDED_INSTANCE_ID).await;
        let squatter = Placeholder::spawn(false);
        let tmp = tempfile::TempDir::new().unwrap();
        // The instance_id matches, so the pid check alone can refuse this responder.
        write_discovery_state(tmp.path(), port, squatter.pid(), RECORDED_INSTANCE_ID);

        let _env = EnvGuard::set(&[
            ("INKENTRY_STATE_DIR", tmp.path().into()),
            ("INKENTRY_TEST_DISCOVERY_PORT", port.to_string().into()),
        ]);
        unsafe { std::env::remove_var("INKENTRY_NO_SERVER") };

        let tier = probe_loopback().await;
        assert!(
            !tier.is_server(),
            "the recorded PID is a live process that is not an inkentry-server, \
             so the responder on that port must not become the embedding \
             backend; got {tier:?}"
        );
    }

    // The pid check passes, so only the recorded instance_id separates the two.
    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial(inkentry_no_server_env, server_state_dir_env)]
    async fn a_responder_with_a_different_instance_id_is_not_the_recorded_daemon() {
        let (_server, port) = mock_daemon("00000000-0000-0000-0000-00000000beef").await;
        let daemon = Placeholder::spawn(true);
        let tmp = tempfile::TempDir::new().unwrap();
        write_discovery_state(tmp.path(), port, daemon.pid(), RECORDED_INSTANCE_ID);

        let _env = EnvGuard::set(&[
            ("INKENTRY_STATE_DIR", tmp.path().into()),
            ("INKENTRY_TEST_DISCOVERY_PORT", port.to_string().into()),
        ]);
        unsafe { std::env::remove_var("INKENTRY_NO_SERVER") };

        let tier = probe_loopback().await;
        assert!(
            !tier.is_server(),
            "the responder reports an instance_id other than the one recorded at \
             start, so it is not the recorded daemon; got {tier:?}"
        );
    }
}
