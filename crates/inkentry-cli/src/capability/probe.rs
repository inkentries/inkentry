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

/// The single state file directory resolver for the whole CLI:
/// `~/.local/state/inkentry/`, or `INKENTRY_STATE_DIR` when set.
///
/// Every reader and writer of runtime state goes through this one function:
/// `inkentry server start/stop/status/logs` (server pid/port/log/db-path
/// files, `cli/cmd/server.rs`), the embed worker's liveness files
/// (`cli/cmd/embed_worker.rs`), and this module's own loopback
/// auto-discovery probe below. A second, independent resolution here was a
/// real bug: it let the override apply to some readers/writers and not
/// others, so a status reader could miss a worker's pid file written to a
/// different directory (or vice versa) and misreport liveness.
///
/// On all platforms we use `~/.local/state` rather than the OS-native state dir.
/// This mirrors the deliberate choice made for the config dir
/// (`inkentry_config_dir` in inkentry-core's `config.rs`, which uses `~/.config`
/// on every platform): it keeps the path identical across Linux and macOS, and
/// matches what the CLI documentation and error messages say.
///
/// It also sidesteps a concrete portability bug: `dirs::state_dir()` returns
/// `None` on macOS (dirs v6 has no XDG_STATE_HOME equivalent there), which
/// silently disabled loopback auto-discovery on the primary dev platform
/// (spelunk-cloud/spelunk#316).
///
/// `INKENTRY_STATE_DIR` is a supported override of the entire path, not
/// dev-only cruft: it is load-bearing on Windows CI, where `dirs::home_dir()`
/// 6.x calls `SHGetKnownFolderPath` (a Windows Registry lookup) rather than
/// reading `USERPROFILE`, making per-process environment overrides of `HOME`
/// ineffective. It is also used directly by end users who want state files
/// somewhere other than the default (e.g. an ephemeral or sandboxed HOME).
///
/// Errors only when the home directory can't be resolved and no override is
/// set.
pub(crate) fn inkentry_state_dir() -> anyhow::Result<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("INKENTRY_STATE_DIR") {
        return Ok(std::path::PathBuf::from(p));
    }
    dirs::home_dir()
        .map(|home| home.join(".local").join("state").join("inkentry"))
        .ok_or_else(|| anyhow::anyhow!("could not determine home directory"))
}

/// Read the port written by `inkentry server start` into
/// `~/.local/state/inkentry/server.port`. Returns `None` if absent or unreadable.
fn read_server_port_file() -> Option<u16> {
    let path = inkentry_state_dir().ok()?.join("server.port");
    let content = std::fs::read_to_string(&path).ok()?;
    content.trim().parse::<u16>().ok()
}

/// Why a loopback responder is not the daemon `inkentry server start` recorded.
///
/// Both checks read state this CLI wrote itself. The health body cannot supply
/// either one: a process that holds the port answers `/v1/health` with whatever
/// it likes, `instance_id` and `started_by` included, so a self-reported value
/// only ever confirms itself. What the recorded PID and the recorded id add is
/// a second, independent source.
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

/// The state directory to name when telling the user which recording is in
/// force. Falls back to the documented default path, which is what the reader
/// would go looking for anyway, rather than saying nothing.
fn state_dir_for_message() -> String {
    inkentry_state_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "~/.local/state/inkentry".to_string())
}

/// Check a loopback responder against what `inkentry server start` recorded.
///
/// `reported` is the `instance_id` from the health body just received.
/// Both files are read through `cli/cmd/server.rs`, which writes them: the
/// state directory's layout has one owner, and a second spelling of these
/// names here is how a reader and a writer drift apart.
///
/// Gathers the recorded facts, then hands the decision to [`classify_responder`],
/// which is where the policy lives so it can be tested on a host that cannot
/// stand up a process named `inkentry-server`. Also reached by
/// `server::probe_local_relay_port` through the `capability` re-export, so the
/// relay-reuse gate and step 3a apply the identical check.
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

/// The recorded pid still names an `inkentry-server`, per the OS process query.
///
/// `INKENTRY_TEST_TRUST_RECORDED_RESPONDER=1` (or `=true`) answers "yes" without
/// running that query. It exists because the discovery-trust integration tests
/// stand up an in-process server, which has no separate `inkentry-server`
/// process to point a recorded pid at, and because the query's positive case
/// cannot be staged on Windows at all, where the match is against an image name
/// a test cannot forge. It relaxes only this one un-fakeable signal: a pid must
/// still be recorded, and the recorded `instance_id` must still match, so a test
/// using it still exercises every check it can. It is read only on the
/// discovery-trust path and never by `classify_running_server`, so no value of
/// it can widen the set of processes a lifecycle command will signal (ADR-085).
/// A no-op for every real user: unset outside the test harness, and any value
/// other than the two above runs the real query.
fn recorded_pid_is_server(pid: u32) -> bool {
    if let Ok(raw) = std::env::var("INKENTRY_TEST_TRUST_RECORDED_RESPONDER") {
        let v = raw.trim();
        if v == "1" || v.eq_ignore_ascii_case("true") {
            return true;
        }
    }
    crate::cli::cmd::server::process_matches_server(pid)
}

/// Classify a responder against the facts `inkentry server start` recorded, with
/// the OS process query supplied as `pid_matches_server` rather than run here.
///
/// Splitting the policy from the two disk reads and the `ps`/`tasklist` call is
/// what lets the decision be unit-tested on every platform, including the happy
/// path (`None`) that every command travels when a daemon is running: a test can
/// pass `pid_matches_server` directly instead of fabricating a process the OS
/// query would accept, which is impossible to stage on Windows.
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

/// The reason an *explicit* offline mode is in force, or `None` when the tier
/// must be established by probing.
///
/// An explicit offline mode (config `mode = "offline"`, `INKENTRY_MODE=offline`,
/// or the `INKENTRY_NO_SERVER=1` kill-switch) skips all server probes: the user
/// has asked for a provable no-cloud run.
///
/// The *defaulted* offline (no `server_url` and no explicit `mode`) must NOT
/// skip probing: loopback auto-discovery is inference-only (it never owns
/// memory, ADR-004) and is what gives a local-only project semantic search.
/// Conflating the two would silently disable the loopback embedder.
///
/// The kill-switch is checked first because it wins: while it is set, `mode`
/// and `server_url` are inert, and advice that names either of them is advice
/// the reader cannot act on.
fn explicit_offline_reason(cfg: &Config) -> Option<OfflineReason> {
    if inkentry_core::config::no_server_env_set() {
        return Some(OfflineReason::KillSwitch);
    }
    if cfg.mode != Some(inkentry_core::config::SyncMode::Offline) {
        return None;
    }
    // `INKENTRY_MODE` overwrites `cfg.mode` during load, so by here the two
    // sources are indistinguishable on the value alone. Ask the environment
    // which one is in force: while the variable is set, editing the config
    // line changes nothing.
    Some(match std::env::var("INKENTRY_MODE") {
        Ok(_) => OfflineReason::ModeOfflineEnv,
        Err(_) => OfflineReason::ModeOfflineConfig,
    })
}

/// Return the cached capability tier for this process.
///
/// On the first call, probes the server according to the following priority:
///
/// 1. If `INKENTRY_NO_SERVER=1` is set → `Tier::Offline` immediately.
/// 2. If `cfg.server_url` is set → probe that URL with a **2 s** timeout
///    (`auto_discovered = false`).
/// 3. If `cfg.server_url` is `None` → loopback auto-discovery:
///    a. Read `~/.local/state/inkentry/server.port`; probe `127.0.0.1:<port>`,
///    and use it only if the recorded pid and instance id still match.
///    b. Only when nothing was recorded: probe `127.0.0.1:<DEFAULT_SERVER_PORT>`.
///    Both loopback probes use a **250 ms** timeout.
///    On success: `auto_discovered = true`. On failure: `Tier::Offline`.
///
/// Subsequent calls return immediately from the per-process `OnceCell` cache.
///
/// **Per-process cache**: the result is stored in a `static OnceCell` and is fixed
/// for the lifetime of the process. This is correct for CLI invocations (one process
/// = one config), but unsuitable for long-running daemons that may use multiple
/// configs: they would always see the tier determined by the first call.
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

/// One fresh, uncached tier probe, honouring the same explicit-offline
/// short-circuits as [`get_tier`].
///
/// For pollers only: `get_tier`'s process-lifetime cache pins whatever state
/// the first probe saw, so a caller that must observe a *transition* (the
/// detached embed worker waiting for the embedder to finish loading) has to
/// re-probe. Everything else should keep using [`get_tier`].
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

/// Remote-server probe timeout (explicit `server_url` in config/env).
const REMOTE_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Loopback probe timeout (auto-discovery of a locally-running server).
const LOOPBACK_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

async fn probe(url: Option<&str>, server_ca: Option<&std::path::Path>) -> Tier {
    // ── 1. INKENTRY_NO_SERVER short-circuit ───────────────────────────────────
    if inkentry_core::config::no_server_env_set() {
        tracing::debug!("INKENTRY_NO_SERVER set: skipping all server probes");
        return Tier::Offline(OfflineReason::KillSwitch);
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
    probe_loopback().await
}

/// The port step 3b falls back to, or `None` when the fallback is disabled.
///
/// `INKENTRY_TEST_DISCOVERY_PORT` overrides [`DEFAULT_SERVER_PORT`]; `0`, or
/// any value that is not a port number, disables step 3b outright. A no-op for
/// every real user: the var is never set outside the test harness.
///
/// The integration suite needs this because step 3b is the one step it cannot
/// isolate. Pointing `INKENTRY_STATE_DIR` at an empty dir only defeats step
/// 3a — 3b then probes a fixed port on the developer's own machine, so a test
/// run sends real embedding work to whatever daemon is listening there
/// (inkentry-oss^5). Disabling the fallback is the isolation; giving it a
/// live mock to find is the alternative, and costs a server per test.
fn discovery_fallback_port() -> Option<u16> {
    let Ok(raw) = std::env::var("INKENTRY_TEST_DISCOVERY_PORT") else {
        return Some(DEFAULT_SERVER_PORT);
    };
    match raw.trim().parse::<u16>() {
        Ok(0) => None,
        Ok(port) => Some(port),
        // Fail closed. Restoring the fallback here would let `=o` instead of
        // `=0` silently reach the developer's daemon again — the exact failure
        // this variable exists to prevent, reintroduced by the mechanism meant
        // to prevent it. Offline is loud and obviously wrong in a test; the
        // variable is test-only, so no user is affected either way.
        Err(_) => {
            tracing::warn!(
                "INKENTRY_TEST_DISCOVERY_PORT={raw:?} is not a port number; \
                 disabling loopback discovery's fallback rather than restoring it"
            );
            None
        }
    }
}

/// Loopback auto-discovery only: never consults `cfg.server_url`.
///
/// Step 3a: the port file written by `inkentry server start`, whose responder is
/// used only when the recorded pid and instance id still identify it as that
/// daemon. Step 3b: [`DEFAULT_SERVER_PORT`], reached only when nothing was
/// recorded at all. Both steps use the 250 ms loopback timeout and treat any
/// probe failure as `Tier::Offline` (never a hard error: loopback
/// auto-discovery finding nothing is the normal "no local server" case, not a
/// misconfiguration).
///
/// Once a port is recorded, that recording decides the answer: a responder on
/// it that fails the identity checks does not fall through to step 3b. Falling
/// through was how the check could be walked around, since a process holding
/// the recorded port is usually holding the default one too, and 3b asks it
/// nothing.
///
/// Split out of [`probe`] so [`get_inference_tier`] can run the identical
/// discovery independent of an explicit `server_url`: `local_first` always
/// prefers the local embedder, even when `server_url` targets a remote.
async fn probe_loopback() -> Tier {
    // Step 3a: port file written by `inkentry server start`
    if let Some(port) = read_server_port_file() {
        let loopback_url = format!("http://127.0.0.1:{port}");
        tracing::debug!(
            "loopback auto-discovery: found server.port={port}, probing {loopback_url}"
        );
        // Loopback probes never produce hard errors (auto_discovered=true), so unwrap is safe.
        // Loopback is plaintext http: a custom CA is irrelevant here.
        let (tier, reported_instance_id) =
            probe_url_reporting_instance_id(&loopback_url, LOOPBACK_PROBE_TIMEOUT, true, None)
                .await
                .unwrap_or((Tier::Offline(OfflineReason::NoLocalServer), None));

        // Both refusals below are announced, and for the same reason: the user
        // started a local server, this run is not using it, and a `tracing`
        // line the default log level drops tells them neither fact. They share
        // one remedy, so the arms carry what happened and the tail carries what
        // to do.
        let refused = match tier {
            // A dimension mismatch keeps its own reason and its own advice:
            // that daemon answered and said what is wrong with it.
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

        eprintln!(
            "warning: {refused}. Embeddings are offline for this run: run \
             `inkentry server stop`, then `inkentry server start`."
        );
        return Tier::Offline(OfflineReason::RecordedServerUnreachable);
    }

    // Step 3b: the default port, for a machine that never started a server.
    // There is nothing recorded to check a responder against here, which is
    // why the step above refuses to hand this one its leftovers.
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

/// Resolve the tier used specifically to route **inference** (embeddings +
/// LLM), which can differ from [`get_tier`]'s general-purpose capability tier.
///
/// Per the founder's 2026-07-23 routing decision (ADR-004
/// revision): `local_first` (and the serde-default mode reached when no
/// `server_url` is set) always routes inference to the local loopback
/// embedder, even when `server_url` is explicitly configured — there, an
/// explicit `server_url` is a memory sync replica only, never an inference
/// target. Only `cloud_first` lets an explicit `server_url` serve inference
/// too, in which case this reuses [`get_tier`]'s cached probe of that URL
/// (unchanged behaviour for that mode).
///
/// Explicit offline (`INKENTRY_NO_SERVER` / `mode = "offline"`) skips every
/// probe, mirroring `get_tier`.
///
/// Not cached via a `OnceCell` like `get_tier`: `local_first` always runs a
/// fresh loopback probe rather than reusing whatever `get_tier` already
/// cached for `cfg.server_url` (a different, unrelated target in that mode).
pub async fn get_inference_tier(cfg: &Config) -> Tier {
    inference_tier(cfg, CloudBranchProbe::Cached).await
}

/// Fresh-probing counterpart to [`get_inference_tier`], for callers that must
/// observe a *transition* rather than a point-in-time snapshot: the detached
/// embed worker's readiness wait (`wait_for_embedder`) polls repeatedly for
/// the embedder to flip from `loading` to `ready`, so it can never read
/// through a value pinned by [`get_tier`]'s per-process `OnceCell`.
///
/// Routes identically to [`get_inference_tier`] (same mode-based branch,
/// same explicit-offline short-circuit), except the `cloud_first` branch
/// re-probes the configured `server_url` on every call via
/// [`probe_tier_fresh`] instead of reading [`get_tier`]'s cache: the same
/// relationship `probe_tier_fresh` already has to `get_tier`, applied one
/// level up. The `local_first` branch needs no change here: it already calls
/// `probe_loopback()` directly, which was never cached.
pub async fn get_inference_tier_fresh(cfg: &Config) -> Tier {
    inference_tier(cfg, CloudBranchProbe::Fresh).await
}

/// Which probe the `cloud_first` branch of [`inference_tier`] takes: the
/// per-process cache ([`get_tier`], for one-shot callers) or a fresh probe
/// ([`probe_tier_fresh`], for pollers that must observe a transition).
enum CloudBranchProbe {
    Cached,
    Fresh,
}

/// Shared mode-based routing behind [`get_inference_tier`] and
/// [`get_inference_tier_fresh`]; see their docs for the routing rules. The two
/// differ only in which probe serves the `cloud_first` branch.
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
    probe_url_reporting_instance_id(url, timeout, auto_discovered, server_ca)
        .await
        .map(|(tier, _)| tier)
}

/// [`probe_url`], also returning the `instance_id` the health body reported.
///
/// Only loopback auto-discovery reads that id, and only to compare it against
/// the one recorded at start: it is a claim by the peer, never an identity on
/// its own.
async fn probe_url_reporting_instance_id(
    url: &str,
    timeout: std::time::Duration,
    auto_discovered: bool,
    server_ca: Option<&std::path::Path>,
) -> Result<(Tier, Option<String>), String> {
    // Non-loopback plaintext http:// is invalid config: reject before sending
    // anything. No opt-out: the fix is always "use https, or loopback".
    inkentry_core::config::validate_transport_url(url)?;

    // Which server this probe was aimed at is what the offline advice turns
    // on, and `auto_discovered` is the only thing that answers it here.
    let unreached = if auto_discovered {
        OfflineReason::NoLocalServer
    } else {
        OfflineReason::ExplicitServerUnavailable
    };

    // Already known absent in this process: the probe would spend a connect
    // timeout to return exactly this. Skipping is a latency shortcut only, and
    // returns the same tier a failed probe returns.
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
    // Connect gets half the liveness budget, leaving the rest to read the
    // answer: a probe that spent its whole budget connecting would have nothing
    // left to hear a reply with. Splitting them also makes the failure legible.
    // With one budget for both, a host that never answers and a server that
    // answers slowly both surface as the same undifferentiated timeout, and the
    // difference decides whether the miss may be memoised as unreachable.
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

    // `/v1/health` is an unauthenticated endpoint: do not send a bearer to it.
    let req = client.get(format!("{}/v1/health", url.trim_end_matches('/')));

    match req.send().await {
        Ok(resp) if resp.status().is_success() => {
            let health = parse_health(url, resp).await;

            // If the server advertises index.embed, its embedding dimension must match ours.
            let server_dim = health.embedding_dim;
            if health.caps.index_embed && server_dim != 0 {
                let expected = inkentry_core::embeddings::EMBEDDING_DIM;
                if server_dim != expected {
                    if auto_discovered {
                        // Loopback auto-discovery: downgrade gracefully: the user did
                        // not explicitly configure this server.
                        tracing::warn!(
                            "inkentry-server at {url} serves {server_dim}-dim embeddings; \
                             this CLI expects {expected}-dim. Ignoring loopback server. \
                             Restart the server (`inkentry server start`) or set \
                             INKENTRY_NO_SERVER=1 to suppress this probe."
                        );
                        return Ok((Tier::Offline(OfflineReason::LocalServerUnusable), None));
                    } else {
                        // Explicit server_url: surface as a hard error so the user
                        // gets actionable guidance before any command runs.
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
            // This probe is usually the first thing in a command to touch the
            // server, so recording a genuine miss here is what lets everything
            // after it skip straight to the same answer instead of each
            // spending its own connect timeout. TLS failures are excluded on
            // purpose: that server answered, and a later request has to run its
            // own handshake to report the certificate cause.
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

/// Read one `/v1/health` field, degrading a value this CLI cannot read to that
/// field's default instead of failing the whole body.
///
/// Without this, a strict field nested inside a tolerated structure costs the
/// entire response: `parse_health` maps any deserialization error onto the
/// legacy plain-text branch, so `capabilities`, `embedding_dim` and `limits`
/// are all discarded together because of one field. That amplification is the
/// defect, not any single field's strictness, and the additive-only rule in
/// docs/stability.md means a newer peer is allowed to send shapes this build
/// has never seen.
///
/// The warning names the field, the shape this build expected and what the
/// degrade costs, and deliberately renders **none** of the value. `/v1/health`
/// is unauthenticated and its body is whatever `server_url` resolves to, so a
/// field's contents are peer-controlled and unbounded: serde's own error text
/// quotes a wrong-typed string in full, which turned a 100 kB field into a
/// 100 kB log line and echoed credential-shaped values verbatim. The received
/// JSON kind carries the diagnostic value without carrying the value itself.
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

/// Name of a JSON value's kind, from a fixed vocabulary, for a log line that
/// must not carry the value.
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

/// Wire shape of `/v1/health`'s `limits`, read one member at a time so an
/// unreadable member costs only itself.
///
/// Reading the object all-or-nothing was the more permissive choice, not the
/// conservative one: a peer advertising `max_batch_chunks: 16` alongside one
/// member this build cannot read lost the 16, and `resolve_batch_ceiling` then
/// planned around this CLI's own maximum of 256, which is the `413` the
/// tolerance exists to avoid. Each member is `None` when absent or unreadable,
/// which the embed phase already reads as "not advertised".
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
    /// Embedding dimension produced by this server's embedder.
    /// Absent on old servers that pre-date this field; defaults to 0 (skip check).
    #[serde(default, deserialize_with = "lenient_embedding_dim")]
    embedding_dim: usize,
    /// Embedder readiness sub-object. Absent on older servers
    /// → `embedder_state` stays `Unknown`.
    #[serde(default, deserialize_with = "lenient_embedder")]
    embedder: Option<EmbedderBody>,
    /// Server-enforced `/index/embed` limits.
    /// Absent, or not an object at all, on older servers → `server_limits`
    /// stays `None`. Present but partly unreadable keeps the members that did
    /// parse (see `ServerLimitsBody`).
    #[serde(default, deserialize_with = "lenient_limits")]
    limits: Option<ServerLimitsBody>,
    /// Whether the server accepts a client-pushed embedding vector on
    /// `POST /memory/batch`. Top-level bool, not a
    /// `capabilities` entry. Absent on servers without the accept side
    /// (an older server, or one whose embedder is not ready) → defaults false.
    #[serde(default, deserialize_with = "lenient_accepts_pushed_vectors")]
    accepts_pushed_vectors: bool,
}

/// What one `/v1/health` answer told us.
struct HealthFacts {
    caps: Capabilities,
    /// `0` when the field is absent (a server pre-dating it) or no embedder is
    /// loaded, which skips the dimension check in `probe_url`.
    embedding_dim: usize,
    embedder_state: EmbedderState,
    server_limits: Option<ServerLimits>,
    /// As reported by the peer, so it identifies nothing on its own; loopback
    /// discovery compares it against the id recorded at start.
    instance_id: Option<String>,
}

/// Conservative reading assumed for a server whose `/v1/health` body is not a
/// readable JSON object at all: the legacy plain-text `ok` responder.
fn legacy_plain_text_health() -> HealthFacts {
    HealthFacts {
        caps: Capabilities::legacy_memory_only(),
        embedding_dim: 0,
        embedder_state: EmbedderState::Unknown,
        server_limits: None,
        instance_id: None,
    }
}

/// Bounded rendering of peer-controlled text for a log line. Cuts on a
/// character boundary so a multibyte body cannot be split mid-character.
fn bounded_for_log(text: &str) -> String {
    let head: String = text.chars().take(200).collect();
    if head.len() < text.len() {
        format!("{head}...")
    } else {
        head
    }
}

/// Bounded, lossy rendering of a health body for a log line.
fn health_body_snippet(raw: &[u8]) -> String {
    bounded_for_log(&String::from_utf8_lossy(raw))
}

/// Parse the health response body into the facts the probe acts on.
///
/// `embedding_dim` is `0` when the field is absent (old server without the field)
/// or when no embedder is loaded. A `0` dim skips the dimension check in `probe_url`
/// for backward compatibility.
///
/// `embedder_state` mirrors the `/v1/health` `embedder.state` field
/// (`embedder: { state, detail }`). It is `Unknown` when the sub-object is
/// absent (older server) or the body is legacy plain-text.
///
/// `server_limits` mirrors `/v1/health`'s `limits` object. `None` when absent :
/// a server that pre-dates the field, which is exactly the version-skew case:
/// it still enforces the old blanket 30s `/index/embed` budget with no
/// exemption, regardless of what the CLI's own calibration would otherwise
/// target.
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

    // Parsed in two steps so the failure can be described without rendering
    // serde's error: `invalid type: string "...", expected struct HealthBody`
    // quotes the whole body back, so a 100 kB JSON string would have produced a
    // 100 kB log line through the one arm whose snippet was already bounded.
    // A `serde_json` syntax error carries only a code and a line/column, never
    // input, so that one is safe to render (bounded anyway).
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
            // A server started by another account on this host is the one
            // thing the health body says that the user has to act on, and
            // `tracing::warn!` is off at the default log level, so for years it
            // said it to nobody.
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
                    eprintln!("warning: {warning}");
                    tracing::warn!("{warning}");
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
            HealthFacts {
                caps,
                embedding_dim: body.embedding_dim,
                embedder_state,
                server_limits: body.limits.map(ServerLimits::from),
                instance_id: body.instance_id,
            }
        }
        Err(_) => {
            // Silence here is what let a single strict field discard whole
            // health bodies unnoticed: every field above degrades on its own
            // now, so reaching this arm means the body was valid JSON that is
            // not a health object at all, and that is worth saying out loud.
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

#[cfg(test)]
mod tests {
    use super::super::diagnostics::{
        explicit_probe_failure, reset_explicit_probe_failure_for_test,
    };
    use super::*;

    // ── read_server_port_file ────────────────────────────────────────────────

    #[test]
    fn read_server_port_file_returns_none_when_absent() {
        // In a temp dir with no server.port file, should return None.
        // We can't control the state dir in a unit test, but we can verify
        // the function doesn't panic and returns a valid Option<u16>.
        // The actual file-read path is exercised by integration tests.
        let _ = read_server_port_file(); // must not panic
    }

    // ── INKENTRY_NO_SERVER and loopback constants ──────────────────────────────

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

    // ── discovery_fallback_port ──────────────────────────────────────────────
    //
    // Mutates the process-global override, so serialised against itself.

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
        // What the integration suite sets: step 3b must not reach the
        // developer's own daemon on the default port.
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
        // `=o` for `=0` must not quietly restore the fallback and let the run
        // reach the developer's daemon. The variable is test-only, so the
        // safe direction is off.
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

    // ── INKENTRY_NO_SERVER short-circuit behaviour ─────────────────────────────
    //
    // These tests mutate the process-global `INKENTRY_NO_SERVER` env var, so they
    // are serialised against each other to avoid cross-test interference.

    #[tokio::test]
    #[serial_test::serial(inkentry_no_server_env)]
    async fn inkentry_no_server_forces_offline() {
        // SAFETY: serialised via #[serial] so no other test reads/writes this
        // env var concurrently; restored before the guard scope ends.
        for val in ["1", "true", "yes"] {
            unsafe { std::env::set_var("INKENTRY_NO_SERVER", val) };
            // server_url = None so that, absent the short-circuit, the probe would
            // attempt loopback auto-discovery; the short-circuit must win.
            let tier = probe(None, None).await;
            assert!(
                matches!(tier, Tier::Offline(OfflineReason::KillSwitch)),
                "INKENTRY_NO_SERVER={val} should force the kill-switch reason, got {tier:?}"
            );
        }
        unsafe { std::env::remove_var("INKENTRY_NO_SERVER") };
    }

    // ── Embedding-dim pre-flight checks ──────────────────────────────────────

    // Helper: build a health JSON body with the given capabilities and dim.
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

    // Auto-discovered loopback server with wrong dim → `Tier::Offline` (soft downgrade).
    #[tokio::test]
    async fn probe_loopback_dim_mismatch_downgrades_to_offline() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // Return a health body claiming 768-dim embeddings: wrong for the current CLI (896).
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

    // Auto-discovered loopback server with correct dim → `Tier::Server`.
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

    // ── accepts_pushed_vectors (top-level health bool) ──────────────────────────

    // A server advertising `accepts_pushed_vectors: true` must parse into
    // `caps.accepts_pushed_vectors == true`: the gate the sync push reads
    // before attaching a client-computed vector.
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

    // A server that omits the field (an older server) must default to
    // `false`: the push stays text-only there.
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

    // Neither end mocked: the real server's health handler, this CLI's real
    // probe, and the real push client. A mock is written from one side's own
    // expectations, so it agrees with that side by construction and cannot
    // catch the two disagreeing. That is precisely how this fast path came to
    // be unreachable while every test on both sides stayed green.
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
        .maybe_attach_vector(caps.accepts_pushed_vectors, Some(vec![0.25_f32; dim]));
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

    // ── ServerLimits parsing (/v1/health `limits` object) ──────────────────────

    // A server that DOES advertise `limits` must have it parsed into
    // `Tier::Server.server_limits`. This is the non-version-skew case: a
    // current-build server carrying the `/index/embed` timeout exemption.
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

    // A server that does NOT advertise `limits` (pre-dates the field) must
    // leave `Tier::Server.server_limits` as `None`: this is the exact
    // version-skew case: an old server still enforcing the legacy 30s
    // `/index/embed` budget with no exemption. `None` must never be
    // confused with "no limit" by a caller.
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

    // `embedder_token_cap` specifically must round-trip as `None` when the
    // server reports it as JSON `null` (e.g. embedder not ready, or an
    // external non-native backend with no known cap): distinct from the
    // whole `limits` object being absent.
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

    // Auto-discovered loopback server with no embedder (dim 0) → `Tier::Server`
    // (dim 0 means no `index.embed` check is relevant).
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

    // Explicit server_url with wrong dim → hard `Err` with an actionable message.
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

    // ── transport-scheme validation ──────────────────────────────────────────

    // A non-loopback `http://` URL must be rejected before any request is
    // sent: no mock is mounted, so a request would fail with "connection
    // refused" or similar rather than surfacing the validation error; the
    // assertion on the error message proves the reject happened pre-flight.
    #[tokio::test]
    async fn probe_url_rejects_non_loopback_http_no_request_sent() {
        // Deliberately no MockServer / no listener on this address: if
        // `probe_url` tried to send a request it would get a connection error,
        // not this validation message.
        let result = probe_url("http://team-server:4655", REMOTE_PROBE_TIMEOUT, false, None).await;
        let err = result.expect_err("non-loopback http:// must be a hard error");
        assert!(err.contains("loopback"), "got: {err}");
        assert!(err.contains("https"), "got: {err}");
    }

    // Same rejection applies to the loopback auto-discovery path (defensive;
    // auto-discovery URLs are always loopback in practice).
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

    // Loopback `http://` and `https://` URLs are accepted (proceed to the
    // actual health request against a mock server).
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

    // `/v1/health` must never carry an `Authorization` header: it is an
    // unauthenticated endpoint.
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

    // Health body carrying the PR-A `embedder: { state, detail }` sub-object.
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

    // `probe_url` must surface the server's `embedder.state` on `Tier::Server`.
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

    // A server that pre-dates the `embedder` field → `Unknown` (not an error).
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

    // `TIER` is a `OnceCell`: `get_tier` must probe at most once per process
    // and every later call must return the identical cached `Tier`, not
    // re-probe. This is what makes `EXPLICIT_PROBE_FAILURE` safe to read from
    // `Tier::Offline` rendering: there is no later probe in the same process
    // that could silently swap a fresh success in underneath a stale failure
    // annotation (or vice versa).
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

    // ── classification matrix: real reqwest errors, not hand-built chains ────

    // A genuine TCP connection-refused error through the real `reqwest`
    // client must classify as `Unreachable`, never `Tls`: no TLS layer is
    // ever reached, so `find_rustls_cause` must return `None` on it.
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

    // A genuine client-side timeout (the peer accepts the TCP connection but
    // never answers) must also classify as `Unreachable`, not `Tls`: a slow
    // or hung server is not a certificate problem.
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

    // A reachable server that answers with a non-2xx status (e.g. a
    // misconfigured reverse proxy, a 500, garbage) is neither `[tls: ...]`
    // nor `[unreachable]`: the transport and TLS both worked fine. This
    // path must leave `EXPLICIT_PROBE_FAILURE` unset entirely.
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

    // Auto-discovered (loopback) probe failures must never populate
    // `EXPLICIT_PROBE_FAILURE`: that cache exists only to annotate an
    // *explicit* `server_url` miss. A common "no local server running"
    // loopback miss must not leave behind a failure cause that a later
    // status render could misattribute to an unrelated explicit `server_url`.
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

    // ── get_inference_tier (2026-07-23 founder decision) ───
    //
    // These tests set `INKENTRY_STATE_DIR` / `INKENTRY_NO_SERVER`, both
    // process-global. Reusing the `inkentry_no_server_env` serial group (rather
    // than a new name) keeps them mutually exclusive with
    // `inkentry_no_server_forces_offline` above too: `get_inference_tier` reads
    // `INKENTRY_NO_SERVER` internally, so it must never run concurrently with a
    // test that transiently sets it.

    // `local_first` (the default reached once `server_url` is set, with no
    // explicit `mode`) must probe the LOCAL loopback embedder for inference,
    // never the configured `server_url`. The loopback mock is discovered via
    // the fixed-port fallback (step 3b), the step a mock can satisfy: step 3a
    // additionally requires the recorded pid to be a live `inkentry-server`,
    // which is pinned separately below. `server_url` is left pointed at an
    // address nothing mounts anything on, so the test would fail loudly
    // (connection error, not a silent pass) if the code ever tried it.
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
            // Deliberately never mocked: any accidental fallback to this
            // "remote" would surface as a connection/DNS error, not a silent
            // wrong-but-passing result.
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

    // Explicit offline (`mode = "offline"`) must short-circuit before any
    // probe, exactly like `get_tier`. `server_url` is set to an address
    // nothing mounts anything on, so any attempted probe would hang/error
    // rather than silently returning `Offline` for the right reason.
    //
    // Uses `cfg.mode = Some(SyncMode::Offline)` rather than
    // `INKENTRY_NO_SERVER=1` deliberately: that env var is process-global and
    // read by every concurrently-running test's `probe()`/`get_tier()`
    // call (e.g. `get_tier_probes_at_most_once_and_caches_the_result`
    // above, which is not in this lock group), so mutating it here would
    // reintroduce the exact cross-test race this comment is warning about.
    // `mode` is per-`Config` and carries no such risk.
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

    // `get_inference_tier_fresh`'s `cloud_first` branch must re-probe the
    // server on every call, never freezing on an earlier observation. This
    // is the one behavioural difference from `get_inference_tier` (whose
    // `cloud_first` branch reuses `get_tier`'s process-lifetime cache) and
    // the entire reason `wait_for_embedder` uses the `_fresh` variant: a
    // bug that made this branch delegate to `get_tier` too (i.e. collapse
    // to being identical to `get_inference_tier`) would still pass every
    // other `get_inference_tier_fresh` test in this file, since those only
    // ever make a single call each. This test calls it twice against a
    // mock whose response changes between calls and asserts the second
    // call observes the change, directly at the tier-fetch level (not
    // indirected through `wait_for_embedder`'s poll loop).
    //
    // Deliberately does not touch `get_tier`/`TIER` (the process-wide
    // `OnceCell`) at all, unlike a test that called `get_inference_tier`'s
    // `Cached` branch would have to: that cell is shared by every test in
    // this binary with no reset hook, so asserting on it directly here
    // would make this test's pass/fail depend on unrelated test ordering.
    #[tokio::test]
    #[serial_test::serial(inkentry_no_server_env)]
    async fn get_inference_tier_fresh_cloud_first_reprobes_every_call() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        unsafe { std::env::remove_var("INKENTRY_NO_SERVER") };

        let server = MockServer::start().await;
        // First health check: embedder still loading, no index.embed.
        Mock::given(method("GET"))
            .and(path("/v1/health"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(health_body_with_embedder("loading")),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // Every call after the first: embedder ready.
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

    // ── Recorded peer responses (version skew) ───────────────────────────────
    //
    // Everything above this line builds its health body with `health_body()`,
    // which is the shape we *believe* a peer has. These replay bodies captured
    // from real released `inkentry-server` binaries instead, so they can
    // contradict that belief rather than confirm it. Provenance for each file
    // is recorded in docs/version-skew.md.

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

    // v0.8.0 and v0.9.0 genuinely omit `embedder`, `embedding_dim`, and
    // `limits`: these files are what those binaries actually sent, not a
    // hand-built "imagine an old server" body. Each absent optional must land
    // on its documented conservative default rather than erroring or being
    // read as "unlimited".
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

    // The other half of the same contract: when a real peer does send the
    // optional objects, they must actually be read rather than defaulted away.
    // Without this, a parser that dropped every optional on the floor would
    // still pass the legacy test above.
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

    // A peer newer than this CLI will send fields this CLI has never heard of.
    // Ignoring them is the whole basis of the additive-only evolution rule in
    // docs/stability.md, so it is asserted against a real recorded body with
    // unknown fields grafted on rather than against a synthetic one.
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
        // A new enum member in an existing open field, which is the additive
        // change most likely to be mistaken for a parse error.
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

    // ── Blast radius of a single unreadable field ────────────────────────────
    //
    // `parse_health` maps *any* deserialization error onto the legacy
    // plain-text branch, so one field this CLI cannot read discards the entire
    // body: capabilities, embedding_dim and limits all vanish at once, with no
    // log line to say so. That amplification is what made the `embedder.state`
    // defect expensive, and it is a property of the parser rather than of that
    // one field. These assert the amplification does not fire for shapes a peer
    // can legitimately send.
    //
    // The signature of the fallback is unmistakable: limits None *and*
    // capabilities emptied, on a body that carries both.
    // Asserted on the *siblings* of the mutated field, never on the field
    // itself: the mutated field is allowed to degrade to its documented
    // default. `capabilities` is the sibling that makes the amplification
    // unambiguous, because the legacy fallback replaces it with
    // `legacy_memory_only()` and semantic search disappears from a body that
    // plainly advertised it.
    async fn assert_capabilities_survived(label: &str, mutated: serde_json::Value) {
        let tier = probe_recorded(mutated).await;
        assert!(
            matches!(tier.caps(), Some(c) if c.search_semantic && c.index_embed),
            "{label}: the whole health body was discarded, not just the field \
             under test; every advertised capability was lost with it, and \
             nothing was logged to say so"
        );
    }

    // This server serialises absent optionals as an explicit JSON `null` rather
    // than omitting the key: both recorded v0.9.x bodies carry
    // `embedder.detail: null`, and the v0.9.4/v0.9.5 loading bodies carry
    // `limits.embedder_token_cap: null`. So `null` is a shape this peer family
    // demonstrably emits, and `#[serde(default)]` does not cover it: it fills in
    // a *missing* key, never a present one holding `null`.
    //
    // `#[serde(other)]` closed the unrecognised-string case on `embedder.state`.
    // It does not close the `null` case, which reaches the identical outcome
    // through the identical field.
    #[tokio::test]
    async fn null_embedder_state_does_not_discard_the_rest_of_the_health_body() {
        let mut body = recorded_health("health-v0.9.5-ready.json");
        body["embedder"]["state"] = serde_json::Value::Null;
        assert_capabilities_survived("embedder.state: null", body).await;
    }

    // `limits` is tolerated as a whole (`Option<ServerLimits>`) but strict
    // inside: `embed_request_timeout_secs` and `max_batch_chunks` have no
    // default, so a null or absent value there takes the entire body down. The
    // third member of the same struct, `embedder_token_cap`, is already sent as
    // `null` by real binaries, so nulling-when-unknown is this server's
    // established habit for exactly this object.
    #[tokio::test]
    async fn a_null_limit_does_not_discard_the_rest_of_the_health_body() {
        let mut body = recorded_health("health-v0.9.5-ready.json");
        body["limits"]["max_batch_chunks"] = serde_json::Value::Null;
        assert_capabilities_survived("limits.max_batch_chunks: null", body).await;
    }

    // The counterpart sibling for the one mutation where `capabilities` is
    // itself the field under test: it is allowed to degrade to its own default,
    // so `limits` becomes the field that proves the body was not discarded.
    async fn assert_limits_survived(label: &str, mutated: serde_json::Value) {
        let tier = probe_recorded(mutated).await;
        assert!(
            tier.server_limits().is_some(),
            "{label}: the whole health body was discarded, not just the field \
             under test; the server's advertised limits were lost with it"
        );
    }

    // The same defect shape as the two above, on the six remaining members of
    // the health body. None of these was pinned before, and every one of them
    // discarded the entire body: a strict field nested inside a tolerated
    // structure costs `capabilities`, `embedding_dim` and `limits` together.
    // Each mutation is allowed to lose its own field and nothing else.
    #[tokio::test]
    async fn every_health_field_degrades_alone_rather_than_taking_the_body_down() {
        // A number field holding an explicit null, which is how this server
        // family already serialises the optionals it does not know
        // (`embedder.detail`, `limits.embedder_token_cap`).
        let mut null_dim = recorded_health("health-v0.9.5-ready.json");
        null_dim["embedding_dim"] = serde_json::Value::Null;
        assert_capabilities_survived("embedding_dim: null", null_dim).await;

        let mut null_bool = recorded_health("health-v0.9.5-ready.json");
        null_bool["accepts_pushed_vectors"] = serde_json::Value::Null;
        assert_capabilities_survived("accepts_pushed_vectors: null", null_bool).await;

        // Both identity fields are informational only: one is logged at debug,
        // the other drives a multi-user warning. Neither is worth the whole
        // body, so a peer that widens either type must not break the probe.
        let mut string_uid = recorded_health("health-v0.9.5-ready.json");
        string_uid["started_by"] = serde_json::json!("501");
        assert_capabilities_survived("started_by as a string", string_uid).await;

        let mut numeric_instance = recorded_health("health-v0.9.5-ready.json");
        numeric_instance["instance_id"] = serde_json::json!(12345);
        assert_capabilities_survived("instance_id as a number", numeric_instance).await;

        // `limits` reshaped wholesale, rather than nulled member by member.
        let mut limits_array = recorded_health("health-v0.9.5-ready.json");
        limits_array["limits"] = serde_json::json!([]);
        assert_capabilities_survived("limits sent as an array", limits_array).await;

        // A member with no default simply missing, which is the shape a peer
        // that retires a limit would send.
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

        // `capabilities` is the one field whose own default is indistinguishable
        // from the legacy fallback's effect on it, so `limits` is the sibling
        // that shows the rest of the body survived.
        let mut null_caps = recorded_health("health-v0.9.5-ready.json");
        null_caps["capabilities"] = serde_json::Value::Null;
        assert_limits_survived("capabilities: null", null_caps).await;
    }

    // The tolerances that do hold, pinned so a later tightening of the health
    // structs cannot quietly reintroduce the amplification above.
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

    // ── Independent re-enumeration of the health body's members ──────────────
    //
    // "No strict member remains" is the whole value of the lenient read, so it
    // is re-derived here rather than taken from a list. The member names come
    // from the body a real v0.9.5 peer sends, plus the one member this CLI
    // models that no peer sends yet, so a member added to `HealthBody` without
    // a lenient read is caught by the same loop that covers today's.

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

    // A discarded body is unmistakable: capabilities collapse to
    // `legacy_memory_only()`, limits vanish and the embedder reads Unknown, on
    // a body that plainly carried all three. Every signal is asserted except
    // the one belonging to the field under test, which is allowed to degrade.
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
        // Modelled by this CLI but absent from every recorded body, so the
        // fixture's own keys would not reach it.
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

    // The nested level the top-level helper cannot speak for: a member of
    // `limits` or `embedder` that this CLI cannot read must still cost at most
    // its own parent object.
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

    // Replaces one_unreadable_limit_discards_the_readable_limits_beside_it,
    // which pinned the all-or-nothing read as a decision. It was the wrong
    // decision on the chunk axis: losing an advertised cap raises the ceiling
    // this CLI plans around to its own maximum, so the degrade was more
    // permissive than what the peer published, not more conservative.
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

    // The cap a peer publishes is what the embed phase must plan around, so
    // the assertion above is carried through to the value that actually sizes
    // a request rather than stopping at the parsed struct.
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

    // ── The warning path ─────────────────────────────────────────────────────
    //
    // The warnings exist because silence is what let a single strict field
    // discard whole bodies unnoticed. A log line added to surface a silent
    // failure has to be checked for firing at all, and for what it carries.

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

    // The subscriber default is thread-local and `#[tokio::test]` is a
    // current-thread runtime, so the guard covers the awaits inside it without
    // disturbing tests running in parallel on other threads.
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
            // A negative integer is unreadable for every member of the struct,
            // which an unknown object is not: `embedder` tolerates a reshape
            // into one and reads it as Unknown rather than degrading.
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

    // The 100 kB body above is not valid JSON, so it only ever reached a
    // syntax error, and those carry no input. A body that *is* valid JSON but
    // is not an object reaches the struct error instead, and that one quotes
    // the whole body back. Same 100 kB, different arm, and only this one was
    // unbounded. The 200-char snippet stays deliberate: at this point the peer
    // is not a inkentry server at all, and without a sample of what it did send
    // the failure is undiagnosable.
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

    // The bounded snippet is not the only thing the warnings emit. serde
    // renders a wrong-typed string by quoting the value itself, so the
    // per-field warning carries whatever that field held, at whatever length
    // it held it. `/v1/health` is unauthenticated and the body is attacker
    // controlled the moment `server_url` points somewhere unintended.
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

    // The two arms above were each pinned by the body that broke that one arm.
    // A body is only ever routed to a single arm, so neither test can see a
    // regression in the other, and the arms were found unbounded one at a time
    // for exactly that reason. This drives every shape a peer can put on the
    // wire through the same two assertions.
    const CREDENTIAL_SHAPED: &str = "ghp_A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8";

    fn hostile_health_bodies() -> Vec<(&'static str, String)> {
        // Every credential is placed past the snippet's 200-char bound, so a
        // hit means the body was rendered further than the bound allows.
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

    // The head of a non-object body is rendered on purpose: at that point the
    // peer is not a inkentry server and the sample is the only diagnostic. What
    // must hold is that the deliberate exposure stops at its stated bound
    // rather than running to the end of whatever the peer sent.
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

    // A spoofed-loopback authority must be refused by the probe before any
    // request leaves, exactly as a plainly non-loopback host is. No mock server
    // is mounted: reaching the network at all would be the failure.
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

    // ── classify_responder (the trust policy, on every platform) ─────────────
    //
    // The decision itself, with the OS process query supplied as a bool rather
    // than run. This is the coverage the live-process tests below cannot give on
    // Windows, and it includes the happy path every command travels when a
    // daemon is running (`None`).

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

    // A responder that reports no instance_id at all cannot match the recorded
    // one, so it is refused just like a wrong id.
    #[test]
    fn classify_responder_refuses_a_responder_that_reports_no_instance_id() {
        assert_eq!(
            classify_responder(Some(4711), true, Some("recorded"), None),
            Some(Untrusted::InstanceIdMismatch)
        );
    }

    // The happy path: a recorded pid the OS query accepts and a reported
    // instance_id equal to the recorded one. This is what every command sees
    // when a daemon is running, and the one branch that must stay trusted so the
    // check cannot be satisfied by refusing everything.
    #[test]
    fn classify_responder_trusts_a_fully_matching_daemon() {
        assert_eq!(
            classify_responder(Some(4711), true, Some("id"), Some("id")),
            None
        );
    }

    // The discovery-trust test seam bypasses only the un-fakeable OS process
    // query, and only for `1`/`true`. Every other value, and an unset variable,
    // runs the real query (here a ghost pid, so a definite `false`). In both
    // serial groups: the discovery tests below read the same variable through
    // `untrusted_responder`, and the outbox and status relay tests set it
    // through their own state-dir guards, which use `server_state_dir_env`.
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

    // ── Loopback discovery trust checks (live process, Unix only) ────────────
    //
    // Step 3a hands every indexed source chunk to whoever answers the recorded
    // loopback port, so "something answered" is not enough to make it the
    // embedding backend. These drive the whole of `probe_loopback` against a
    // real recorded process, complementing the platform-independent
    // `classify_responder` tests above.
    //
    // Unix only: the positive PID case needs a live process whose command line
    // reads as `inkentry-server`, which `process_matches_server` finds via
    // `ps`. The Windows side of that check matches an image name, which a test
    // cannot fabricate without shipping a second binary, so the live-process
    // path stays unix-only and the cross-platform coverage is the pure policy
    // tests above plus the subprocess test in
    // `security_tests/loopback_discovery_trust.rs`.

    #[cfg(unix)]
    const RECORDED_INSTANCE_ID: &str = "00000000-0000-0000-0000-000000000001";

    // A live placeholder process, optionally named so `process_matches_server`
    // accepts it. The symlink is what puts the name in `ps -o args=`; the shell
    // loop is what keeps the shell itself alive, since a shell running a single
    // command execs it and loses that name.
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
                // The shell's own `sleep` children inherit whatever the test
                // harness handed this process, and nextest reports a test whose
                // output handles outlive it as leaky.
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

    // Today's happy path: the recorded PID is still the server and the health
    // body reports the recorded instance. A regression guard, not a RED test:
    // it must keep passing, so the new verification cannot be satisfied by
    // refusing everything.
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
            // Step 3b would probe a fixed port on the developer's own machine;
            // the state file is the only thing this test wants exercised.
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

    // A process that squats the recorded port and answers `/v1/health` is not
    // the recorded daemon: the recorded PID belongs to something that is not an
    // `inkentry-server`. The fixed-port fallback is pointed at the squatter too,
    // so falling through to it would re-discover the squatter and fail here.
    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial(inkentry_no_server_env, server_state_dir_env)]
    async fn a_squatter_on_the_recorded_port_is_not_the_embedding_backend() {
        let (_server, port) = mock_daemon(RECORDED_INSTANCE_ID).await;
        let squatter = Placeholder::spawn(false);
        let tmp = tempfile::TempDir::new().unwrap();
        // The instance_id matches, so the PID check is the only thing that can
        // refuse this responder.
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

    // A stale state file next to a different daemon on the same port: the PID
    // check passes, and only the recorded instance_id separates the two.
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
