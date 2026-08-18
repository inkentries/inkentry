// Test helpers shared across the `handlers::tests::*` theme modules: app/router
// builders, mock backends reused by more than one theme, and thin HTTP request
// helpers. Single-use mocks stay local to the theme file that needs them.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{self, Request};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::auth::ApiKeyAuth;
use crate::client_ip::TrustedProxies;
use crate::db::ServerDb;
use crate::{AppState, router};

// Register sqlite-vec extension once per test process.
pub(super) fn register_sqlite_vec() {
    use std::sync::OnceLock;
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        #[allow(clippy::missing_transmute_annotations)]
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}

pub(super) fn make_app(conflict_threshold: f32) -> (axum::Router, i32) {
    register_sqlite_vec();
    let dim: usize = 4;
    let db = ServerDb::open(std::path::Path::new(":memory:"), dim, "test-model")
        .expect("failed to open in-memory server db");
    let instance_id = db.get_or_create_instance_id().expect("instance_id in test");
    let state = AppState {
        db: Arc::new(tokio::sync::Mutex::new(db)),
        auth: Arc::new(ApiKeyAuth::new(None)),
        conflict_threshold,
        embedder: crate::EmbedderSlot::disabled(),
        embed_admission: crate::EmbedAdmission::new(
            crate::EMBED_QUEUE_CAPACITY,
            crate::EMBED_BUSY_RETRY_AFTER_SECS,
        ),
        llm: None,
        max_tokens_ceiling: 8192,
        rate_limiter: Arc::new(crate::rate_limiter::RateLimiter::new(1000, 60)),
        instance_id,
        started_by: None,
        trusted_proxies: Default::default(),
        relay: crate::relay::RelayRegistry::disabled(),
        repair_signal: crate::repair::RepairSignal::new(),
    };
    (router(state), dim as i32)
}

// POST /v1/projects/{slug}/memory with the given client-pushed vector, tagged
// as the accept side requires. Returns the response.
pub(super) async fn post_note(
    app: axum::Router,
    slug: &str,
    title: &str,
    vector: Vec<f32>,
) -> (http::StatusCode, Value) {
    let body = json!({
        "kind": "note",
        "title": title,
        "body": "test body",
        "vector": vector,
        "vector_model": inkentry_core::embeddings::pushed_vector_model_tag(),
        "vector_precision": inkentry_core::embeddings::PUSHED_VECTOR_PRECISION,
    });
    let req = Request::builder()
        .method("POST")
        .uri(format!("/v1/projects/{slug}/memory"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

// A minimal mock embedder that always returns a single zero vector of `dim` dimensions.
// Used to verify that `embedding_dim` is surfaced correctly in the health response.
pub(super) struct MockEmbedder {
    pub(super) dim: usize,
}

#[async_trait::async_trait]
impl inkentry_core::embeddings::EmbeddingBackend for MockEmbedder {
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![0.0_f32; self.dim]).collect())
    }

    fn dimension(&self) -> usize {
        self.dim
    }
}

// Build an app with the given embedder slot (dim used only to size the DB).
pub(super) fn make_app_with_slot(dim: usize, embedder: crate::EmbedderSlot) -> axum::Router {
    crate::router(make_state_with_slot(dim, embedder))
}

// The state behind `make_app_with_slot`, for tests that need to reach past the
// router: the repair pass takes an `AppState` directly, and the repair signal
// is only observable on the state a request was served from.
pub(super) fn make_state_with_slot(dim: usize, embedder: crate::EmbedderSlot) -> AppState {
    register_sqlite_vec();
    let db = ServerDb::open(std::path::Path::new(":memory:"), dim, "test-model")
        .expect("failed to open in-memory server db");
    let instance_id = db.get_or_create_instance_id().expect("instance_id in test");
    AppState {
        db: Arc::new(tokio::sync::Mutex::new(db)),
        auth: Arc::new(ApiKeyAuth::new(None)),
        conflict_threshold: 0.92,
        embedder,
        embed_admission: crate::EmbedAdmission::new(
            crate::EMBED_QUEUE_CAPACITY,
            crate::EMBED_BUSY_RETRY_AFTER_SECS,
        ),
        llm: None,
        max_tokens_ceiling: 8192,
        rate_limiter: Arc::new(crate::rate_limiter::RateLimiter::new(1000, 60)),
        instance_id,
        started_by: None,
        trusted_proxies: Default::default(),
        relay: crate::relay::RelayRegistry::disabled(),
        repair_signal: crate::repair::RepairSignal::new(),
    }
}

// Records every text it is asked to embed, so a test can prove an entry was
// never embedded at all rather than inferring it from the stored vector. Also
// the only way to prove which text the repair pass embedded a row with.
pub(super) struct RecordingEmbedder {
    pub(super) dim: usize,
    pub(super) embedded: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl inkentry_core::embeddings::EmbeddingBackend for RecordingEmbedder {
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        let mut seen = self.embedded.lock().unwrap();
        for t in texts {
            seen.push((*t).to_string());
        }
        Ok(texts.iter().map(|_| vec![0.5_f32; self.dim]).collect())
    }

    fn dimension(&self) -> usize {
        self.dim
    }
}

pub(super) fn recording_slot(
    dim: usize,
) -> (crate::EmbedderSlot, Arc<std::sync::Mutex<Vec<String>>>) {
    let embedded = Arc::new(std::sync::Mutex::new(Vec::new()));
    let slot = crate::EmbedderSlot::ready(Arc::new(RecordingEmbedder {
        dim,
        embedded: embedded.clone(),
    }));
    (slot, embedded)
}

pub(super) fn app_with_recording_embedder(
    dim: usize,
) -> (axum::Router, Arc<std::sync::Mutex<Vec<String>>>) {
    let (slot, embedded) = recording_slot(dim);
    (make_app_with_slot(dim, slot), embedded)
}

// Build an app with a ready mock embedder of the given dimension.
pub(super) fn make_app_with_embedder(dim: usize) -> axum::Router {
    make_app_with_slot(
        dim,
        crate::EmbedderSlot::ready(Arc::new(MockEmbedder { dim })),
    )
}

pub(super) async fn get_health_json(app: axum::Router) -> Value {
    let req = Request::builder()
        .method("GET")
        .uri("/v1/health")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), http::StatusCode::OK, "health must be 200");
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).expect("health must return JSON")
}

pub(super) async fn post_embed(app: axum::Router) -> http::Response<Body> {
    let body = json!({"chunks": [{"chunk_id": "abc", "content": "fn foo() {}"}]});
    let req = Request::builder()
        .method("POST")
        .uri("/v1/projects/proj/index/embed")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    app.oneshot(req).await.unwrap()
}

// An LLM backend that immediately closes the token channel: enough to
// exercise routing/rate-limiting without generating real content.
struct NoopLlm;

#[async_trait::async_trait]
impl inkentry_core::llm::LlmBackend for NoopLlm {
    async fn generate(
        &self,
        _messages: &[inkentry_core::llm::Message],
        _max_tokens: usize,
        _tx: tokio::sync::mpsc::Sender<inkentry_core::llm::Token>,
        _json_schema: Option<serde_json::Value>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

// Build an app with a configured LLM backend and a tight rate limit, for
// exercising `/llm/complete` rate limiting.
pub(super) fn make_app_with_llm_and_limit(max_requests: u32) -> axum::Router {
    make_app_with_llm_limit_and_proxies(max_requests, TrustedProxies::default())
}

// As above, but with an explicit trusted-proxy list, so a test can exercise
// both the default (believe nobody's `X-Forwarded-For`) and the opted-in
// deployment.
pub(super) fn make_app_with_llm_limit_and_proxies(
    max_requests: u32,
    trusted_proxies: TrustedProxies,
) -> axum::Router {
    register_sqlite_vec();
    let db = ServerDb::open(std::path::Path::new(":memory:"), 4, "test-model")
        .expect("failed to open in-memory server db");
    let instance_id = db.get_or_create_instance_id().expect("instance_id in test");
    let state = AppState {
        db: Arc::new(tokio::sync::Mutex::new(db)),
        auth: Arc::new(ApiKeyAuth::new(None)),
        conflict_threshold: 0.92,
        embedder: crate::EmbedderSlot::disabled(),
        embed_admission: crate::EmbedAdmission::new(
            crate::EMBED_QUEUE_CAPACITY,
            crate::EMBED_BUSY_RETRY_AFTER_SECS,
        ),
        llm: Some(Arc::new(NoopLlm)),
        max_tokens_ceiling: 8192,
        rate_limiter: Arc::new(crate::rate_limiter::RateLimiter::new(max_requests, 60)),
        instance_id,
        started_by: None,
        trusted_proxies,
        relay: crate::relay::RelayRegistry::disabled(),
        repair_signal: crate::repair::RepairSignal::new(),
    };
    router(state)
}

// POST /llm/complete over a connection whose TCP peer is `peer`, optionally
// carrying a client-supplied `X-Forwarded-For`. `ConnectInfo` is inserted the
// same way `into_make_service_with_connect_info` inserts it in production, so
// two different `peer` values are two different clients as far as every
// handler can tell.
pub(super) async fn post_llm_complete_from(
    app: &axum::Router,
    peer: &str,
    forwarded_for: Option<&str>,
) -> http::StatusCode {
    let body = json!({
        "messages": [{"role": "user", "content": "q"}],
        "max_tokens": 16,
    });
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/projects/llm-test/llm/complete")
        .header("content-type", "application/json");
    if let Some(xff) = forwarded_for {
        builder = builder.header("x-forwarded-for", xff);
    }
    let mut req = builder
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let addr: SocketAddr = peer.parse().expect("test peer address");
    req.extensions_mut().insert(ConnectInfo(addr));
    app.clone().oneshot(req).await.unwrap().status()
}

pub(super) async fn post_llm_complete(app: &axum::Router, content: &str) -> http::StatusCode {
    let body = json!({
        "messages": [{"role": "user", "content": content}],
        "max_tokens": 16,
    });
    let req = Request::builder()
        .method("POST")
        .uri("/v1/projects/llm-test/llm/complete")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

// Build an app with an explicit auth key configured (for 401 tests).
pub(super) fn make_app_with_auth_key(key: Option<&str>) -> axum::Router {
    register_sqlite_vec();
    let db = ServerDb::open(std::path::Path::new(":memory:"), 4, "test-model")
        .expect("failed to open in-memory server db");
    let instance_id = db.get_or_create_instance_id().expect("instance_id in test");
    let state = AppState {
        db: Arc::new(tokio::sync::Mutex::new(db)),
        auth: Arc::new(ApiKeyAuth::new(key.map(str::to_string))),
        conflict_threshold: 0.92,
        embedder: crate::EmbedderSlot::disabled(),
        embed_admission: crate::EmbedAdmission::new(
            crate::EMBED_QUEUE_CAPACITY,
            crate::EMBED_BUSY_RETRY_AFTER_SECS,
        ),
        llm: None,
        max_tokens_ceiling: 8192,
        rate_limiter: Arc::new(crate::rate_limiter::RateLimiter::new(1000, 60)),
        instance_id,
        started_by: None,
        trusted_proxies: Default::default(),
        relay: crate::relay::RelayRegistry::disabled(),
        repair_signal: crate::repair::RepairSignal::new(),
    };
    crate::router(state)
}

// POST /v1/projects/{slug}/memory/batch with a raw `entries` JSON value
// (not a typed struct, so malformed/missing-field payloads can be built).
fn batch_request(slug: &str, entries: Value) -> Request<Body> {
    let body = json!({ "entries": entries });
    Request::builder()
        .method("POST")
        .uri(format!("/v1/projects/{slug}/memory/batch"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

pub(super) async fn post_batch(
    app: axum::Router,
    slug: &str,
    entries: Value,
) -> (http::StatusCode, Value) {
    let resp = app.oneshot(batch_request(slug, entries)).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

pub(super) async fn list_notes_via_http(app: axum::Router, slug: &str) -> Vec<Value> {
    let req = Request::builder()
        .method("GET")
        .uri(format!("/v1/projects/{slug}/memory?limit=100"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    body["entries"].as_array().cloned().unwrap_or_default()
}

pub(super) fn note_item(title: &str, external_id: &str) -> Value {
    json!({"kind": "note", "title": title, "external_id": external_id})
}

pub(super) async fn get_status_and_json(app: axum::Router, uri: &str) -> (http::StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

// Bind a real router (with the given injected timeout) to an ephemeral
// TCP port and start serving it in the background. Returns the base URL
// and the shared DB handle (so tests can hold its lock externally to
// simulate a slow synchronous handler).
pub(super) async fn spawn_test_server(
    llm: Option<Arc<dyn inkentry_core::llm::LlmBackend>>,
    request_timeout: std::time::Duration,
) -> (String, Arc<tokio::sync::Mutex<ServerDb>>) {
    register_sqlite_vec();
    let db = ServerDb::open(std::path::Path::new(":memory:"), 4, "test-model")
        .expect("failed to open in-memory server db");
    let instance_id = db.get_or_create_instance_id().expect("instance_id in test");
    // Create the project up front so `/memory/stream` (which 404s on an
    // unknown project) has something valid to stream from.
    db.upsert_project("timeout-test", 4, "test-model")
        .expect("create test project");
    let db = Arc::new(tokio::sync::Mutex::new(db));
    let state = AppState {
        db: db.clone(),
        auth: Arc::new(ApiKeyAuth::new(None)),
        conflict_threshold: 0.92,
        embedder: crate::EmbedderSlot::disabled(),
        embed_admission: crate::EmbedAdmission::new(
            crate::EMBED_QUEUE_CAPACITY,
            crate::EMBED_BUSY_RETRY_AFTER_SECS,
        ),
        llm,
        max_tokens_ceiling: 8192,
        rate_limiter: Arc::new(crate::rate_limiter::RateLimiter::new(1000, 60)),
        instance_id,
        started_by: None,
        trusted_proxies: Default::default(),
        relay: crate::relay::RelayRegistry::disabled(),
        repair_signal: crate::repair::RepairSignal::new(),
    };
    let app = crate::router_with_timeout(state, request_timeout);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .expect("test server crashed");
    });
    (format!("http://{addr}"), db)
}

// Same as [`spawn_test_server`], but with an embedder slot and the
// general/`/index/embed` timeouts injected independently: exists so
// tests can prove `/index/embed` survives past the *general*
// `request_timeout` budget using its own, separately-injected
// `embed_request_timeout` (mirroring the production
// `REQUEST_TIMEOUT`/`EMBED_REQUEST_TIMEOUT` split), without waiting out
// real multi-second budgets.
pub(super) async fn spawn_test_server_with_embed(
    embedder: crate::EmbedderSlot,
    request_timeout: std::time::Duration,
    embed_request_timeout: std::time::Duration,
) -> (String, Arc<tokio::sync::Mutex<ServerDb>>) {
    spawn_test_server_with_embed_and_admission(
        embedder,
        request_timeout,
        embed_request_timeout,
        crate::EmbedAdmission::new(
            crate::EMBED_QUEUE_CAPACITY,
            crate::EMBED_BUSY_RETRY_AFTER_SECS,
        ),
    )
    .await
}

// Same as [`spawn_test_server_with_embed`], but with the embed admission
// gate injected too: exists so tests can prove the `429` shedding
// behaviour with a small, deterministic queue capacity instead of the
// production default.
pub(super) async fn spawn_test_server_with_embed_and_admission(
    embedder: crate::EmbedderSlot,
    request_timeout: std::time::Duration,
    embed_request_timeout: std::time::Duration,
    embed_admission: crate::EmbedAdmission,
) -> (String, Arc<tokio::sync::Mutex<ServerDb>>) {
    register_sqlite_vec();
    let db = ServerDb::open(std::path::Path::new(":memory:"), 4, "test-model")
        .expect("failed to open in-memory server db");
    let instance_id = db.get_or_create_instance_id().expect("instance_id in test");
    db.upsert_project("timeout-test", 4, "test-model")
        .expect("create test project");
    let db = Arc::new(tokio::sync::Mutex::new(db));
    let state = AppState {
        db: db.clone(),
        auth: Arc::new(ApiKeyAuth::new(None)),
        conflict_threshold: 0.92,
        embedder,
        embed_admission,
        llm: None,
        max_tokens_ceiling: 8192,
        rate_limiter: Arc::new(crate::rate_limiter::RateLimiter::new(1000, 60)),
        instance_id,
        started_by: None,
        trusted_proxies: Default::default(),
        relay: crate::relay::RelayRegistry::disabled(),
        repair_signal: crate::repair::RepairSignal::new(),
    };
    let app = crate::router_with_timeouts(state, request_timeout, embed_request_timeout);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .expect("test server crashed");
    });
    (format!("http://{addr}"), db)
}
