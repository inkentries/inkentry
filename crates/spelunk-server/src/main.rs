use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use clap::Parser;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use spelunk_server::auth::ApiKeyAuth;
use spelunk_server::db::ServerDb;
use spelunk_server::rate_limiter::RateLimiter;
use spelunk_server::{ApiDoc, AppState, EmbedderSlot, default_conflict_threshold, router};
use utoipa::OpenApi;

#[cfg(feature = "embed-native")]
use spelunk_embed::DIM as NATIVE_EMBED_DIM;
// Via spelunk-core (always linked); spelunk_embed is only present under embed-native.
use spelunk_core::embeddings::MODEL_ID as NATIVE_MODEL_ID;
#[cfg(feature = "embed-native")]
use spelunk_server::embed_hub;

#[derive(Parser, Debug)]
#[command(
    name = "spelunk-server",
    version,
    about = "Shared memory server for spelunk",
    before_help = concat!("spelunk-server v", env!("CARGO_PKG_VERSION"))
)]
struct Args {
    /// Port to listen on
    #[arg(long, default_value = "7777")]
    port: u16,

    /// Host/address to bind. Defaults to loopback (`127.0.0.1`) — the safe,
    /// firewall-exempt posture for a local server. Pass `--host 0.0.0.0`
    /// explicitly to expose the server on all interfaces (e.g. a shared/team
    /// server or the container image, which sets this in its entrypoint).
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// Path to the server SQLite database
    #[arg(long, default_value = "spelunk.db")]
    db: PathBuf,

    /// Shared API key (Bearer token) passed inline. Visible in the process
    /// table and `systemctl show`, so prefer --key-file or SPELUNK_SERVER_KEY
    /// for real deployments. Leave all key sources unset to disable auth
    /// (loopback dev only). Overrides every other key source.
    #[arg(long)]
    key: Option<String>,

    /// Read the shared API key from a file (its whole trimmed contents). A
    /// first-class alternative to SPELUNK_SERVER_KEY, not a fallback: point it
    /// at a `0600` file, or at `$CREDENTIALS_DIRECTORY/server-key` when run
    /// under systemd `LoadCredential=`. When run under systemd, a
    /// `server-key` credential is picked up automatically even without this
    /// flag. Read failure is fatal.
    #[arg(long, value_name = "PATH")]
    key_file: Option<PathBuf>,

    /// Embedding dimension expected from clients (must match the team's model).
    /// Default: 896 (F2LLM-v2-330M).
    #[arg(long, default_value = "896")]
    embedding_dim: usize,

    /// Cosine similarity threshold for conflict detection (0.0–1.0). New entries with
    /// similarity ≥ this value to an existing active entry trigger a 409 response.
    /// Set to 1.0 to disable conflict detection.
    #[arg(long, default_value_t = default_conflict_threshold())]
    conflict_threshold: f32,

    /// Base URL of an OpenAI-compatible embedding server for server-side embedding
    /// (e.g. `http://127.0.0.1:1234`). Overrides `SPELUNK_EMBEDDING_URL`.
    /// When set, entries posted without a pre-computed `embedding` field are embedded
    /// by the server before storage.
    #[arg(long, env = "SPELUNK_EMBEDDING_URL")]
    embedding_url: Option<String>,

    /// Embedding model name to pass to the external `--embedding-url` server (e.g.
    /// `f2llm-v2-330m`). Ignored by the bundled native embedder. Overrides
    /// `SPELUNK_EMBEDDING_MODEL`.
    #[arg(long, env = "SPELUNK_EMBEDDING_MODEL", default_value = "")]
    embedding_model: String,

    /// Base URL of an OpenAI-compatible chat completions server for LLM features
    /// (`/explore`). Overrides `SPELUNK_LLM_URL`.
    #[arg(long, env = "SPELUNK_LLM_URL")]
    llm_url: Option<String>,

    /// LLM model name (e.g. `google/gemma-3n-e4b`). Overrides `SPELUNK_LLM_MODEL`.
    #[arg(long, env = "SPELUNK_LLM_MODEL", default_value = "")]
    llm_model: String,

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

#[tokio::main]
async fn main() -> Result<()> {
    // Register sqlite-vec extension for every connection in this process.
    #[allow(clippy::missing_transmute_annotations)]
    unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    }

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(fmt::layer())
        .init();

    let args = Args::parse();

    if args.print_openapi {
        println!("{}", ApiDoc::openapi().to_pretty_json()?);
        return Ok(());
    }

    if args.health_check {
        return run_health_check(&args.host, args.port).await;
    }

    // Resolve the API key from --key / --key-file / SPELUNK_SERVER_KEY /
    // systemd LoadCredential (see resolve_api_key for precedence). A blank
    // value from any source counts as "no key" — a set-but-empty
    // `SPELUNK_SERVER_KEY` (docker-compose's `${SPELUNK_SERVER_KEY:-}` default)
    // must read as unauthenticated, not as a broken empty-token key.
    let env_key = std::env::var("SPELUNK_SERVER_KEY").ok();
    let credentials_dir = std::env::var_os("CREDENTIALS_DIRECTORY").map(PathBuf::from);
    let api_key = resolve_api_key(
        args.key.as_deref(),
        args.key_file.as_deref(),
        env_key.as_deref(),
        credentials_dir.as_deref(),
    )?;

    // Bind-safety: refuse to expose the server off-host over plaintext HTTP,
    // whether that would leak an open (keyless) endpoint or the bearer key in
    // cleartext (keyed). Fail fast, before touching the DB or warming the
    // embedder. This refusal is unconditional — there is no opt-out.
    check_bind_safety(&args.host, args.port, api_key.is_some())?;

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
             Set --key-file, SPELUNK_SERVER_KEY, or --key for production use."
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
    let auth: Arc<dyn spelunk_server::auth::AuthProvider> =
        Arc::new(ApiKeyAuth::new(api_key.clone()));

    // Build the server-side embedder readiness slot.
    //
    // The external `--embedding-url` backend is ready synchronously (it has no
    // local model to warm up), so it starts `ready`. The bundled native embedder
    // is CPU-/download-heavy, so we start the slot `loading` and defer the actual
    // `embed_hub::load_from_hub()` to a background task spawned *after* the listener
    // binds (below) — that way `/v1/health` is live immediately with
    // `embedder.state = "loading"` instead of being dark for the whole first-run
    // model download. When no embedder is configured at all, the slot is
    // `disabled` (embed endpoints return a permanent 400).
    let (embedder, load_native): (EmbedderSlot, bool) = if let Some(base_url) = args.embedding_url {
        let model = if args.embedding_model.is_empty() {
            "default".to_string()
        } else {
            args.embedding_model.clone()
        };
        tracing::info!("server-side embedding enabled: {base_url} model={model}");
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .context("building HTTP client for server-side embedder")?;
        let backend: Arc<dyn spelunk_core::embeddings::EmbeddingBackend> =
            Arc::new(ServerEmbedder {
                client,
                base_url,
                model,
            });
        (EmbedderSlot::ready(backend), false)
    } else {
        // No --embedding-url: try the bundled native embedder (embed-native feature).
        #[cfg(feature = "embed-native")]
        {
            (EmbedderSlot::loading(), true)
        }
        #[cfg(not(feature = "embed-native"))]
        {
            (EmbedderSlot::disabled(), false)
        }
    };

    // Build the optional LLM backend.
    let llm: Option<Arc<dyn spelunk_core::llm::LlmBackend>> = if let Some(base_url) = args.llm_url {
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
        }))
    } else {
        None
    };

    // Server-side max_tokens ceiling: env var or 8192 default.
    let max_tokens_ceiling: usize = std::env::var("SPELUNK_MAX_TOKENS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8192);

    // Per-principal rate limiter: 60 requests per minute by default.
    let rate_limiter = Arc::new(RateLimiter::new(60, 60));

    let state = AppState {
        db: Arc::new(tokio::sync::Mutex::new(db)),
        auth,
        conflict_threshold: args.conflict_threshold,
        embedder,
        llm,
        max_tokens_ceiling,
        rate_limiter,
        instance_id,
        started_by,
    };

    // Keep a handle to the embedder slot so the background load task can flip it
    // `loading → ready | unavailable` after the listener binds.
    let embedder_slot = state.embedder.clone();

    let app = router(state);
    let addr: SocketAddr = format!("{}:{}", args.host, args.port)
        .parse()
        .context("parsing bind address")?;

    // Bind first: `/v1/health` must be reachable the instant the port is bound,
    // *before* the (potentially multi-minute, ~339 MB) native model download.
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("spelunk-server listening on http://{addr}");

    // Load the native embedder on a background task now that health is live.
    // `embed_hub::load_from_hub()` is blocking/CPU-heavy, so run it on the blocking
    // pool; publish the backend into the slot on success (state → ready) or
    // record the failure (state → unavailable). Only the native path warms up
    // here — external/disabled slots are already in a terminal state.
    #[cfg(feature = "embed-native")]
    if load_native {
        let slot = embedder_slot.clone();
        tokio::spawn(async move {
            match tokio::task::spawn_blocking(embed_hub::load_from_hub).await {
                Ok(Ok(native)) => {
                    tracing::info!("native embedding model loaded (dim={})", NATIVE_EMBED_DIM);
                    slot.set_ready(
                        Arc::new(native) as Arc<dyn spelunk_core::embeddings::EmbeddingBackend>
                    );
                }
                Ok(Err(e)) => {
                    let msg = format!("{e}");
                    tracing::warn!(
                        "native embedding model failed to load: {msg}; \
                         embedder unavailable (set --embedding-url to override)"
                    );
                    slot.set_unavailable(msg);
                }
                Err(join_err) => {
                    let msg = format!("embedder load task panicked: {join_err}");
                    tracing::warn!("{msg}");
                    slot.set_unavailable(msg);
                }
            }
        });
    }
    // Silence "unused" for the non-embed-native build (no background load).
    #[cfg(not(feature = "embed-native"))]
    let _ = (load_native, &embedder_slot);

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

    Ok(())
}

// ── Bind-safety guard ─────────────────────────────────────────────────────────

/// Returns `true` when `host` names the loopback interface only — `127.0.0.0/8`,
/// `::1`, or the literal `localhost`. A loopback bind is not reachable from other
/// machines, so it is safe to serve without authentication. Anything else
/// (`0.0.0.0`, `::`, a LAN/public IP, an unresolved hostname) is treated as
/// off-host and is *not* loopback.
fn host_is_loopback(host: &str) -> bool {
    let h = host.trim();
    if h.eq_ignore_ascii_case("localhost") {
        return true;
    }
    h.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

/// Normalise a configured API key: a blank/whitespace value (e.g. a
/// set-but-empty `SPELUNK_SERVER_KEY`, or an empty credential file) becomes
/// `None`, so "empty key" is treated as "no key" everywhere — both by the
/// bind-safety guard and by the auth provider.
fn normalize_api_key(key: Option<&str>) -> Option<String> {
    key.map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_owned)
}

/// Filename systemd exposes for `LoadCredential=server-key:...` under
/// `$CREDENTIALS_DIRECTORY`.
const SERVER_KEY_CREDENTIAL: &str = "server-key";

/// Resolve the shared API key from all supported sources, in precedence order
/// (a blank value at any level is ignored and falls through to the next):
///
/// 1. `--key <value>` — inline flag (most explicit).
/// 2. `--key-file <path>` — explicit file; a read failure is fatal.
/// 3. `SPELUNK_SERVER_KEY` — environment variable.
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
        return Ok(Some(k));
    }
    if let Some(path) = key_file {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading --key-file {}", path.display()))?;
        if let Some(k) = normalize_api_key(Some(&raw)) {
            return Ok(Some(k));
        }
    }
    if let Some(k) = normalize_api_key(env_key) {
        return Ok(Some(k));
    }
    if let Some(dir) = credentials_dir {
        let path = dir.join(SERVER_KEY_CREDENTIAL);
        match std::fs::read_to_string(&path) {
            Ok(raw) => {
                if let Some(k) = normalize_api_key(Some(&raw)) {
                    return Ok(Some(k));
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

/// Refuse to expose the server off-host over plaintext HTTP. Binding to any
/// non-loopback address makes the endpoint reachable from other machines, and
/// the server only ever speaks plaintext HTTP. Two cases are refused,
/// unconditionally:
///
/// - **keyless** non-loopback bind: an open, unauthenticated server; and
/// - **keyed** non-loopback bind: the bearer `SPELUNK_SERVER_KEY` would cross
///   the network in cleartext.
///
/// This matches the ADR-056 transport guardrail: plaintext HTTP is
/// loopback-only. Loopback binds (the default) are always allowed. There is
/// no opt-out. The refusal names the interface and port being refused.
fn check_bind_safety(host: &str, port: u16, key_is_set: bool) -> Result<()> {
    if host_is_loopback(host) {
        return Ok(());
    }

    if !key_is_set {
        // Keyless off-host bind: an open, unauthenticated server.
        anyhow::bail!(
            "Refusing to bind to non-loopback address '{host}:{port}' without \
             authentication.\n\
             A server reachable from other machines must require an API key. Either:\n  \
             • set --key / SPELUNK_SERVER_KEY to expose it on {host}:{port}, or\n  \
             • bind to loopback (the default --host 127.0.0.1) for local-only use."
        );
    }

    // Keyed off-host bind over plaintext HTTP: the bearer key would cross the
    // network in cleartext. Refused unconditionally — there is no opt-out.
    anyhow::bail!(
        "Refusing to bind to non-loopback address '{host}:{port}' over plaintext HTTP: \
         the bearer SPELUNK_SERVER_KEY would cross the network in cleartext.\n\
         A shared server must not send its key in the clear. Bind to loopback \
         (the default --host 127.0.0.1) instead.\n\
         See docs/adr/056-oss-server-tenancy-model.md."
    );
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

// ── Inline embedder for the server binary ─────────────────────────────────────

struct ServerEmbedder {
    client: reqwest::Client,
    base_url: String,
    model: String,
}

#[async_trait::async_trait]
impl spelunk_core::embeddings::EmbeddingBackend for ServerEmbedder {
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        #[derive(serde::Serialize)]
        struct Req<'a> {
            model: &'a str,
            input: &'a [&'a str],
        }
        #[derive(serde::Deserialize)]
        struct Resp {
            data: Vec<Data>,
        }
        #[derive(serde::Deserialize)]
        struct Data {
            embedding: Vec<f32>,
        }

        let resp: Resp = self
            .client
            .post(format!("{}/v1/embeddings", self.base_url))
            .json(&Req {
                model: &self.model,
                input: texts,
            })
            .send()
            .await
            .context("calling embedding server")?
            .error_for_status()
            .context("embedding server returned an error")?
            .json()
            .await
            .context("parsing embedding response")?;

        anyhow::ensure!(!resp.data.is_empty(), "embedding server returned 0 vectors");
        Ok(resp.data.into_iter().map(|d| d.embedding).collect())
    }

    fn dimension(&self) -> usize {
        0 // dimension is model-dependent; not used server-side
    }
}

// ── Inline LLM for the server binary ─────────────────────────────────────────

struct ServerLlm {
    client: reqwest::Client,
    base_url: String,
    model: String,
}

#[async_trait::async_trait]
impl spelunk_core::llm::LlmBackend for ServerLlm {
    async fn generate(
        &self,
        messages: &[spelunk_core::llm::Message],
        max_tokens: usize,
        tx: tokio::sync::mpsc::Sender<spelunk_core::llm::Token>,
        json_schema: Option<serde_json::Value>,
    ) -> anyhow::Result<()> {
        use futures_util::StreamExt;

        #[derive(serde::Serialize)]
        struct ChatReq<'a> {
            model: &'a str,
            messages: Vec<ChatMsg<'a>>,
            stream: bool,
            max_tokens: usize,
            temperature: f32,
            #[serde(skip_serializing_if = "Option::is_none")]
            response_format: Option<serde_json::Value>,
        }
        #[derive(serde::Serialize)]
        struct ChatMsg<'a> {
            role: &'a str,
            content: &'a str,
        }
        #[derive(serde::Deserialize)]
        struct StreamChunk {
            choices: Vec<StreamChoice>,
        }
        #[derive(serde::Deserialize)]
        struct StreamChoice {
            delta: Delta,
        }
        #[derive(serde::Deserialize)]
        struct Delta {
            content: Option<String>,
        }

        let chat_messages: Vec<ChatMsg> = messages
            .iter()
            .map(|m| ChatMsg {
                role: &m.role,
                content: &m.content,
            })
            .collect();

        let response_format =
            json_schema.map(|s| serde_json::json!({ "type": "json_schema", "json_schema": s }));

        let req = ChatReq {
            model: &self.model,
            messages: chat_messages,
            stream: true,
            max_tokens,
            temperature: 0.7,
            response_format,
        };

        let mut stream = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .json(&req)
            .send()
            .await
            .context("calling LLM server")?
            .error_for_status()
            .context("LLM server returned an error")?
            .bytes_stream();

        let mut buffer = String::new();
        while let Some(chunk) = stream.next().await {
            let bytes = chunk.context("reading SSE byte chunk")?;
            buffer.push_str(&String::from_utf8_lossy(&bytes));

            while let Some(pos) = buffer.find("\n\n") {
                let event = buffer[..pos].to_string();
                buffer.drain(..pos + 2);

                for line in event.lines() {
                    let data = match line.strip_prefix("data: ") {
                        Some(d) => d,
                        None => continue,
                    };
                    if data == "[DONE]" {
                        return Ok(());
                    }
                    if data.is_empty() {
                        continue;
                    }
                    if let Ok(chunk) = serde_json::from_str::<StreamChunk>(data) {
                        for choice in chunk.choices {
                            if let Some(content) = choice.delta.content
                                && !content.is_empty()
                                && tx.send(content).await.is_err()
                            {
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

// ── Args default tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod arg_tests {
    use super::Args;
    use clap::Parser;

    /// The server binary default host must be loopback (127.0.0.1), not the
    /// wildcard (0.0.0.0). The wildcard bind is now an explicit `--host 0.0.0.0`
    /// opt-in (loopback is firewall-exempt and the safer default). oss^50 / req #6.
    #[test]
    fn default_host_is_loopback() {
        let args = Args::parse_from(["spelunk-server"]);
        assert_eq!(
            args.host, "127.0.0.1",
            "server binary default host must be 127.0.0.1 (loopback), not the wildcard"
        );
    }

    /// `--host 0.0.0.0` still binds all interfaces when explicitly requested
    /// (e.g. the container entrypoint / a shared team server).
    #[test]
    fn explicit_wildcard_host_is_honoured() {
        let args = Args::parse_from(["spelunk-server", "--host", "0.0.0.0"]);
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

    /// Loopback binds never require a key (local-only, unreachable off-host).
    #[test]
    fn loopback_without_key_is_allowed() {
        for h in ["127.0.0.1", "::1", "localhost"] {
            assert!(
                super::check_bind_safety(h, 7777, false).is_ok(),
                "{h} without a key should be allowed"
            );
            assert!(
                super::check_bind_safety(h, 7777, true).is_ok(),
                "{h} with a key should be allowed on loopback"
            );
        }
    }

    /// A non-loopback bind without a key is refused — no open, unauthenticated
    /// server reachable from other machines.
    #[test]
    fn non_loopback_without_key_is_refused() {
        for h in ["0.0.0.0", "::", "192.168.1.10"] {
            assert!(
                super::check_bind_safety(h, 7777, false).is_err(),
                "{h} without a key must be refused"
            );
        }
    }

    /// A keyed non-loopback *plaintext* bind is refused unconditionally: the
    /// bearer key would cross the network in cleartext (ADR-056 transport
    /// guardrail). There is no opt-out. The error names the interface/port
    /// and points at the ADR-056 guidance doc.
    #[test]
    fn non_loopback_with_key_plaintext_is_refused_unconditionally() {
        for h in ["0.0.0.0", "::", "192.168.1.10", "example.com"] {
            let err = super::check_bind_safety(h, 7777, true)
                .expect_err(&format!("{h} with a key over plaintext must be refused"));
            let msg = format!("{err}");
            assert!(
                msg.contains(h) && msg.contains("7777"),
                "error must name the interface and port '{h}:7777': {msg}"
            );
            assert!(
                msg.contains("cleartext"),
                "error must mention cleartext exposure: {msg}"
            );
            assert!(
                msg.contains("loopback"),
                "error must point at binding to loopback instead: {msg}"
            );
            assert!(
                msg.contains("docs/adr/056-oss-server-tenancy-model.md"),
                "error must point at the ADR-056 guidance doc so the operator \
                 can find the loopback-only policy: {msg}"
            );
        }
    }

    /// A blank/whitespace key (incl. clap's `Some("")` for a set-but-empty
    /// `SPELUNK_SERVER_KEY`, e.g. docker-compose's default) normalises to `None`
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

    /// Precedence: inline `--key` > `--key-file` > `SPELUNK_SERVER_KEY` >
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
    /// e.g. a set-but-empty `SPELUNK_SERVER_KEY` must not mask a real credential.
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
        let path = std::path::Path::new("/nonexistent/spelunk/server-key");
        assert!(super::resolve_api_key(None, Some(path), None, None).is_err());
    }

    /// `--key-file` parses as a path arg.
    #[test]
    fn key_file_flag_parses() {
        let args = Args::parse_from(["spelunk-server", "--key-file", "/etc/spelunk/server-key"]);
        assert_eq!(
            args.key_file.as_deref(),
            Some(std::path::Path::new("/etc/spelunk/server-key"))
        );
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
