use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use axum::http::HeaderMap;
use tokio::sync::mpsc;

use crate::auth::AuthContext;
use crate::client_ip::{TrustedProxies, client_ip_key};
use crate::{AppError, AppState, EmbedderState};

mod batch;
mod health;
mod index;
mod llm;
mod notes;
mod projects;
mod search;
mod sync;

pub use batch::*;
pub use health::*;
pub use index::*;
pub use llm::*;
pub use notes::*;
pub use projects::*;
pub use search::*;
pub use sync::*;

#[cfg(test)]
mod tests;

/// Max length (chars) for a memory entry's `title`.
pub const MAX_TITLE_LEN: usize = 500;
/// Max length (chars) for a memory entry's `body`.
pub const MAX_BODY_LEN: usize = 50_000;
/// Max length (bytes) for a `project_id` path slug (e.g. `inkentries/inkentry`).
pub const MAX_SLUG_LEN: usize = 200;
/// Max number of chunks accepted in a single `/index/embed` request. Also
/// advertised in `/v1/health`'s `limits.max_batch_chunks` so a client can size
/// its calibrated batch without guessing.
pub const MAX_EMBED_BATCH: usize = 256;
/// Max number of entries accepted in a single `POST /memory/batch` request.
/// Comfortably exceeds a legitimate CLI push's own chunk size, so a real push
/// never trips it.
pub const MAX_BATCH_ENTRIES: usize = 200;

// Shared by every handler that accepts free-text memory content.
fn validate_title_body(title: &str, body: &str) -> Result<(), AppError> {
    if title.chars().count() > MAX_TITLE_LEN {
        return Err(AppError::BadRequest(format!(
            "title exceeds maximum length of {MAX_TITLE_LEN} characters (got {})",
            title.chars().count()
        )));
    }
    if body.chars().count() > MAX_BODY_LEN {
        return Err(AppError::BadRequest(format!(
            "body exceeds maximum length of {MAX_BODY_LEN} characters (got {})",
            body.chars().count()
        )));
    }
    Ok(())
}

// A client may push its own vector to skip server-side embedding, but only
// if it matches what this server would have produced: tagged with the right
// model and precision, the right dimension, no non-finite components, and an
// L2 magnitude inside [0.5, 1.5] — wide enough to catch an unnormalised or
// zero vector without policing float error around 1.0. `None` always passes.
// The index ranks by Euclidean distance, which agrees with cosine similarity
// only across vectors of equal length, so an unnormalised vector would rank
// against unrelated entries with nothing downstream able to tell. A vector
// or tag outside contract is refused rather than coerced, so the mismatch
// reaches the caller instead of silently landing next to the server's own
// vectors in the same index.
fn validate_pushed_vector(
    vector: Option<&[f32]>,
    model: Option<&str>,
    precision: Option<&str>,
    configured_dim: usize,
) -> Result<(), AppError> {
    let Some(v) = vector else {
        return Ok(());
    };

    let expected_model = inkentry_core::embeddings::pushed_vector_model_tag();
    match model {
        Some(m) if m == expected_model => {}
        Some(m) => {
            return Err(AppError::BadRequest(format!(
                "pushed vector model '{m}' does not match server embedding model '{expected_model}'"
            )));
        }
        None => {
            return Err(AppError::BadRequest(format!(
                "vector_model is required with a pushed vector; expected '{expected_model}'"
            )));
        }
    }

    let expected_precision = inkentry_core::embeddings::PUSHED_VECTOR_PRECISION;
    match precision {
        Some(p) if p == expected_precision => {}
        Some(p) => {
            return Err(AppError::BadRequest(format!(
                "pushed vector precision '{p}' is unsupported; expected '{expected_precision}'"
            )));
        }
        None => {
            return Err(AppError::BadRequest(format!(
                "vector_precision is required with a pushed vector; expected '{expected_precision}'"
            )));
        }
    }

    if configured_dim != 0 && v.len() != configured_dim {
        return Err(AppError::BadRequest(format!(
            "embedding vector length {} does not match server's configured dimension {configured_dim}",
            v.len()
        )));
    }
    if !v.iter().all(|x| x.is_finite()) {
        return Err(AppError::BadRequest(
            "pushed vector contains NaN or infinite values".into(),
        ));
    }
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if !(0.5..=1.5).contains(&norm) {
        return Err(AppError::BadRequest(format!(
            "pushed vector L2 magnitude {norm:.4} outside expected [0.5, 1.5]"
        )));
    }
    Ok(())
}

// `None` always passes; the server mints its own id. A present value must
// be a well-formed UUIDv7. A malformed id is rejected 400 rather than
// coerced, so a bad id surfaces loudly instead of producing a divergent
// identity.
fn validate_optional_uuid_v7(id: Option<&str>) -> Result<(), AppError> {
    let Some(id) = id else {
        return Ok(());
    };
    match uuid::Uuid::parse_str(id) {
        Ok(u) if u.get_version() == Some(uuid::Version::SortRand) => Ok(()),
        Ok(_) => Err(AppError::BadRequest(format!(
            "id '{id}' must be a UUIDv7 (wrong version)"
        ))),
        Err(_) => Err(AppError::BadRequest(format!(
            "id '{id}' is not a well-formed UUID"
        ))),
    }
}

// Project ids are human slugs (e.g. `inkentries/inkentry`), not UUIDs, so
// this is a length/sanity cap rather than a UUID-format check.
fn validate_project_slug(slug: &str) -> Result<(), AppError> {
    if slug.is_empty() {
        return Err(AppError::BadRequest("project_id must not be empty".into()));
    }
    if slug.len() > MAX_SLUG_LEN {
        return Err(AppError::BadRequest(format!(
            "project_id exceeds maximum length of {MAX_SLUG_LEN} bytes (got {})",
            slug.len()
        )));
    }
    Ok(())
}

// Lets tests inject a millisecond-scale generation budget in place of
// `crate::REQUEST_TIMEOUT`.
#[cfg(test)]
static GENERATION_TIMEOUT_OVERRIDE: std::sync::OnceLock<std::sync::Mutex<Option<Duration>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
fn set_generation_timeout_override(d: Duration) {
    let cell = GENERATION_TIMEOUT_OVERRIDE.get_or_init(|| std::sync::Mutex::new(None));
    *cell.lock().expect("override mutex poisoned") = Some(d);
}

#[cfg(test)]
fn clear_generation_timeout_override() {
    if let Some(cell) = GENERATION_TIMEOUT_OVERRIDE.get() {
        *cell.lock().expect("override mutex poisoned") = None;
    }
}

#[cfg(test)]
fn generation_timeout() -> Duration {
    GENERATION_TIMEOUT_OVERRIDE
        .get()
        .and_then(|cell| *cell.lock().expect("override mutex poisoned"))
        .unwrap_or(crate::REQUEST_TIMEOUT)
}

#[cfg(not(test))]
#[inline]
fn generation_timeout() -> Duration {
    crate::REQUEST_TIMEOUT
}

// `/llm/complete` returns its SSE `Response` as soon as the stream is built
// and hands generation to a detached `tokio::spawn`, so the router-level
// `TimeoutLayer` never sees this work. This applies the same budget to the
// generation call itself, so a hung/slow backend can't hold the task (and
// the SSE connection it feeds) open forever.
async fn llm_generate_with_timeout(
    llm: Arc<dyn inkentry_core::llm::LlmBackend>,
    messages: Vec<inkentry_core::llm::Message>,
    max_tokens: usize,
    tx: mpsc::Sender<String>,
    json_schema: Option<serde_json::Value>,
    label: &'static str,
) {
    let budget = generation_timeout();
    match tokio::time::timeout(budget, llm.generate(&messages, max_tokens, tx, json_schema)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!("{label} LLM generate error: {e}"),
        Err(_elapsed) => {
            tracing::warn!(
                "{label} LLM generate exceeded the {budget:?} generation budget; aborting",
            );
            // Dropping the tx-holding future here closes the channel, so the
            // SSE stream's rx.recv() loop sees None and ends the connection.
        }
    }
}

// Keying on IP as well as principal means a shared team API key doesn't
// collapse every distinct client onto one shared rate-limit bucket: each
// caller gets its own budget. Both halves must be outside the caller's
// control or the budget is not a budget.
fn rate_limit_key(
    auth_ctx: &AuthContext,
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
    trusted_proxies: &TrustedProxies,
) -> String {
    let principal = match &auth_ctx.principal {
        crate::auth::Principal::ApiKey(k) => k.clone(),
        crate::auth::Principal::User { id } => id.clone(),
    };
    let ip = client_ip_key(headers, peer, trusted_proxies);
    format!("{principal}|{ip}")
}

// Translates the embedder slot's readiness into the correct HTTP error when
// it is not `ready`: `loading` -> 503 + Retry-After (transient, CLI keeps
// polling); `unavailable` -> 503 (terminal, CLI surfaces the error);
// `disabled` -> 400 (permanent misconfiguration for this request).
fn require_embedder(
    state: &AppState,
    disabled_msg: &str,
) -> Result<Arc<dyn inkentry_core::embeddings::EmbeddingBackend>, AppError> {
    if let Some(backend) = state.embedder.backend() {
        return Ok(backend);
    }
    match state.embedder.state() {
        EmbedderState::Loading => {
            let detail = state
                .embedder
                .detail()
                .unwrap_or_else(|| "embedder warming up, retry shortly".to_string());
            // debug, not warn: this is expected during warm-up, not an outage.
            tracing::debug!(%detail, "embed request rejected: embedder still loading");
            Err(AppError::EmbedderWarmingUp {
                terminal: false,
                detail,
            })
        }
        EmbedderState::Unavailable => {
            let detail = state
                .embedder
                .detail()
                .unwrap_or_else(|| "embedder failed to load".to_string());
            tracing::warn!(%detail, "embed request rejected: embedder unavailable (load failed)");
            Err(AppError::EmbedderWarmingUp {
                terminal: true,
                detail,
            })
        }
        // Disabled (or the improbable ready-but-no-backend race) → permanent 400.
        EmbedderState::Disabled | EmbedderState::Ready => {
            Err(AppError::BadRequest(disabled_msg.to_string()))
        }
    }
}

// Embeds memory-entry text for the storage routes and the repair sweep,
// storing text-only rather than failing when no vector can be produced.
// `lane` is the caller's, not the text's (interactive for a person waiting,
// bulk for a batch push or the repair sweep).
//
// Must never be called with the `ServerDb` lock held: that lock is global,
// so an embed awaited under it stalls every other request until the whole
// batch finishes. Runs under an EmbedAdmission permit on `lane` like every
// other embed-consuming route, so a storage write can't bypass the bound on
// concurrent embedder callers. The whole slice goes in one call, keeping
// the lock-free window short and the embed itself cheaper.
pub(crate) async fn embed_for_storage(
    state: &AppState,
    texts: &[&str],
    lane: crate::EmbedLane,
) -> Result<StorageEmbedding, AppError> {
    if texts.is_empty() {
        return Ok(StorageEmbedding::Vectors(Vec::new()));
    }
    // A memory write must not block on model warm-up.
    let Some(embedder) = state.embedder.backend() else {
        return Ok(StorageEmbedding::NotReady);
    };
    let _admission = state.embed_admission.try_acquire(lane)?;
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    match embedder.embed_lane(texts, cancel, lane).await {
        Ok(vectors) if vectors.len() == texts.len() => Ok(StorageEmbedding::Vectors(vectors)),
        Ok(vectors) => {
            tracing::warn!(
                "server-side embedding returned {} vectors for {} entries, storing without vectors",
                vectors.len(),
                texts.len(),
            );
            Ok(StorageEmbedding::Failed)
        }
        Err(e) => {
            tracing::warn!("server-side embedding failed, storing without vector: {e}");
            Ok(StorageEmbedding::Failed)
        }
    }
}

// The two degraded arms are kept apart for the repair pass, not for the
// write paths: repair must stop on `NotReady` (nothing it retries can make
// progress) but fall back to smaller units on `Failed` (one text in the
// page is poison, the rest are still embeddable).
pub(crate) enum StorageEmbedding {
    // One vector per input text, in input order.
    Vectors(Vec<Vec<f32>>),
    // No backend is ready: loading, unavailable, or disabled.
    NotReady,
    // An error, or a vector count that doesn't line up with the input —
    // treated as a failure rather than a partial success, since with the
    // input-to-output mapping unknown any assignment could attach a vector
    // to the wrong text.
    Failed,
}

// Both write paths and the repair pass call this, so a repaired row's
// vector cannot describe a differently shaped string than the push path
// would have produced.
pub(crate) fn storage_embedding_text(title: &str, body: &str) -> String {
    format!("title: {title} | text: {body}")
}

fn require_project(db: &crate::db::ServerDb, slug: &str) -> Result<crate::db::Project, AppError> {
    validate_project_slug(slug)?;
    db.get_project(slug)?.ok_or(AppError::NotFound)
}
