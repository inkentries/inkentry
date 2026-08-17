use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use axum_server::tls_rustls::RustlsConfig;
use clap::Parser;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use inkentry_server::auth::ApiKeyAuth;
use inkentry_server::client_ip::TrustedProxies;
use inkentry_server::db::ServerDb;
use inkentry_server::rate_limiter::RateLimiter;
use inkentry_server::relay::{RelayPolicy, RelayRegistry};
use inkentry_server::{
    ApiDoc, AppState, EmbedderSlot, default_conflict_threshold, host_is_loopback, router,
};
use utoipa::OpenApi;

#[cfg(feature = "embed-native")]
use inkentry_embed::DIM as NATIVE_EMBED_DIM;
// Via inkentry-core (always linked); inkentry_embed is only present under embed-native.
use inkentry_core::embeddings::MODEL_ID as NATIVE_MODEL_ID;
#[cfg(feature = "embed-native")]
use inkentry_server::embed_hub;

mod server_llm;
use server_llm::{ServerLlm, check_llm_transport, resolve_llm_key};

#[derive(Parser, Debug)]
#[command(
    name = "inkentry-server",
    version,
    about = "Shared memory server for inkentry",
    before_help = concat!("inkentry-server v", env!("CARGO_PKG_VERSION"))
)]
struct Args {
    /// Port to listen on
    #[arg(long, default_value = "7777")]
    port: u16,

    /// Host/address to bind. Defaults to loopback (`127.0.0.1`): a local
    /// plaintext-HTTP server, no API key required. To serve a team/remote
    /// server, bind a routable address (e.g. `--host 0.0.0.0`) together with
    /// `--tls-cert`/`--tls-key` and an API key — the server terminates HTTPS
    /// itself. A non-loopback bind is refused unless both TLS and a key are set.
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// Path to the server SQLite database
    #[arg(long, default_value = "inkentry.db")]
    db: PathBuf,

    /// Shared API key (Bearer token) passed inline. Visible in the process
    /// table and `systemctl show`, so prefer --key-file or INKENTRY_SERVER_KEY
    /// for real deployments. Leave all key sources unset to disable auth
    /// (loopback dev only). Overrides every other key source.
    #[arg(long)]
    key: Option<String>,

    /// Read the shared API key from a file (its whole trimmed contents). A
    /// first-class alternative to INKENTRY_SERVER_KEY, not a fallback: point it
    /// at a `0600` file, or at `$CREDENTIALS_DIRECTORY/server-key` when run
    /// under systemd `LoadCredential=`. When run under systemd, a
    /// `server-key` credential is picked up automatically even without this
    /// flag. Read failure is fatal.
    #[arg(long, value_name = "PATH")]
    key_file: Option<PathBuf>,

    /// PEM certificate chain (leaf + intermediates) for in-process HTTPS. Set
    /// with `--tls-key` (both or neither). Distinct from `--key`/`--key-file`,
    /// which are the bearer API key — a different secret. The certificate chain
    /// is public; a routable bind needs this plus `--tls-key` and an API key.
    #[arg(long, env = "INKENTRY_SERVER_TLS_CERT", value_name = "PATH")]
    tls_cert: Option<PathBuf>,

    /// PEM private key matching `--tls-cert`. Set with `--tls-cert` (both or
    /// neither). A high-value secret: supply via a systemd credential or a
    /// `0600` root-owned file, never an `Environment=` line.
    #[arg(long, env = "INKENTRY_SERVER_TLS_KEY", value_name = "PATH")]
    tls_key: Option<PathBuf>,

    /// IP address of a reverse proxy whose `X-Forwarded-For` header this server
    /// should believe when identifying a client for rate limiting. Repeatable,
    /// or comma-separated in the environment variable.
    ///
    /// Empty by default, and that default is the supported shape: ADR-066 gives
    /// the server in-process HTTPS so a team deployment has nothing in front of
    /// it. Set this only if you genuinely run a proxy — naming an address that
    /// is not one lets any caller reaching the server from that address pick its
    /// own rate-limit bucket.
    #[arg(
        long = "trusted-proxy",
        env = "INKENTRY_TRUSTED_PROXIES",
        value_delimiter = ',',
        value_name = "IP"
    )]
    trusted_proxy: Vec<std::net::IpAddr>,

    /// Embedding dimension expected from clients (must match the team's model).
    /// Default: 896 (F2LLM-v2-330M).
    #[arg(long, default_value = "896")]
    embedding_dim: usize,

    /// Cosine similarity threshold for conflict detection (0.0–1.0). New entries with
    /// similarity ≥ this value to an existing active entry trigger a 409 response.
    /// Set to 1.0 to disable conflict detection.
    #[arg(long, default_value_t = default_conflict_threshold())]
    conflict_threshold: f32,

    /// Directory holding a pre-provisioned F2LLM-v2-330M GGUF + tokenizer (see
    /// "Air-gapped / no-egress install" in docs/server-setup.md), for hosts
    /// with no route to huggingface.co. When set, the bundled native embedder
    /// loads from this directory instead of the Hugging Face Hub: zero
    /// network access, at startup or at runtime. Only consulted when the
    /// bundled native embedder is the active backend; ignored otherwise.
    #[arg(long, env = "INKENTRY_MODEL_DIR", value_name = "PATH")]
    model_dir: Option<PathBuf>,

    /// Base URL of an OpenAI-compatible chat completions server for LLM features
    /// (`/llm/complete`). Overrides `INKENTRY_LLM_URL`.
    #[arg(long, env = "INKENTRY_LLM_URL")]
    llm_url: Option<String>,

    /// LLM model name (e.g. `google/gemma-3n-e4b`). Overrides `INKENTRY_LLM_MODEL`.
    #[arg(long, env = "INKENTRY_LLM_MODEL", default_value = "")]
    llm_model: String,

    /// `reasoning_effort` sent on every LLM request, to suppress chain-of-thought
    /// on reasoning models. Defaults to `none` (reasoning off), because
    /// harvest wants the answer, not the model's thinking, and an
    /// unbounded reasoning pass exhausts the token budget before any content is
    /// emitted. Set `minimal`/`low`/`medium`/`high` to allow it, or `default` to
    /// omit the field entirely for endpoints that reject it.
    #[arg(
        long,
        env = "INKENTRY_LLM_REASONING_EFFORT",
        default_value = "none",
        value_name = "EFFORT"
    )]
    llm_reasoning_effort: String,

    /// Credential for the `--llm-url` endpoint, passed inline. Visible in the
    /// process table: prefer `--llm-key-file` or `INKENTRY_LLM_KEY`. Distinct
    /// from `--key`, which is this server's own inbound bearer.
    #[arg(long, value_name = "KEY")]
    llm_key: Option<String>,

    /// File whose whole (trimmed) contents are the `--llm-url` credential.
    /// An unreadable path is fatal, never a fall-through to another source.
    #[arg(long, value_name = "PATH")]
    llm_key_file: Option<PathBuf>,

    /// Print the OpenAPI spec as JSON and exit (for Postman / Newman import).
    #[arg(long)]
    print_openapi: bool,

    /// Probe this server's own `/v1/health` on the configured `--host`/`--port`,
    /// then exit 0 if live or non-zero otherwise. Self-contained container
    /// HEALTHCHECK that needs no curl/wget in the runtime image. A wildcard
    /// `--host` is probed over loopback.
    #[arg(long)]
    health_check: bool,
}

/// Map the `--llm-reasoning-effort` value to what travels on the wire: `default`
/// / `model` / blank means "omit the field, use the model's own default";
/// anything else is sent verbatim (`none` suppresses reasoning).
fn normalize_reasoning_effort(v: &str) -> Option<String> {
    let v = v.trim();
    if v.is_empty() || v.eq_ignore_ascii_case("default") || v.eq_ignore_ascii_case("model") {
        None
    } else {
        Some(v.to_string())
    }
}

fn main() -> Result<()> {
    // Bound candle's CPU threads BEFORE the runtime / first candle op: candle
    // reads RAYON_NUM_THREADS live for gemm and caches its private rayon pool in
    // a OnceLock on first use, so the env must be set while still single-threaded
    // (set_var is unsafe in edition 2024 for that reason). Setting only an
    // already-unset var keeps a user's RAYON_NUM_THREADS authoritative.
    let budget = resolve_embed_thread_budget();
    unsafe {
        if std::env::var_os("RAYON_NUM_THREADS").is_none() {
            std::env::set_var("RAYON_NUM_THREADS", budget.threads.to_string());
        }
        if std::env::var_os("CANDLE_NUM_THREADS").is_none() {
            std::env::set_var("CANDLE_NUM_THREADS", budget.threads.to_string());
        }
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?
        .block_on(run(budget))
}

async fn run(budget: ThreadBudget) -> Result<()> {
    // Register sqlite-vec extension for every connection in this process.
    #[allow(clippy::missing_transmute_annotations)]
    unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    }

    // Parse args and handle --print-openapi before any subscriber/log init, so
    // the emitted document is pure JSON on stdout (CI diffs it byte-for-byte).
    let args = Args::parse();

    if args.print_openapi {
        println!("{}", ApiDoc::openapi().to_pretty_json()?);
        return Ok(());
    }

    if args.health_check {
        return run_health_check(&args.host, args.port).await;
    }

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(fmt::layer())
        .init();

    tracing::info!(
        threads = budget.threads,
        source = budget.source,
        "embed CPU thread budget resolved"
    );

    // Resolve the API key from --key / --key-file / INKENTRY_SERVER_KEY /
    // systemd LoadCredential (see resolve_api_key for precedence). A blank
    // value from any source counts as "no key" — a set-but-empty
    // `INKENTRY_SERVER_KEY` (docker-compose's `${INKENTRY_SERVER_KEY:-}` default)
    // must read as unauthenticated, not as a broken empty-token key.
    let env_key = std::env::var("INKENTRY_SERVER_KEY").ok();
    let credentials_dir = std::env::var_os("CREDENTIALS_DIRECTORY").map(PathBuf::from);
    let api_key = resolve_api_key(
        args.key.as_deref(),
        args.key_file.as_deref(),
        env_key.as_deref(),
        credentials_dir.as_deref(),
    )?;

    // TLS flags are all-or-nothing (ADR-066 §2).
    let tls_enabled = resolve_tls_enabled(args.tls_cert.is_some(), args.tls_key.is_some())?;

    // Bind-safety (ADR-066 §4): loopback binds are always allowed; a non-loopback
    // bind is refused unless BOTH in-process TLS and an API key are configured.
    // Fail fast, before touching the DB or warming the embedder.
    check_bind_safety(&args.host, args.port, api_key.is_some(), tls_enabled)?;

    // The upstream LLM credential never comes from a keychain: this process is
    // usually a detached daemon with no user session, so a keychain read would
    // be an invisible, unanswerable authorization prompt. The spawning CLI
    // resolves it and hands it over out of band.
    let env_llm_key = std::env::var("INKENTRY_LLM_KEY").ok();
    let llm_key = resolve_llm_key(
        args.llm_key.as_deref(),
        args.llm_key_file.as_deref(),
        env_llm_key.as_deref(),
    )?;
    // Alongside the bind check, and for the same reason: refuse a credential
    // over a plaintext non-loopback hop before touching the DB or the embedder.
    if let Some(url) = &args.llm_url {
        check_llm_transport(url, llm_key.is_some())?;
    }

    let db = ServerDb::open(&args.db, args.embedding_dim, NATIVE_MODEL_ID)
        .with_context(|| format!("opening server db at {}", args.db.display()))?;

    let instance_id = db
        .get_or_create_instance_id()
        .context("initialising instance_id")?;
    tracing::debug!("instance_id: {instance_id}");

    let started_by = effective_uid();

    if api_key.is_none() {
        tracing::warn!(
            "No API key configured — server is running without authentication. \
             Set --key-file, INKENTRY_SERVER_KEY, or --key for production use."
        );
    }

    // Single-trust-domain notice (ADR-056): a keyed, non-loopback bind is a
    // shared/team server. The shared key is the *only* boundary — every
    // keyholder is a full administrator of every project on this instance
    // (list, read, write, supersede, archive, delete). This is intended
    // behaviour, not a bug; teams that must not see each other's memory need
    // separate server instances (separate keys, separate databases), not a
    // per-project ACL on one instance. Loopback binds are a single developer's
    // own machine, so the notice does not apply there.
    warn_single_trust_domain(&args.host, api_key.is_some());

    // Build the auth provider from the configured key.
    let auth: Arc<dyn inkentry_server::auth::AuthProvider> =
        Arc::new(ApiKeyAuth::new(api_key.clone()));

    // Build the server-side embedder readiness slot. The bundled native
    // embedder is CPU-/download-heavy, so the slot starts `loading` and the
    // actual `embed_hub::load_from_hub()` is deferred to a background task
    // spawned *after* the listener binds (below) — that way `/v1/health` is
    // live immediately with `embedder.state = "loading"` instead of being
    // dark for the whole first-run model download. A server built without the
    // `embed-native` feature has no embed path at all, so the slot is
    // `disabled` (embed endpoints return a permanent 400).
    let (embedder, load_native): (EmbedderSlot, bool) = {
        #[cfg(feature = "embed-native")]
        {
            (EmbedderSlot::loading(), true)
        }
        #[cfg(not(feature = "embed-native"))]
        {
            (EmbedderSlot::disabled(), false)
        }
    };

    let llm: Option<Arc<dyn inkentry_core::llm::LlmBackend>> = if let Some(base_url) = args.llm_url
    {
        let model = if args.llm_model.is_empty() {
            "default".to_string()
        } else {
            args.llm_model.clone()
        };
        tracing::info!("server-side LLM enabled: {base_url} model={model}");
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .context("building HTTP client for server-side LLM")?;
        Some(Arc::new(ServerLlm {
            client,
            base_url,
            model,
            api_key: llm_key,
            reasoning_effort: normalize_reasoning_effort(&args.llm_reasoning_effort),
        }))
    } else {
        None
    };

    // Server-side max_tokens ceiling: env var or 8192 default.
    let max_tokens_ceiling: usize = std::env::var("INKENTRY_MAX_TOKENS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8192);

    // Per-principal rate limiter: 60 requests per minute by default.
    let rate_limiter = Arc::new(RateLimiter::new(60, 60));

    let trusted_proxies = TrustedProxies::new(args.trusted_proxy.iter().copied());
    if !trusted_proxies.is_empty() {
        tracing::warn!(
            "trusting X-Forwarded-For from {:?}: any client that can reach the server \
             from one of those addresses chooses its own rate-limit bucket",
            trusted_proxies.addrs()
        );
    }

    let state = AppState {
        db: Arc::new(tokio::sync::Mutex::new(db)),
        auth,
        conflict_threshold: args.conflict_threshold,
        embedder,
        embed_admission: inkentry_server::EmbedAdmission::new(
            inkentry_server::EMBED_QUEUE_CAPACITY,
            inkentry_server::EMBED_BUSY_RETRY_AFTER_SECS,
        ),
        llm,
        max_tokens_ceiling,
        rate_limiter,
        trusted_proxies,
        instance_id,
        started_by,
        // The relay only ever talks to the team servers this machine's own
        // config declares, and only on a loopback bind (see the `relay`
        // module docs).
        relay: RelayRegistry::for_bind(&args.host, RelayPolicy::from_local_config()),
    };

    // Keep a handle to the embedder slot so the background load task can flip it
    // `loading → ready | unavailable` after the listener binds.
    let embedder_slot = state.embedder.clone();
    let model_dir = args.model_dir.clone();

    let app = router(state);
    let addr: SocketAddr = format!("{}:{}", args.host, args.port)
        .parse()
        .context("parsing bind address")?;

    // Load TLS material before binding so a bad cert/key fails fast. Install
    // `ring` as the process crypto provider (NOT rustls' default aws-lc-rs,
    // which needs a cmake/C/NASM build toolchain); ignore the error if another
    // component already installed one.
    let tls_config = if tls_enabled {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let cert = args.tls_cert.as_deref().expect("tls_enabled ⇒ cert set");
        let key = args.tls_key.as_deref().expect("tls_enabled ⇒ key set");
        let config = RustlsConfig::from_pem_file(cert, key)
            .await
            .with_context(|| {
                format!(
                    "loading TLS cert {} / key {}",
                    cert.display(),
                    key.display()
                )
            })?;
        Some(config)
    } else {
        None
    };

    // Bind first: `/v1/health` must be reachable the instant the port is bound,
    // *before* the (potentially multi-minute, ~339 MB) native model download. A
    // std listener backs both serve paths (plaintext axum, TLS axum-server), so
    // the single bind-before-warm point is preserved either way.
    let listener = std::net::TcpListener::bind(addr).with_context(|| format!("binding {addr}"))?;
    // Both serve paths register the fd with tokio, which rejects a blocking fd.
    listener
        .set_nonblocking(true)
        .context("setting listener non-blocking")?;
    let scheme = if tls_config.is_some() {
        "https"
    } else {
        "http"
    };
    tracing::info!("inkentry-server listening on {scheme}://{addr}");

    // Load the native embedder on a background task now that health is live.
    // Both `embed_hub::load_from_hub()` (network) and `load_from_model_dir()`
    // (offline, when `--model-dir`/`INKENTRY_MODEL_DIR` is set) are
    // blocking/CPU-heavy, so run whichever applies on the blocking pool;
    // publish the backend into the slot on success (state → ready) or record
    // the failure (state → unavailable) either way: an offline host with no
    // (or bad) provisioned artifacts reaches the same terminal `unavailable`
    // state as a failed Hub download, just with an error naming the offline
    // docs instead of a connection failure. Only the native path warms up
    // here: disabled slots are already in a terminal state.
    #[cfg(feature = "embed-native")]
    if load_native {
        let slot = embedder_slot.clone();
        tokio::spawn(async move {
            let load = move || match model_dir {
                Some(dir) => embed_hub::load_from_model_dir(&dir),
                None => embed_hub::load_from_hub(),
            };
            match tokio::task::spawn_blocking(load).await {
                Ok(Ok(native)) => {
                    tracing::info!("native embedding model loaded (dim={})", NATIVE_EMBED_DIM);
                    slot.set_ready(
                        Arc::new(native) as Arc<dyn inkentry_core::embeddings::EmbeddingBackend>
                    );
                }
                Ok(Err(e)) => {
                    let msg = embedder_load_failure_message(&e);
                    tracing::warn!(
                        "native embedding model failed to load: {msg}; embedder unavailable"
                    );
                    slot.set_unavailable(msg);
                }
                Err(join_err) => {
                    let msg = embedder_load_failure_message(format_args!(
                        "embedder load task panicked: {join_err}"
                    ));
                    tracing::warn!("{msg}");
                    slot.set_unavailable(msg);
                }
            }
        });
    }
    // Silence "unused" for the non-embed-native build (no background load).
    #[cfg(not(feature = "embed-native"))]
    let _ = (load_native, &embedder_slot, &model_dir);

    let make_service = app.into_make_service_with_connect_info::<SocketAddr>();
    match tls_config {
        // axum-server accepts the pre-bound std listener (bind-before-warm kept).
        Some(config) => {
            axum_server::from_tcp_rustls(listener, config)
                .context("adopting std listener for TLS")?
                .serve(make_service)
                .await?;
        }
        // Plaintext loopback path, unchanged.
        None => {
            let listener =
                tokio::net::TcpListener::from_std(listener).context("adopting std listener")?;
            axum::serve(listener, make_service).await?;
        }
    }

    Ok(())
}

// ── Native embedder load failure ──────────────────────────────────────────────

/// Prefix a native-embedder load failure so `/v1/health`'s `embedder.detail`
/// reads unambiguously as terminal: the underlying `anyhow::Context` message
/// (e.g. "creating model cache dir ...") otherwise reads like in-progress
/// bootstrap text rather than a failure.
#[cfg(feature = "embed-native")]
fn embedder_load_failure_message(context: impl std::fmt::Display) -> String {
    format!("failed: {context}")
}

// ── Embed CPU thread budget ───────────────────────────────────────────────────

/// Resolved candle CPU-thread budget plus the source it came from, for the
/// startup log line.
struct ThreadBudget {
    threads: usize,
    source: &'static str,
}

/// CPU threads candle may use for a forward pass, so a running embed leaves
/// cores free to serve requests. Precedence: `INKENTRY_EMBED_THREADS` > an
/// already-set `RAYON_NUM_THREADS` > `max(1, physical - 2)`. A zero or
/// unparseable override is `None`/`Some(0)` here and falls through.
fn embed_thread_budget(
    physical: usize,
    rayon_override: Option<usize>,
    inkentry_override: Option<usize>,
) -> usize {
    if let Some(n) = inkentry_override.filter(|&n| n > 0) {
        return n;
    }
    if let Some(n) = rayon_override.filter(|&n| n > 0) {
        return n;
    }
    physical.saturating_sub(2).max(1)
}

/// Read the physical core count and env overrides, then resolve the budget and
/// which source won (for the startup log).
fn resolve_embed_thread_budget() -> ThreadBudget {
    fn env_threads(key: &str) -> Option<usize> {
        std::env::var(key).ok().and_then(|v| v.trim().parse().ok())
    }
    let rayon = env_threads("RAYON_NUM_THREADS");
    let inkentry = env_threads("INKENTRY_EMBED_THREADS");
    let threads = embed_thread_budget(num_cpus::get_physical(), rayon, inkentry);
    let source = if inkentry.filter(|&n| n > 0).is_some() {
        "INKENTRY_EMBED_THREADS"
    } else if rayon.filter(|&n| n > 0).is_some() {
        "RAYON_NUM_THREADS"
    } else {
        "default"
    };
    ThreadBudget { threads, source }
}

// ── Bind-safety guard ─────────────────────────────────────────────────────────

/// Normalise a configured API key: a blank/whitespace value (e.g. a
/// set-but-empty `INKENTRY_SERVER_KEY`, or an empty credential file) becomes
/// `None`, so "empty key" is treated as "no key" everywhere — both by the
/// bind-safety guard and by the auth provider.
///
/// A leading UTF-8 BOM is stripped first: an editor that writes one (the
/// Windows default) would otherwise put U+FEFF inside the key, and `str::trim`
/// cannot remove it because U+FEFF is a format character, not whitespace. The
/// strip runs before the trim so a BOM ahead of leading whitespace is handled.
fn normalize_api_key(key: Option<&str>) -> Option<String> {
    key.map(|k| k.strip_prefix('\u{feff}').unwrap_or(k))
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_owned)
}

/// Filename systemd exposes for `LoadCredential=server-key:...` under
/// `$CREDENTIALS_DIRECTORY`.
const SERVER_KEY_CREDENTIAL: &str = "server-key";

/// Accept a resolved key, or refuse startup when it can never authenticate.
///
/// A bearer token reaches the auth provider through `HeaderValue::to_str`,
/// which yields only printable ASCII and tab, so a key holding any other byte
/// cannot equal any token a client is able to send. That is not a likely
/// failure but an unsatisfiable one, so it is a hard error rather than a
/// warning, and it names `source` because the resulting 401 cannot.
fn accept_key(key: String, source: &str) -> Result<Option<String>> {
    if let Some(byte) = key
        .bytes()
        .find(|b| !(0x20..0x7f).contains(b) && *b != b'\t')
    {
        anyhow::bail!(
            "the API key from {source} contains a byte (0x{byte:02x}) that cannot be sent in an \
             HTTP Authorization header, so no request could ever authenticate against it. \
             Use a key of printable ASCII characters."
        );
    }
    Ok(Some(key))
}

/// Resolve the shared API key from all supported sources, in precedence order
/// (a blank value at any level is ignored and falls through to the next):
///
/// 1. `--key <value>` — inline flag (most explicit).
/// 2. `--key-file <path>` — explicit file; a read failure is fatal.
/// 3. `INKENTRY_SERVER_KEY` — environment variable.
/// 4. `$CREDENTIALS_DIRECTORY/server-key` — systemd `LoadCredential=`, used
///    automatically when the credential is present.
///
/// The credential file (3/4) and the env var are equal first-class sources, not
/// fallbacks: a systemd deployment sets only the credential and gets it; an
/// operator outside systemd sets only the env var and gets it.
fn resolve_api_key(
    key: Option<&str>,
    key_file: Option<&std::path::Path>,
    env_key: Option<&str>,
    credentials_dir: Option<&std::path::Path>,
) -> Result<Option<String>> {
    if let Some(k) = normalize_api_key(key) {
        return accept_key(k, "--key");
    }
    if let Some(path) = key_file {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading --key-file {}", path.display()))?;
        if let Some(k) = normalize_api_key(Some(&raw)) {
            return accept_key(k, &format!("--key-file {}", path.display()));
        }
    }
    if let Some(k) = normalize_api_key(env_key) {
        return accept_key(k, "INKENTRY_SERVER_KEY");
    }
    if let Some(dir) = credentials_dir {
        let path = dir.join(SERVER_KEY_CREDENTIAL);
        match std::fs::read_to_string(&path) {
            Ok(raw) => {
                if let Some(k) = normalize_api_key(Some(&raw)) {
                    return accept_key(k, &format!("systemd credential {}", path.display()));
                }
            }
            // A credentials dir without our credential is normal (systemd may
            // be exporting other credentials); only a real read error is fatal.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(anyhow::Error::new(e)
                    .context(format!("reading systemd credential {}", path.display())));
            }
        }
    }
    Ok(None)
}

/// `--tls-cert`/`--tls-key` are all-or-nothing (ADR-066 §2): a cert chain needs
/// its matching private key. Returns whether in-process TLS is enabled, or errs
/// when exactly one of the two is set.
fn resolve_tls_enabled(cert_set: bool, key_set: bool) -> Result<bool> {
    match (cert_set, key_set) {
        (true, true) => Ok(true),
        (false, false) => Ok(false),
        (true, false) => {
            anyhow::bail!(
                "--tls-cert was set without --tls-key: a certificate chain needs its matching private key."
            )
        }
        (false, true) => {
            anyhow::bail!(
                "--tls-key was set without --tls-cert: a private key needs its matching certificate chain."
            )
        }
    }
}

/// TLS-aware bind-safety guard (ADR-066 §4). Encodes "local = HTTP no key,
/// remote = HTTPS + key":
///
/// | Bind | TLS | Key | Result |
/// |---|---|---|---|
/// | loopback | any | any | allow |
/// | non-loopback | no | any | refuse (no plaintext off-host) |
/// | non-loopback | yes | no | refuse (remote requires an API key) |
/// | non-loopback | yes | yes | allow (the remote HTTPS path) |
///
/// Loopback binds are always allowed (unreachable off-host). Plaintext off-host
/// stays refused with no opt-out. The refusal names the interface/port and
/// points the operator at `--tls-cert`/`--tls-key`.
fn check_bind_safety(host: &str, port: u16, key_is_set: bool, tls_is_set: bool) -> Result<()> {
    if host_is_loopback(host) {
        return Ok(());
    }

    if !tls_is_set {
        // Non-loopback plaintext, keyed or not: an open server, or the bearer
        // key crossing the wire in cleartext. Refused unconditionally.
        anyhow::bail!(
            "Refusing to bind to non-loopback address '{host}:{port}' over plaintext HTTP.\n\
             A server reachable from other machines must terminate TLS in-process. Either:\n  \
             • pass --tls-cert <pem> --tls-key <pem> and an API key \
             (--key / --key-file / INKENTRY_SERVER_KEY) to serve HTTPS on {host}:{port}, or\n  \
             • bind to loopback (the default --host 127.0.0.1) for local-only plaintext use."
        );
    }

    if !key_is_set {
        // TLS is configured but no bearer key: a remote HTTPS server must
        // authenticate its callers.
        anyhow::bail!(
            "Refusing to bind to non-loopback address '{host}:{port}' with TLS but no API key.\n\
             A remote HTTPS server must require an API key so callers are authenticated. Either:\n  \
             • set --key / --key-file / INKENTRY_SERVER_KEY, or\n  \
             • bind to loopback (the default --host 127.0.0.1) for local-only use."
        );
    }

    // Non-loopback + TLS + key: the remote HTTPS path (ADR-066 §4). Allowed.
    Ok(())
}

/// Whether a keyed, non-loopback bind is a shared/team server that should get
/// the ADR-056 single-trust-domain notice: the shared key is the tenancy
/// boundary, and every keyholder is a full administrator of every project on
/// the instance — this is intended behaviour, not a defect. `false` for a
/// loopback bind (a developer's own machine) or when no key is set
/// (`check_bind_safety` already refuses a keyless non-loopback bind, so in
/// practice this is never `true` with `key_is_set == false`).
fn should_warn_single_trust_domain(host: &str, key_is_set: bool) -> bool {
    !host_is_loopback(host) && key_is_set
}

/// Emit the ADR-056 single-trust-domain notice (see
/// `should_warn_single_trust_domain` for the firing condition).
fn warn_single_trust_domain(host: &str, key_is_set: bool) {
    if should_warn_single_trust_domain(host, key_is_set) {
        tracing::warn!(
            "Shared server: every keyholder can read, modify and permanently delete \
             ALL projects' memory on this server. This instance is a single trust \
             domain — the shared key is the only access boundary, not a per-project \
             one. Run separate servers (separate keys) if you need isolation between \
             teams or projects. See docs/adr/056-oss-server-tenancy-model.md."
        );
    }
}

// ── Self-contained health probe ───────────────────────────────────────────────

/// Host to aim the health probe at. A wildcard bind (`0.0.0.0` / `::` / empty)
/// is not itself connectable, so probe over loopback; any other host is probed
/// as-is.
fn health_probe_host(host: &str) -> &str {
    match host.trim() {
        "0.0.0.0" | "" => "127.0.0.1",
        "::" => "::1",
        h => h,
    }
}

/// Build the `/v1/health` URL for the probe, bracketing IPv6 literals.
fn health_probe_url(host: &str, port: u16) -> String {
    let h = health_probe_host(host);
    if h.contains(':') {
        format!("http://[{h}]:{port}/v1/health")
    } else {
        format!("http://{h}:{port}/v1/health")
    }
}

/// Probe `/v1/health` and return `Ok` iff the server answers `2xx`. Backs the
/// container HEALTHCHECK so the runtime image needs no curl/wget; a non-`Ok`
/// return propagates to a non-zero process exit.
async fn run_health_check(host: &str, port: u16) -> Result<()> {
    let url = health_probe_url(host, port);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(4))
        .build()
        .context("building health-check HTTP client")?;
    let status = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("health probe request to {url} failed"))?
        .status();
    anyhow::ensure!(
        status.is_success(),
        "health probe to {url} returned HTTP {status}"
    );
    Ok(())
}

// ── Effective UID helper ──────────────────────────────────────────────────────

/// Return the effective user ID of the current process (Unix), or `None` on Windows.
fn effective_uid() -> Option<u32> {
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

// ── Args default tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod arg_tests {
    use super::{Args, normalize_reasoning_effort};
    use clap::Parser;

    // Reasoning is off by default: harvest wants the answer, and an
    // unbounded reasoning pass exhausts the token budget before any content
    // arrives. The default must send `none`.
    #[test]
    fn reasoning_is_disabled_by_default() {
        let args = Args::parse_from(["inkentry-server"]);
        assert_eq!(args.llm_reasoning_effort, "none");
        assert_eq!(
            normalize_reasoning_effort(&args.llm_reasoning_effort),
            Some("none".to_string()),
            "the default must be sent on the wire as reasoning_effort=none"
        );
    }

    // Trusting a forwarded header is an explicit operator decision, never a
    // default: with no proxy configured the rate limiter must fall back to the
    // TCP peer, which the caller cannot choose.
    #[test]
    #[serial_test::serial(trusted_proxies_env)]
    fn no_proxy_is_trusted_by_default() {
        let args = Args::parse_from(["inkentry-server"]);
        assert!(args.trusted_proxy.is_empty());
    }

    #[test]
    fn trusted_proxies_accept_repetition_and_comma_separation() {
        let args = Args::parse_from([
            "inkentry-server",
            "--trusted-proxy",
            "10.0.0.5,10.0.0.6",
            "--trusted-proxy",
            "2001:db8::1",
        ]);
        assert_eq!(
            args.trusted_proxy,
            vec![
                "10.0.0.5".parse::<std::net::IpAddr>().unwrap(),
                "10.0.0.6".parse().unwrap(),
                "2001:db8::1".parse().unwrap(),
            ]
        );
    }

    // The env var is the container/systemd path, and it reaches a `Vec<IpAddr>`
    // through clap's `value_delimiter` rather than any code of ours, so its
    // comma-splitting is worth pinning independently of the flag.
    #[test]
    #[serial_test::serial(trusted_proxies_env)]
    fn trusted_proxies_are_comma_split_from_the_environment() {
        // SAFETY: pinned to the `trusted_proxies_env` serial group, so no other
        // test reads or writes this variable concurrently.
        unsafe { std::env::set_var("INKENTRY_TRUSTED_PROXIES", "10.0.0.5,2001:db8::1") };
        let args = Args::parse_from(["inkentry-server"]);
        unsafe { std::env::remove_var("INKENTRY_TRUSTED_PROXIES") };
        assert_eq!(
            args.trusted_proxy,
            vec![
                "10.0.0.5".parse::<std::net::IpAddr>().unwrap(),
                "2001:db8::1".parse().unwrap(),
            ]
        );
    }

    #[test]
    #[serial_test::serial(trusted_proxies_env)]
    fn an_environment_trusted_proxy_that_is_not_an_ip_is_rejected_at_startup() {
        // SAFETY: see the sibling test; same serial group.
        unsafe { std::env::set_var("INKENTRY_TRUSTED_PROXIES", "10.0.0.5,proxy.internal") };
        let parsed = Args::try_parse_from(["inkentry-server"]);
        unsafe { std::env::remove_var("INKENTRY_TRUSTED_PROXIES") };
        assert!(
            parsed.is_err(),
            "one bad entry must fail startup rather than silently narrowing the trust list"
        );
    }

    #[test]
    fn a_trusted_proxy_that_is_not_an_ip_is_rejected_at_startup() {
        assert!(
            Args::try_parse_from(["inkentry-server", "--trusted-proxy", "proxy.internal"]).is_err(),
            "a hostname would have to be resolved per request to compare against a peer"
        );
    }

    #[test]
    fn reasoning_effort_default_and_model_and_blank_omit_the_field() {
        for opt_out in ["default", "Default", "model", "  ", ""] {
            assert_eq!(
                normalize_reasoning_effort(opt_out),
                None,
                "'{opt_out}' must omit reasoning_effort so strict endpoints aren't sent it"
            );
        }
        assert_eq!(
            normalize_reasoning_effort("minimal"),
            Some("minimal".to_string()),
            "an explicit effort level passes through verbatim"
        );
    }

    /// The server binary default host must be loopback (127.0.0.1), not the
    /// wildcard (0.0.0.0). The wildcard bind is an explicit `--host 0.0.0.0`
    /// opt-in (loopback is firewall-exempt and the safer default).
    #[test]
    fn default_host_is_loopback() {
        let args = Args::parse_from(["inkentry-server"]);
        assert_eq!(
            args.host, "127.0.0.1",
            "server binary default host must be 127.0.0.1 (loopback), not the wildcard"
        );
    }

    // `--llm-key` deliberately carries no clap `env` attribute. With one,
    // INKENTRY_LLM_KEY would populate `args.llm_key`, which `resolve_llm_key`
    // ranks above `--llm-key-file`, silently inverting the documented
    // precedence. Precedence lives in `resolve_llm_key` alone, so this pins the
    // absence against a future tidy-up.
    #[test]
    #[serial_test::serial(llm_key_env)]
    fn the_key_env_var_does_not_populate_the_inline_key_arg() {
        // SAFETY: pinned to the `llm_key_env` serial group, so no other test
        // reads or writes INKENTRY_LLM_KEY concurrently.
        unsafe { std::env::set_var("INKENTRY_LLM_KEY", "sk-from-env") };
        let args = Args::parse_from(["inkentry-server"]);
        unsafe { std::env::remove_var("INKENTRY_LLM_KEY") };

        assert_eq!(
            args.llm_key, None,
            "INKENTRY_LLM_KEY must not reach args.llm_key: it would outrank --llm-key-file"
        );
    }

    #[test]
    #[serial_test::serial(llm_key_env)]
    fn the_key_env_var_does_not_populate_the_key_file_arg_either() {
        // SAFETY: see the sibling test; same serial group.
        unsafe { std::env::set_var("INKENTRY_LLM_KEY", "/etc/passwd") };
        let args = Args::parse_from(["inkentry-server"]);
        unsafe { std::env::remove_var("INKENTRY_LLM_KEY") };

        assert_eq!(args.llm_key_file, None);
    }

    /// `--host 0.0.0.0` still binds all interfaces when explicitly requested
    /// (e.g. the container entrypoint / a shared team server).
    #[test]
    fn explicit_wildcard_host_is_honoured() {
        let args = Args::parse_from(["inkentry-server", "--host", "0.0.0.0"]);
        assert_eq!(args.host, "0.0.0.0");
    }

    #[test]
    fn loopback_hosts_recognised() {
        for h in [
            "127.0.0.1",
            "127.0.0.5",
            "::1",
            "localhost",
            "LocalHost",
            " 127.0.0.1 ",
        ] {
            assert!(super::host_is_loopback(h), "{h} should be loopback");
        }
    }

    #[test]
    fn non_loopback_hosts_recognised() {
        for h in [
            "0.0.0.0",
            "::",
            "192.168.1.10",
            "10.0.0.2",
            "example.com",
            "",
        ] {
            assert!(!super::host_is_loopback(h), "{h} should NOT be loopback");
        }
    }

    /// A native-embedder load failure's `/v1/health` detail must read as a
    /// terminal failure, not in-progress bootstrap text (the underlying
    /// `anyhow::Context` message, e.g. "creating model cache dir ...", reads
    /// like progress on its own).
    #[test]
    #[cfg(feature = "embed-native")]
    fn embedder_load_failure_message_is_prefixed() {
        assert_eq!(
            super::embedder_load_failure_message(
                "creating model cache dir /home/inkentry/.local/share/inkentry/models: \
                 Permission denied (os error 13)"
            ),
            "failed: creating model cache dir /home/inkentry/.local/share/inkentry/models: \
             Permission denied (os error 13)"
        );
    }

    // ── ADR-066 §4: TLS-aware bind-safety table ─────────────────────────────
    //
    // | Bind | TLS | Key | Result |
    // | loopback     | any | any | allow |
    // | non-loopback | no  | any | refuse |
    // | non-loopback | yes | no  | refuse |
    // | non-loopback | yes | yes | allow  |

    /// Row 1: a loopback bind is allowed for every TLS/key combination
    /// (unreachable off-host, so local plaintext with no key is fine).
    #[test]
    fn loopback_is_allowed_for_every_combination() {
        for h in ["127.0.0.1", "::1", "localhost"] {
            for tls in [false, true] {
                for key in [false, true] {
                    assert!(
                        super::check_bind_safety(h, 7777, key, tls).is_ok(),
                        "loopback {h} (tls={tls}, key={key}) should be allowed"
                    );
                }
            }
        }
    }

    /// Row 2: a non-loopback bind with no TLS is refused whether keyed or not —
    /// no plaintext off-host, keyed (key in cleartext) or keyless (open server).
    #[test]
    fn non_loopback_without_tls_is_refused() {
        for h in ["0.0.0.0", "::", "192.168.1.10", "example.com"] {
            for key in [false, true] {
                let err = super::check_bind_safety(h, 7777, key, false)
                    .expect_err(&format!("{h} (tls=false, key={key}) must be refused"));
                let msg = format!("{err}");
                assert!(
                    msg.contains(h) && msg.contains("7777"),
                    "error must name the interface and port '{h}:7777': {msg}"
                );
                assert!(
                    msg.contains("--tls-cert") && msg.contains("--tls-key"),
                    "refusal must offer the --tls-cert/--tls-key remedy, not a proxy: {msg}"
                );
            }
        }
    }

    /// Row 3: a non-loopback TLS bind with no API key is refused — a remote
    /// HTTPS server must authenticate its callers.
    #[test]
    fn non_loopback_tls_without_key_is_refused() {
        for h in ["0.0.0.0", "::", "192.168.1.10", "example.com"] {
            let err = super::check_bind_safety(h, 7777, false, true)
                .expect_err(&format!("{h} (tls=true, key=false) must be refused"));
            let msg = format!("{err}");
            assert!(
                msg.contains(h) && msg.contains("7777"),
                "error must name the interface and port '{h}:7777': {msg}"
            );
            assert!(
                msg.contains("API key"),
                "refusal must say a remote server requires an API key: {msg}"
            );
        }
    }

    /// Row 4: the new remote path — a non-loopback bind with BOTH TLS and a key
    /// is allowed.
    #[test]
    fn non_loopback_tls_with_key_is_allowed() {
        for h in ["0.0.0.0", "::", "192.168.1.10", "example.com"] {
            assert!(
                super::check_bind_safety(h, 7777, true, true).is_ok(),
                "{h} (tls=true, key=true) is the remote HTTPS path and must be allowed"
            );
        }
    }

    // ── ADR-066 §2: --tls-cert / --tls-key are all-or-nothing ────────────────

    /// Both unset → TLS disabled; both set → TLS enabled.
    #[test]
    fn tls_both_or_neither_resolves() {
        assert!(!super::resolve_tls_enabled(false, false).unwrap());
        assert!(super::resolve_tls_enabled(true, true).unwrap());
    }

    /// Exactly one set is a fatal configuration error, and the message names the
    /// missing flag.
    #[test]
    fn tls_exactly_one_set_is_error() {
        let err = super::resolve_tls_enabled(true, false).unwrap_err();
        assert!(
            format!("{err}").contains("--tls-key"),
            "cert-only error must name the missing --tls-key: {err}"
        );
        let err = super::resolve_tls_enabled(false, true).unwrap_err();
        assert!(
            format!("{err}").contains("--tls-cert"),
            "key-only error must name the missing --tls-cert: {err}"
        );
    }

    /// The TLS flags parse as paths and read their env vars.
    #[test]
    fn tls_flags_parse() {
        let args = Args::parse_from([
            "inkentry-server",
            "--tls-cert",
            "/etc/inkentry/tls-cert",
            "--tls-key",
            "/etc/inkentry/tls-key",
        ]);
        assert_eq!(
            args.tls_cert.as_deref(),
            Some(std::path::Path::new("/etc/inkentry/tls-cert"))
        );
        assert_eq!(
            args.tls_key.as_deref(),
            Some(std::path::Path::new("/etc/inkentry/tls-key"))
        );
    }

    /// A blank/whitespace key (incl. clap's `Some("")` for a set-but-empty
    /// `INKENTRY_SERVER_KEY`, e.g. docker-compose's default) normalises to `None`
    /// — otherwise a keyless container would slip past the bind-safety guard.
    #[test]
    fn blank_api_key_normalises_to_none() {
        assert_eq!(super::normalize_api_key(None), None);
        assert_eq!(super::normalize_api_key(Some("")), None);
        assert_eq!(super::normalize_api_key(Some("   ")), None);
        assert_eq!(super::normalize_api_key(Some("\t\n")), None);
    }

    #[test]
    fn real_api_key_is_preserved_and_trimmed() {
        assert_eq!(
            super::normalize_api_key(Some("secret")).as_deref(),
            Some("secret")
        );
        assert_eq!(
            super::normalize_api_key(Some("  secret  ")).as_deref(),
            Some("secret")
        );
    }

    // The BOM is stripped ahead of the trim, so a BOM followed by leading
    // whitespace leaves neither behind.
    #[test]
    fn bom_is_stripped_before_surrounding_whitespace() {
        assert_eq!(
            super::normalize_api_key(Some("\u{feff}secret")).as_deref(),
            Some("secret")
        );
        assert_eq!(
            super::normalize_api_key(Some("\u{feff}  secret  \n")).as_deref(),
            Some("secret")
        );
    }

    // A file holding nothing but a BOM is as empty as a whitespace-only one: it
    // must resolve to None and fall through, not to a one-character key.
    #[test]
    fn bom_only_value_normalises_to_none() {
        assert_eq!(super::normalize_api_key(Some("\u{feff}")), None);
        assert_eq!(super::normalize_api_key(Some("\u{feff}\n")), None);
    }

    // ── ADR-056 single-trust-domain notice ──────────────────────────────────

    /// The notice fires for a keyed shared bind (`0.0.0.0` + key) — the
    /// scenario the ADR calls out: every keyholder is a full administrator of
    /// every project on the instance, and an operator standing up a shared
    /// server must be told that explicitly.
    #[test]
    fn trust_domain_warning_fires_for_non_loopback_with_key() {
        for h in ["0.0.0.0", "::", "192.168.1.10", "example.com"] {
            assert!(
                super::should_warn_single_trust_domain(h, true),
                "{h} with a key should trigger the single-trust-domain notice"
            );
        }
    }

    /// The notice is suppressed on loopback (a developer's own machine, not a
    /// shared deployment) regardless of whether a key is set.
    #[test]
    fn trust_domain_warning_suppressed_on_loopback() {
        for h in ["127.0.0.1", "::1", "localhost"] {
            assert!(
                !super::should_warn_single_trust_domain(h, true),
                "{h} with a key should NOT trigger the notice (loopback)"
            );
            assert!(
                !super::should_warn_single_trust_domain(h, false),
                "{h} without a key should NOT trigger the notice (loopback)"
            );
        }
    }

    /// The notice is suppressed when no key is set. In practice
    /// `check_bind_safety` already refuses a keyless non-loopback bind before
    /// this check runs, but the predicate itself must not fire either way —
    /// there is no "shared key" boundary to warn about without a key.
    #[test]
    fn trust_domain_warning_suppressed_without_key() {
        assert!(!super::should_warn_single_trust_domain("0.0.0.0", false));
    }

    // ── API key resolution (--key / --key-file / env / systemd credential) ──

    use std::io::Write as _;

    /// Write `contents` to a fresh file in `dir` and return its path.
    fn write_key_file(dir: &tempfile::TempDir, name: &str, contents: &str) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    /// `--key-file` reads the whole file, trimmed — the systemd credential /
    /// `0600`-file path is a first-class source, not a shim.
    #[test]
    fn key_file_is_read_and_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_key_file(&dir, "server-key", "  file-secret\n");
        let key = super::resolve_api_key(None, Some(&path), None, None).unwrap();
        assert_eq!(key.as_deref(), Some("file-secret"));
    }

    /// systemd `LoadCredential=server-key` exposes `$CREDENTIALS_DIRECTORY/server-key`;
    /// the binary picks it up automatically with no flag.
    #[test]
    fn systemd_credential_dir_is_read() {
        let dir = tempfile::tempdir().unwrap();
        write_key_file(&dir, "server-key", "cred-secret\n");
        let key = super::resolve_api_key(None, None, None, Some(dir.path())).unwrap();
        assert_eq!(key.as_deref(), Some("cred-secret"));
    }

    /// Precedence: inline `--key` > `--key-file` > `INKENTRY_SERVER_KEY` >
    /// systemd credential. Each non-blank source wins over the ones below it.
    #[test]
    fn key_source_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_key_file(&dir, "kf", "from-file");
        write_key_file(&dir, "server-key", "from-cred");

        // Inline --key beats everything.
        assert_eq!(
            super::resolve_api_key(
                Some("inline"),
                Some(&file),
                Some("from-env"),
                Some(dir.path())
            )
            .unwrap()
            .as_deref(),
            Some("inline")
        );
        // --key-file beats env + credential.
        assert_eq!(
            super::resolve_api_key(None, Some(&file), Some("from-env"), Some(dir.path()))
                .unwrap()
                .as_deref(),
            Some("from-file")
        );
        // env beats the systemd credential.
        assert_eq!(
            super::resolve_api_key(None, None, Some("from-env"), Some(dir.path()))
                .unwrap()
                .as_deref(),
            Some("from-env")
        );
        // credential is the last resort.
        assert_eq!(
            super::resolve_api_key(None, None, None, Some(dir.path()))
                .unwrap()
                .as_deref(),
            Some("from-cred")
        );
    }

    /// A blank source is ignored and resolution falls through to the next one —
    /// e.g. a set-but-empty `INKENTRY_SERVER_KEY` must not mask a real credential.
    #[test]
    fn blank_source_falls_through() {
        let dir = tempfile::tempdir().unwrap();
        write_key_file(&dir, "server-key", "cred-secret");
        let key = super::resolve_api_key(Some("   "), None, Some(""), Some(dir.path())).unwrap();
        assert_eq!(key.as_deref(), Some("cred-secret"));
    }

    /// No source set at all → unauthenticated (loopback dev server).
    #[test]
    fn no_key_source_resolves_to_none() {
        assert_eq!(
            super::resolve_api_key(None, None, None, None).unwrap(),
            None
        );
    }

    /// A credentials dir without our `server-key` file is not an error — systemd
    /// may export other credentials.
    #[test]
    fn credentials_dir_without_server_key_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            super::resolve_api_key(None, None, None, Some(dir.path())).unwrap(),
            None
        );
    }

    /// An explicit `--key-file` that cannot be read is a fatal error — the
    /// operator asked for that file, so a missing/unreadable one must not
    /// silently degrade to no-auth.
    #[test]
    fn missing_key_file_is_fatal() {
        let path = std::path::Path::new("/nonexistent/inkentry/server-key");
        assert!(super::resolve_api_key(None, Some(path), None, None).is_err());
    }

    // ── Byte-order mark / non-transmittable keys ────────────────────────────

    // A key file authored on Windows starts with a UTF-8 BOM. str::trim does not
    // remove it (U+FEFF is a format character, not whitespace), so without an
    // explicit strip the stored hash covers bytes no HTTP client can ever send,
    // and every request 401s, including one presenting the file's exact
    // contents.
    #[tokio::test]
    async fn bom_prefixed_key_file_authenticates_with_the_typed_key() {
        use inkentry_server::auth::{ApiKeyAuth, AuthProvider};

        let dir = tempfile::tempdir().unwrap();
        let path = write_key_file(&dir, "server-key", "\u{feff}file-secret\n");

        let key = super::resolve_api_key(None, Some(&path), None, None).unwrap();
        assert_eq!(key.as_deref(), Some("file-secret"));

        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "Authorization",
            axum::http::HeaderValue::from_static("Bearer file-secret"),
        );
        assert!(
            ApiKeyAuth::new(key).authenticate(&headers).await.is_ok(),
            "the key the operator typed must authenticate against a BOM-prefixed key file"
        );
    }

    // Every source funnels through one normalisation, so all four are stripped by
    // the same code, including inline --key, which can carry a BOM from a paste.
    #[test]
    fn every_key_source_is_bom_stripped() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_key_file(&dir, "kf", "\u{feff}from-file");
        write_key_file(&dir, "server-key", "\u{feff}from-cred");

        assert_eq!(
            super::resolve_api_key(Some("\u{feff}inline"), None, None, None)
                .unwrap()
                .as_deref(),
            Some("inline")
        );
        assert_eq!(
            super::resolve_api_key(None, Some(&file), None, None)
                .unwrap()
                .as_deref(),
            Some("from-file")
        );
        assert_eq!(
            super::resolve_api_key(None, None, Some("\u{feff}from-env"), None)
                .unwrap()
                .as_deref(),
            Some("from-env")
        );
        assert_eq!(
            super::resolve_api_key(None, None, None, Some(dir.path()))
                .unwrap()
                .as_deref(),
            Some("from-cred")
        );
    }

    // A key carrying any byte an Authorization header cannot hold can never match
    // a presented token, so startup is refused instead of leaving a server that
    // 401s every request. The refusal names the source, which is the one thing a
    // 401 cannot tell the operator.
    #[test]
    fn untransmittable_key_refuses_startup_naming_its_source() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_key_file(&dir, "kf", "sécret");
        let cred = tempfile::tempdir().unwrap();
        write_key_file(&cred, "server-key", "sécret");

        let cases: [(anyhow::Error, String); 4] = [
            (
                super::resolve_api_key(Some("sécret"), None, None, None).unwrap_err(),
                "--key".to_string(),
            ),
            (
                super::resolve_api_key(None, Some(&file), None, None).unwrap_err(),
                file.display().to_string(),
            ),
            (
                super::resolve_api_key(None, None, Some("sécret"), None).unwrap_err(),
                "INKENTRY_SERVER_KEY".to_string(),
            ),
            (
                super::resolve_api_key(None, None, None, Some(cred.path())).unwrap_err(),
                cred.path().join("server-key").display().to_string(),
            ),
        ];
        for (err, source) in cases {
            let msg = format!("{err:#}");
            assert!(
                msg.contains(&source),
                "the refusal must name {source}, got: {msg}"
            );
        }
    }

    // A control character is refused for the same reason as a non-ASCII byte: it
    // cannot ride in a header value either.
    #[test]
    fn control_character_key_refuses_startup() {
        assert!(super::resolve_api_key(Some("sec\u{7}ret"), None, None, None).is_err());
    }

    // The guard refuses only what a header cannot carry: any printable-ASCII key
    // resolves untouched, spaces, punctuation and interior tabs included.
    #[test]
    fn printable_ascii_key_is_accepted_unchanged() {
        let all_printable: String = (0x21u8..0x7f).map(char::from).collect();
        for key in [all_printable.as_str(), "two words\tand a tab", "s3cret"] {
            assert_eq!(
                super::resolve_api_key(Some(key), None, None, None)
                    .unwrap()
                    .as_deref(),
                Some(key)
            );
        }
    }

    // ADR-056: the keyless local posture is untouched. No key configured means
    // nothing to validate and nothing to refuse, and a loopback bind still
    // starts.
    #[test]
    fn no_key_configured_still_starts() {
        assert_eq!(
            super::resolve_api_key(None, None, None, None).unwrap(),
            None
        );
        assert!(super::check_bind_safety("127.0.0.1", 7777, false, false).is_ok());
    }

    // A BOM-only file is empty, so resolution falls through to the next source
    // exactly as it does for a whitespace-only one.
    #[test]
    fn bom_only_source_falls_through() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_key_file(&dir, "kf", "\u{feff}");
        write_key_file(&dir, "server-key", "cred-secret");
        let key = super::resolve_api_key(None, Some(&file), None, Some(dir.path())).unwrap();
        assert_eq!(key.as_deref(), Some("cred-secret"));
    }

    /// `--key-file` parses as a path arg.
    #[test]
    fn key_file_flag_parses() {
        let args = Args::parse_from(["inkentry-server", "--key-file", "/etc/inkentry/server-key"]);
        assert_eq!(
            args.key_file.as_deref(),
            Some(std::path::Path::new("/etc/inkentry/server-key"))
        );
    }

    // ── Offline / air-gapped model-dir ───────────────────────────────────────

    /// `--model-dir` parses as a path arg, for the air-gapped load path.
    #[test]
    fn model_dir_flag_parses() {
        let args = Args::parse_from(["inkentry-server", "--model-dir", "/srv/inkentry/models"]);
        assert_eq!(
            args.model_dir.as_deref(),
            Some(std::path::Path::new("/srv/inkentry/models"))
        );
    }

    /// Unset by default: the online Hugging Face Hub path stays the default
    /// (no regression for the common case).
    #[test]
    #[serial_test::serial(model_dir_env)]
    fn model_dir_defaults_to_none() {
        // Serialized against model_dir_env_var_is_honoured: both read/write
        // the real process env var, and cargo test runs in threads within one
        // process, so an unguarded reader can observe another test's
        // temporarily-set value.
        let prev = std::env::var("INKENTRY_MODEL_DIR").ok();
        // SAFETY: guarded by #[serial] so no other test reads/writes this var
        // concurrently.
        unsafe { std::env::remove_var("INKENTRY_MODEL_DIR") };

        let args = Args::parse_from(["inkentry-server"]);

        if let Some(v) = prev {
            unsafe { std::env::set_var("INKENTRY_MODEL_DIR", v) };
        }

        assert_eq!(args.model_dir, None);
    }

    /// `INKENTRY_MODEL_DIR` is a first-class equal source, not just a flag:
    /// the same convention as `INKENTRY_SERVER_TLS_CERT`/`INKENTRY_LLM_URL`, so
    /// a systemd unit or container entrypoint can set it without a flag.
    #[test]
    #[serial_test::serial(model_dir_env)]
    fn model_dir_env_var_is_honoured() {
        let prev = std::env::var("INKENTRY_MODEL_DIR").ok();
        // SAFETY: guarded by #[serial] so no other test reads/writes this var
        // concurrently; restored before returning.
        unsafe { std::env::set_var("INKENTRY_MODEL_DIR", "/srv/inkentry/models") };

        let args = Args::parse_from(["inkentry-server"]);

        match prev {
            Some(v) => unsafe { std::env::set_var("INKENTRY_MODEL_DIR", v) },
            None => unsafe { std::env::remove_var("INKENTRY_MODEL_DIR") },
        }

        assert_eq!(
            args.model_dir.as_deref(),
            Some(std::path::Path::new("/srv/inkentry/models"))
        );
    }

    // ── Removed external-embedding relocation options ────────────────────────
    //
    // The embedding model is pinned product-wide to the bundled native
    // embedder: `--embedding-url` / `INKENTRY_EMBEDDING_URL` (relocate compute)
    // and the legacy `--embedding-model` / `INKENTRY_EMBEDDING_MODEL` (select a
    // model) must no longer exist as parseable flags at all.

    /// `--embedding-url` is unknown to clap, not silently accepted.
    #[test]
    fn embedding_url_flag_is_unknown() {
        let err = Args::try_parse_from(["inkentry-server", "--embedding-url", "http://x:1234"])
            .expect_err("--embedding-url must no longer parse");
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    /// `--embedding-model` is unknown to clap, not silently accepted.
    #[test]
    fn embedding_model_flag_is_unknown() {
        let err = Args::try_parse_from(["inkentry-server", "--embedding-model", "some-model"])
            .expect_err("--embedding-model must no longer parse");
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    /// `--help` must not advertise either removed flag.
    #[test]
    fn help_omits_removed_embedding_flags() {
        use clap::CommandFactory as _;
        let help = Args::command().render_long_help().to_string();
        assert!(
            !help.contains("embedding-url"),
            "help must not mention --embedding-url: {help}"
        );
        assert!(
            !help.contains("embedding-model"),
            "help must not mention --embedding-model: {help}"
        );
    }

    /// `INKENTRY_EMBEDDING_URL` / `INKENTRY_EMBEDDING_MODEL` in the environment
    /// are plain unread variables now (no `env = "..."` attribute maps them to
    /// any field): parsing must succeed and must not be influenced by them.
    #[test]
    #[serial_test::serial]
    fn embedding_env_vars_are_inert() {
        // SAFETY: test-only, guarded by #[serial_test::serial] against
        // concurrent env mutation from other tests.
        unsafe {
            std::env::set_var("INKENTRY_EMBEDDING_URL", "http://127.0.0.1:1234");
            std::env::set_var("INKENTRY_EMBEDDING_MODEL", "some-model");
        }
        let args = Args::parse_from(["inkentry-server"]);
        assert_eq!(args.host, "127.0.0.1", "parsing must succeed unaffected");
        unsafe {
            std::env::remove_var("INKENTRY_EMBEDDING_URL");
            std::env::remove_var("INKENTRY_EMBEDDING_MODEL");
        }
    }

    // ── Self-contained health probe ──────────────────────────────────────────

    /// Wildcard binds are probed over loopback (a wildcard is not itself
    /// connectable); a concrete host is probed as-is. IPv6 literals are
    /// bracketed in the URL.
    #[test]
    fn health_probe_url_maps_wildcard_and_brackets_ipv6() {
        assert_eq!(
            super::health_probe_url("127.0.0.1", 7777),
            "http://127.0.0.1:7777/v1/health"
        );
        assert_eq!(
            super::health_probe_url("0.0.0.0", 7777),
            "http://127.0.0.1:7777/v1/health"
        );
        assert_eq!(
            super::health_probe_url("", 7777),
            "http://127.0.0.1:7777/v1/health"
        );
        assert_eq!(
            super::health_probe_url("::", 9000),
            "http://[::1]:9000/v1/health"
        );
        assert_eq!(
            super::health_probe_url("::1", 9000),
            "http://[::1]:9000/v1/health"
        );
        assert_eq!(
            super::health_probe_url("example.com", 8080),
            "http://example.com:8080/v1/health"
        );
    }

    /// The probe returns `Ok` when `/v1/health` answers `2xx`.
    #[tokio::test]
    async fn health_probe_ok_on_2xx() {
        use axum::{Router, routing::get};
        let app = Router::new().route("/v1/health", get(|| async { "ok" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        assert!(super::run_health_check("127.0.0.1", port).await.is_ok());
    }

    /// A non-`2xx` response is a failed probe (non-zero exit).
    #[tokio::test]
    async fn health_probe_err_on_5xx() {
        use axum::{Router, http::StatusCode, routing::get};
        let app = Router::new().route(
            "/v1/health",
            get(|| async { StatusCode::SERVICE_UNAVAILABLE }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        assert!(super::run_health_check("127.0.0.1", port).await.is_err());
    }

    /// No listener → the probe fails (connection refused), not hangs.
    #[tokio::test]
    async fn health_probe_err_when_unreachable() {
        // Bind then drop to claim a definitely-free port.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert!(super::run_health_check("127.0.0.1", port).await.is_err());
    }
}

// ── Embed CPU thread budget ──────────────────────────────────────────────────

#[cfg(test)]
mod thread_budget_tests {
    use super::embed_thread_budget;

    /// No overrides: reserve 2 cores for the async runtime + OS.
    #[test]
    fn default_reserves_two_cores() {
        assert_eq!(embed_thread_budget(10, None, None), 8);
        assert_eq!(embed_thread_budget(4, None, None), 2);
    }

    /// Tiny hosts must never yield 0 threads.
    #[test]
    fn tiny_hosts_clamp_to_one() {
        assert_eq!(embed_thread_budget(1, None, None), 1);
        assert_eq!(embed_thread_budget(2, None, None), 1);
        assert_eq!(embed_thread_budget(3, None, None), 1);
    }

    /// `INKENTRY_EMBED_THREADS` wins over both the default and a set
    /// `RAYON_NUM_THREADS`.
    #[test]
    fn inkentry_override_wins() {
        assert_eq!(embed_thread_budget(10, None, Some(3)), 3);
        assert_eq!(embed_thread_budget(10, Some(6), Some(3)), 3);
    }

    /// A user-set `RAYON_NUM_THREADS` is respected when there is no inkentry
    /// override — don't override CI / power users.
    #[test]
    fn rayon_override_respected_without_inkentry() {
        assert_eq!(embed_thread_budget(10, Some(4), None), 4);
    }

    /// Zero (and, upstream, unparseable) overrides are ignored and fall through
    /// to the next source.
    #[test]
    fn zero_overrides_fall_through() {
        assert_eq!(embed_thread_budget(10, Some(0), Some(0)), 8);
        assert_eq!(embed_thread_budget(10, Some(0), None), 8);
        assert_eq!(embed_thread_budget(10, Some(4), Some(0)), 4);
    }
}
