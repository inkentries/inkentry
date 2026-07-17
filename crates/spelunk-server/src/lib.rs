pub mod auth;
pub mod db;
#[cfg(feature = "embed-native")]
pub mod embed_hub;
pub mod handlers;
pub mod rate_limiter;
pub mod security;

use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::{
    Json, Router,
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::Serialize;
use tower::limit::ConcurrencyLimitLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use utoipa::{OpenApi, ToSchema};

use auth::{AuthError, AuthProvider};
use db::ServerDb;
use rate_limiter::RateLimiter;

/// Wall-clock budget for a single request before the server aborts it with
/// `408`. `/memory/stream` (SSE) and `/index/embed` (see
/// [`EMBED_REQUEST_TIMEOUT`]) are exempt.
///
/// Does NOT bound `/explore` or `/llm/complete`: both return their SSE
/// `Response` immediately and run generation in a detached `tokio::spawn`, which
/// this layer can't see. Those handlers bound the spawned generation directly
/// with this same constant (see `handlers::llm_generate_with_timeout`).
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Wall-clock budget for a single `/index/embed` request before `408`. Much
/// larger than [`REQUEST_TIMEOUT`] and matched to the CLI's `MAX_REQUEST_TIMEOUT`:
/// a legitimate embed batch can run for minutes on slow/CPU-only hardware, which
/// a 30s budget would kill.
///
/// Not a DoS gap: `/index/embed` is still auth-gated and behind
/// `ConcurrencyLimitLayer` + `RequestBodyLimitLayer`, so the count and size of
/// concurrent embed requests stay bounded.
pub(crate) const EMBED_REQUEST_TIMEOUT: Duration = Duration::from_secs(1800);

/// Cap on the JSON request body size. Generous enough for the largest legitimate
/// payload (a 256-chunk `index/embed` batch); per-field caps (title/body) are
/// enforced in the handlers on top of this.
const DEFAULT_BODY_LIMIT_BYTES: usize = 2 * 1024 * 1024; // 2 MiB

/// Global cap on requests processed concurrently across the whole server,
/// including the `/memory/stream` SSE poll loop.
const GLOBAL_CONCURRENCY_LIMIT: usize = 256;

/// Readiness state of the server-side embedder.
///
/// The native embedder loads on a background task after the listener binds, so
/// `/v1/health` is live while the model warms up. Single source of truth for
/// that readiness; the health body carries it and the embed endpoints branch on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum EmbedderState {
    /// Native embedder build/download in progress. Not ready — embed endpoints
    /// return `503 + Retry-After` and the CLI should keep polling.
    Loading,
    /// Model loaded (or an external embedding backend is configured); embed
    /// endpoints will serve.
    Ready,
    /// The background load failed (download error, OOM, …). Terminal for this
    /// process; embed endpoints return `503` and the CLI should stop polling.
    Unavailable,
    /// No in-process model to load — the server was built without an embedder
    /// and has no external `--embedding-url`. Embed endpoints return `400`
    /// (permanent misconfiguration for that request).
    Disabled,
}

/// Inner, lock-guarded contents of the embedder slot.
struct EmbedderSlotInner {
    state: EmbedderState,
    /// The concrete backend once it is ready. `None` while `loading`,
    /// `unavailable`, or `disabled`.
    backend: Option<Arc<dyn spelunk_core::embeddings::EmbeddingBackend>>,
    /// Optional human-readable detail (e.g. the load error) surfaced in the
    /// health body and warm-up responses.
    detail: Option<String>,
}

/// Shared, mutable readiness cell for the server-side embedder.
///
/// Health/handlers read the current state without blocking; a background load
/// task flips it `loading → ready | unavailable`. Reads only ever hold the lock
/// long enough to copy the state and clone an `Arc`, so there is no contention
/// with the (single) background writer.
#[derive(Clone)]
pub struct EmbedderSlot(Arc<RwLock<EmbedderSlotInner>>);

impl EmbedderSlot {
    /// A slot that starts in `loading` — the native embedder is being built on
    /// a background task and will publish itself via [`EmbedderSlot::set_ready`].
    pub fn loading() -> Self {
        Self(Arc::new(RwLock::new(EmbedderSlotInner {
            state: EmbedderState::Loading,
            backend: None,
            detail: Some("loading embedding model".to_string()),
        })))
    }

    /// A slot that is immediately ready with the given backend (e.g. an external
    /// `--embedding-url` that has no local model to warm up).
    pub fn ready(backend: Arc<dyn spelunk_core::embeddings::EmbeddingBackend>) -> Self {
        Self(Arc::new(RwLock::new(EmbedderSlotInner {
            state: EmbedderState::Ready,
            backend: Some(backend),
            detail: None,
        })))
    }

    /// A slot with no embedder at all (no feature / no external URL). Embed
    /// endpoints treat this as a permanent `400` misconfiguration.
    pub fn disabled() -> Self {
        Self(Arc::new(RwLock::new(EmbedderSlotInner {
            state: EmbedderState::Disabled,
            backend: None,
            detail: None,
        })))
    }

    /// Publish a successfully-loaded backend: state → `ready`.
    pub fn set_ready(&self, backend: Arc<dyn spelunk_core::embeddings::EmbeddingBackend>) {
        let mut inner = self.0.write().expect("embedder slot poisoned");
        inner.state = EmbedderState::Ready;
        inner.backend = Some(backend);
        inner.detail = None;
    }

    /// Mark the background load as failed: state → `unavailable` (terminal).
    pub fn set_unavailable(&self, detail: impl Into<String>) {
        let mut inner = self.0.write().expect("embedder slot poisoned");
        inner.state = EmbedderState::Unavailable;
        inner.backend = None;
        inner.detail = Some(detail.into());
    }

    /// Current readiness state (cheap, non-blocking copy).
    pub fn state(&self) -> EmbedderState {
        self.0.read().expect("embedder slot poisoned").state
    }

    /// Optional human-readable detail for the current state.
    pub fn detail(&self) -> Option<String> {
        self.0
            .read()
            .expect("embedder slot poisoned")
            .detail
            .clone()
    }

    /// The backend, cloned, if and only if the slot is `ready`.
    pub fn backend(&self) -> Option<Arc<dyn spelunk_core::embeddings::EmbeddingBackend>> {
        self.0
            .read()
            .expect("embedder slot poisoned")
            .backend
            .clone()
    }
}

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<tokio::sync::Mutex<ServerDb>>,
    /// Auth strategy — replaces the old `api_key: Option<String>` field.
    pub auth: Arc<dyn AuthProvider>,
    /// Cosine similarity threshold above which a new entry is flagged as conflicting (0.0–1.0).
    /// Default: 0.92. Set to 1.0 to disable conflict detection.
    pub conflict_threshold: f32,
    /// Server-side embedder readiness cell. The native embedder loads on a
    /// background task after the listener binds, flipping this slot
    /// `loading → ready | unavailable`; an external `--embedding-url` starts
    /// `ready`; no embedder at all starts `disabled`. Handlers read the current
    /// state without blocking. See [`EmbedderSlot`].
    pub embedder: EmbedderSlot,
    /// Optional LLM backend for `/explore` and `/llm/complete`.
    pub llm: Option<Arc<dyn spelunk_core::llm::LlmBackend>>,
    /// Server-side hard ceiling for `max_tokens` on `/llm/complete`.
    /// Client-supplied values are clamped down to this. Default: 8192.
    pub max_tokens_ceiling: usize,
    /// Per-principal rate limiter for `/llm/complete`.
    pub rate_limiter: Arc<RateLimiter>,
    /// Persistent UUID v7 identifying this server instance across restarts.
    /// CLI warns on instance_id change mid-session.
    pub instance_id: String,
    /// Effective UID of the process that started the server (Unix); `None` on Windows.
    /// CLI warns when this differs from the connecting user's UID (multi-user host).
    pub started_by: Option<u32>,
}

pub fn default_conflict_threshold() -> f32 {
    0.92
}

// ── OpenAPI spec ──────────────────────────────────────────────────────────────

#[derive(OpenApi)]
#[openapi(
    info(
        title = "spelunk-server",
        version = "0.1.0",
        description = "Shared memory server for spelunk. Stores decisions, requirements, \
                        and context for a team and serves them over HTTP.",
        contact(name = "spelunk", url = "https://github.com/spelunk-cloud/spelunk"),
        license(name = "MIT"),
    ),
    paths(
        handlers::health,
        handlers::list_projects,
        handlers::add_note,
        handlers::list_notes,
        handlers::get_note,
        handlers::search_notes,
        handlers::delete_note,
        handlers::archive_note,
        handlers::supersede_note,
        handlers::project_stats,
        handlers::harvested_shas,
        handlers::push_memory_batch,
        handlers::memory_since,
        handlers::memory_stream,
        handlers::index_embed,
        handlers::project_search,
        handlers::explore,
        handlers::llm_complete,
    ),
    components(schemas(
        handlers::AddNoteRequest,
        handlers::AddNoteResponse,
        handlers::ConflictEntry,
        handlers::ListQuery,
        handlers::SearchRequest,
        handlers::BoolResponse,
        handlers::CountResponse,
        handlers::SupersedeRequest,
        handlers::BatchNoteItem,
        handlers::BatchPushRequest,
        handlers::BatchItemResult,
        handlers::BatchPushResponse,
        handlers::SinceQuery,
        handlers::SinceIdEntry,
        handlers::SinceIdResponse,
        handlers::StreamQuery,
        handlers::HealthResponse,
        handlers::EmbedderStatus,
        handlers::ServerLimits,
        EmbedderState,
        handlers::EmbedRequest,
        handlers::EmbedChunkIn,
        handlers::EmbedResponse,
        handlers::EmbedChunkOut,
        handlers::CodeSearchRequest,
        handlers::CodeSearchResponse,
        handlers::ExploreRequest,
        handlers::ExploreContextChunk,
        handlers::LlmCompleteRequest,
        handlers::LlmCompleteMessage,
        ErrorBody,
        ErrorDetail,
        db::Project,
        db::ServerNote,
        db::ProjectStats,
    )),
    tags(
        (name = "health", description = "Liveness"),
        (name = "projects", description = "Project management"),
        (name = "memory", description = "Memory CRUD and semantic search"),
        (name = "index", description = "Code index / embedding"),
        (name = "search", description = "Server-side code search (query embedding proxy)"),
        (name = "inference", description = "LLM-powered code exploration and raw completion"),
    ),
    security(
        ("bearer_auth" = [])
    ),
    modifiers(&SecurityAddon),
)]
pub struct ApiDoc;

struct SecurityAddon;

impl utoipa::Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "bearer_auth",
            utoipa::openapi::security::SecurityScheme::Http(
                utoipa::openapi::security::HttpBuilder::new()
                    .scheme(utoipa::openapi::security::HttpAuthScheme::Bearer)
                    .bearer_format("API key")
                    .description(Some(
                        "Pass as `Authorization: Bearer <key>`. \
                         Not required when no key is configured on the server.",
                    ))
                    .build(),
            ),
        );
    }
}

// ── Router ────────────────────────────────────────────────────────────────────

/// Build the axum router with all routes.
///
/// - `RequestBodyLimitLayer` + `ConcurrencyLimitLayer` apply to every route.
/// - `TimeoutLayer` (30s) applies to everything except `/memory/stream` (SSE)
///   and `/index/embed` (its own [`EMBED_REQUEST_TIMEOUT`]).
/// - Per-handler input caps are enforced in `handlers.rs`.
pub fn router(state: AppState) -> Router {
    router_with_timeouts(state, REQUEST_TIMEOUT, EMBED_REQUEST_TIMEOUT)
}

/// Same as [`router`], but with an injectable request timeout for both the
/// general route group and `/index/embed`. Lets tests exercise the timeout
/// exemptions on a millisecond-scale budget.
pub fn router_with_timeout(state: AppState, request_timeout: Duration) -> Router {
    router_with_timeouts(state, request_timeout, request_timeout)
}

/// Same as [`router`], but with the general timeout, `/index/embed` timeout, and
/// global concurrency cap all injectable. Lets tests prove `/index/embed`
/// survives past the general budget without waiting out the real 1800s.
pub fn router_with_timeouts(
    state: AppState,
    request_timeout: Duration,
    embed_request_timeout: Duration,
) -> Router {
    router_with_limits(
        state,
        request_timeout,
        embed_request_timeout,
        GLOBAL_CONCURRENCY_LIMIT,
    )
}

/// Same as [`router`], but with the request timeout, `/index/embed` timeout, and
/// global concurrency cap all injectable. Lets tests exercise
/// `ConcurrencyLimitLayer` backpressure with a small limit.
pub fn router_with_limits(
    state: AppState,
    request_timeout: Duration,
    embed_request_timeout: Duration,
    concurrency_limit: usize,
) -> Router {
    let stream_route = Router::new()
        .route(
            "/v1/projects/{project_id}/memory/stream",
            get(handlers::memory_stream),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .layer(RequestBodyLimitLayer::new(DEFAULT_BODY_LIMIT_BYTES))
        .layer(ConcurrencyLimitLayer::new(concurrency_limit));

    // `/index/embed` gets its own long-budget timeout instead of the general
    // `REQUEST_TIMEOUT` — see [`EMBED_REQUEST_TIMEOUT`]. Auth/concurrency/body
    // limits match `protected` below.
    let embed_route = Router::new()
        .route(
            "/v1/projects/{project_id}/index/embed",
            post(handlers::index_embed),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            embed_request_timeout,
        ))
        .layer(RequestBodyLimitLayer::new(DEFAULT_BODY_LIMIT_BYTES))
        .layer(ConcurrencyLimitLayer::new(concurrency_limit));

    let protected = Router::new()
        .route("/v1/projects", get(handlers::list_projects))
        .route("/v1/projects/{project_id}/memory", post(handlers::add_note))
        .route(
            "/v1/projects/{project_id}/memory",
            get(handlers::list_notes),
        )
        // Literal siblings under `/memory/` (this route, `search`, `since`,
        // `harvested-shas`) must stay registered here, in the same router as
        // `/memory/{note_id}` below: matchit resolves a static segment over a
        // param capture regardless of registration order, so `batch` never
        // falls into `{note_id}` (which has no POST handler and would 405, not
        // 404: that was this route's original bug, not a 404).
        .route(
            "/v1/projects/{project_id}/memory/batch",
            post(handlers::push_memory_batch),
        )
        .route(
            "/v1/projects/{project_id}/memory/search",
            post(handlers::search_notes),
        )
        .route(
            "/v1/projects/{project_id}/memory/harvested-shas",
            get(handlers::harvested_shas),
        )
        .route(
            "/v1/projects/{project_id}/memory/since",
            get(handlers::memory_since),
        )
        .route(
            "/v1/projects/{project_id}/memory/{note_id}",
            get(handlers::get_note),
        )
        .route(
            "/v1/projects/{project_id}/memory/{note_id}",
            delete(handlers::delete_note),
        )
        .route(
            "/v1/projects/{project_id}/memory/{note_id}/archive",
            post(handlers::archive_note),
        )
        .route(
            "/v1/projects/{project_id}/memory/{note_id}/supersede",
            post(handlers::supersede_note),
        )
        .route(
            "/v1/projects/{project_id}/stats",
            get(handlers::project_stats),
        )
        .route(
            "/v1/projects/{project_id}/search",
            post(handlers::project_search),
        )
        .route("/v1/projects/{project_id}/explore", post(handlers::explore))
        .route(
            "/v1/projects/{project_id}/llm/complete",
            post(handlers::llm_complete),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            request_timeout,
        ))
        .layer(RequestBodyLimitLayer::new(DEFAULT_BODY_LIMIT_BYTES))
        .layer(ConcurrencyLimitLayer::new(concurrency_limit));

    Router::new()
        .route("/v1/health", get(handlers::health))
        .route("/api-docs/openapi.json", get(openapi_spec))
        .merge(stream_route)
        .merge(embed_route)
        .merge(protected)
        .with_state(state)
}

// ── AppError response mapping tests ────────────────────────────────────────────

#[cfg(test)]
mod app_error_tests {
    use axum::response::IntoResponse;

    use super::AppError;
    use crate::db::DimensionMismatch;

    /// Extract the response status + JSON body as a string for assertions.
    async fn body_string(resp: axum::response::Response) -> (axum::http::StatusCode, String) {
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// A `DimensionMismatch` wrapped in `AppError::Internal` must map to a 400
    /// with the typed, safe message — not fall through to the generic 500.
    #[tokio::test]
    async fn dimension_mismatch_maps_to_typed_bad_request() {
        let err = anyhow::Error::new(DimensionMismatch {
            slug: "proj".to_string(),
            expected: 896,
            got: 4,
        });
        let (status, body) = body_string(AppError::Internal(err).into_response()).await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert!(body.contains("dimension mismatch"));
        assert!(body.contains("proj"));
    }

    /// Any other error — even one whose text contains "mismatch"/"required" —
    /// must NEVER have its raw text reach the client body; only the generic
    /// "Internal server error" message is allowed through. Regression guard
    /// against substring-sniffing the error message.
    #[tokio::test]
    async fn generic_internal_error_never_leaks_raw_text_even_with_trigger_words() {
        let secret_path = "/Users/johan/.ssh/id_ed25519";
        let err = anyhow::anyhow!(
            "column count mismatch: table 'chunks' required 12 columns but got 3 at {secret_path}"
        );
        let (status, body) = body_string(AppError::Internal(err).into_response()).await;
        assert_eq!(status, axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!body.contains("mismatch"));
        assert!(!body.contains("required"));
        assert!(!body.contains(secret_path));
        assert!(!body.contains("chunks"));
        assert_eq!(
            body,
            r#"{"error":{"code":"internal_error","message":"Internal server error"}}"#
        );
    }

    /// A plain anyhow error with no special substrings still gets the generic
    /// message (baseline, no leak).
    #[tokio::test]
    async fn plain_internal_error_returns_generic_message() {
        let err = anyhow::anyhow!("disk I/O error at /var/lib/spelunk/server.db");
        let (status, body) = body_string(AppError::Internal(err).into_response()).await;
        assert_eq!(status, axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!body.contains("/var/lib/spelunk"));
    }
}

// ── OpenAPI spec endpoint ─────────────────────────────────────────────────────

/// Serve the OpenAPI spec as JSON. Import into Postman via
/// `File → Import → Link` using the server URL + `/api-docs/openapi.json`.
async fn openapi_spec() -> impl IntoResponse {
    Json(ApiDoc::openapi())
}

// ── Auth middleware ───────────────────────────────────────────────────────────

/// Trait-driven auth middleware. Delegates to `AppState.auth`.
async fn auth_middleware(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    match state.auth.authenticate(request.headers()).await {
        Ok(ctx) => {
            request.extensions_mut().insert(ctx);
            next.run(request).await
        }
        Err(AuthError(msg)) => (
            StatusCode::UNAUTHORIZED,
            Json(ErrorBody::new("unauthorized", &msg)),
        )
            .into_response(),
    }
}

// ── Shared error body ─────────────────────────────────────────────────────────

/// Consistent JSON error body: `{"error": {"code": "...", "message": "..."}}`.
#[derive(Serialize, ToSchema)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Serialize, ToSchema)]
pub struct ErrorDetail {
    pub code: String,
    pub message: String,
}

impl ErrorBody {
    pub fn new(code: &str, message: &str) -> Self {
        Self {
            error: ErrorDetail {
                code: code.to_string(),
                message: message.to_string(),
            },
        }
    }
}

// ── Error type ────────────────────────────────────────────────────────────────

/// Map anyhow errors to HTTP responses using the standard error body format.
pub enum AppError {
    NotFound,
    BadRequest(String),
    ServiceUnavailable(String),
    /// The server-side embedder is not ready. `503` for both, but `loading`
    /// (transient) adds `Retry-After: 5` and carries `"state": "loading"` so the
    /// CLI keeps polling, whereas `unavailable` (terminal, `terminal: true`)
    /// carries `"state": "unavailable"` so the CLI stops and surfaces the error.
    EmbedderWarmingUp {
        terminal: bool,
        detail: String,
    },
    Internal(anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            AppError::NotFound => (
                StatusCode::NOT_FOUND,
                Json(ErrorBody::new("not_found", "Not found")),
            )
                .into_response(),
            AppError::BadRequest(msg) => (
                StatusCode::BAD_REQUEST,
                Json(ErrorBody::new("bad_request", &msg)),
            )
                .into_response(),
            AppError::ServiceUnavailable(msg) => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorBody::new("service_unavailable", &msg)),
            )
                .into_response(),
            AppError::EmbedderWarmingUp { terminal, detail } => {
                let state = if terminal { "unavailable" } else { "loading" };
                let error = if terminal {
                    "embedder unavailable"
                } else {
                    "embedder warming up, retry shortly"
                };
                let body = Json(serde_json::json!({
                    "error": error,
                    "state": state,
                    "detail": detail,
                }));
                if terminal {
                    (StatusCode::SERVICE_UNAVAILABLE, body).into_response()
                } else {
                    // Transient: tell polite clients when to retry.
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        [(axum::http::header::RETRY_AFTER, "5")],
                        body,
                    )
                        .into_response()
                }
            }
            AppError::Internal(e) => {
                // Only a known, explicitly-typed user-facing error is ever
                // surfaced to the client; everything else gets a generic
                // message so raw error text (paths, SQL, etc.) never reaches
                // the response body. No substring sniffing of the message.
                if let Some(mismatch) = e.downcast_ref::<crate::db::DimensionMismatch>() {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(ErrorBody::new("bad_request", &mismatch.to_string())),
                    )
                        .into_response()
                } else if let Some(mismatch) = e.downcast_ref::<crate::db::ModelMismatch>() {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(ErrorBody::new("bad_request", &mismatch.to_string())),
                    )
                        .into_response()
                } else {
                    tracing::error!("internal error: {e:#}");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(ErrorBody::new("internal_error", "Internal server error")),
                    )
                        .into_response()
                }
            }
        }
    }
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        AppError::Internal(e.into())
    }
}

// ── OpenAPI snapshot test ─────────────────────────────────────────────────────

#[cfg(test)]
mod openapi_tests {
    use utoipa::OpenApi;

    /// Write the generated OpenAPI spec to `docs/openapi.json` so it can be
    /// committed as the reference snapshot.  Run with:
    ///   cargo test -p spelunk-server write_openapi_snapshot -- --nocapture
    #[test]
    fn write_openapi_snapshot() {
        let spec = super::ApiDoc::openapi()
            .to_pretty_json()
            .expect("serialise openapi");
        // Resolve path relative to the workspace root (two levels up from src/).
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root")
            .join("docs/openapi.json");
        std::fs::create_dir_all(out.parent().unwrap()).ok();
        std::fs::write(&out, &spec).expect("write docs/openapi.json");
        println!("Written: {}", out.display());
    }
}
