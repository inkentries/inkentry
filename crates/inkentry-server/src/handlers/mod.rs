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

// ── Input validation caps ─────────────────────────────────────────────────────

/// Max length (chars) for a memory entry's `title`.
pub const MAX_TITLE_LEN: usize = 500;
/// Max length (chars) for a memory entry's `body`.
pub const MAX_BODY_LEN: usize = 50_000;
/// Max length (bytes) for a `project_id` path slug (e.g. `usercise/spelunk`).
pub const MAX_SLUG_LEN: usize = 200;
/// Max number of chunks accepted in a single `/index/embed` request. Also
/// advertised in `/v1/health`'s `limits.max_batch_chunks` so a client can size
/// its calibrated batch without guessing (see `HealthResponse`).
pub const MAX_EMBED_BATCH: usize = 256;
/// Max number of entries accepted in a single `POST /memory/batch` request.
/// Matches cloud-api's cap and comfortably exceeds the CLI's own push chunk
/// size (`PUSH_BATCH_CHUNK_SIZE` in `sync.rs`), so a legitimate CLI push never
/// trips it.
pub const MAX_BATCH_ENTRIES: usize = 200;

/// Reject a title/body pair that exceeds the configured caps. Shared by every
/// handler that accepts free-text memory content (`add_note`, `supersede`'s
/// linked note content is validated at insert time, etc.).
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

/// Full-contract validation for a client-pushed embedding vector.
///
/// A client may push its own vector to skip server-side embedding, but only if
/// it matches what this server would have produced: same model family, fp32,
/// same dimension, no non-finite components. `None` (no vector supplied)
/// always passes, since pushing one is optional and the server embeds instead.
///
/// The model tag and precision are **required** whenever a vector is present.
/// An untagged vector cannot be checked against what this server embeds with,
/// so accepting one would put a vector of unknown provenance next to the
/// server's own in the same index, where nothing downstream could tell them
/// apart. Refusing surfaces the mismatch to the caller instead of silently
/// re-embedding behind its back.
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
    Ok(())
}

/// Reject a `project_id` path parameter that is empty or unreasonably long.
/// Project ids are human slugs (e.g. `usercise/spelunk`), not UUIDs, so this
/// is a length/sanity cap rather than a UUID-format check.
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

/// Test-only override for the generation budget `llm_generate_with_timeout`
/// enforces (production uses `crate::REQUEST_TIMEOUT`). Lets tests inject a
/// millisecond-scale budget. `#[cfg(test)]`-gated, inert in the release binary.
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

/// Run an LLM backend's `generate` call with a wall-clock budget, so a hung/slow
/// backend can't hold the spawned generation task (and the SSE connection it
/// feeds) open forever.
///
/// `/llm/complete` returns its SSE `Response` as soon as the stream is built
/// and hands generation to a detached `tokio::spawn`, so the router-level
/// `TimeoutLayer` never sees this work. This wraps the generation call with the
/// same budget to close that gap without changing the SSE framing.
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
            // Dropping `tx`-holding future here closes the channel; the SSE
            // stream's `rx.recv()` loop sees `None` and ends the connection
            // (with whatever partial output was already sent).
        }
    }
}

/// Build the rate-limiter bucket key for an authenticated inference request:
/// `"<principal>|<client-ip>"`. Keying on IP as well as principal means a
/// shared team API key (a single `Principal::ApiKey` string, or the empty
/// string when no key is configured at all) doesn't collapse every distinct
/// client onto one shared bucket: each caller gets its own budget.
///
/// Both halves must be outside the caller's control or the budget is not a
/// budget. See [`crate::client_ip`] for why the address half comes from the TCP
/// peer rather than `X-Forwarded-For`.
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

/// Resolve the embedder for an embed-consuming handler, translating the slot's
/// readiness into the correct HTTP error when it is not `ready`:
/// - `loading`     → `503` + `Retry-After: 5` (transient: CLI keeps polling)
/// - `unavailable` → `503` (terminal: CLI stops polling, surfaces the error)
/// - `disabled`    → `400` (permanent misconfiguration for this request)
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
            // Log the real cause: a 503 here is the model still loading, not a
            // generic outage. Keeps the transient case out of error logs.
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

/// Embed memory-entry text for the storage routes (`add_note`,
/// `push_memory_batch`), which store text-only rather than failing when no
/// vector can be produced.
///
/// Two invariants live here rather than at each call site, where both have been
/// broken silently:
///
/// - **Never called with the `ServerDb` lock held.** That lock is global and the
///   embedder is serialized and slow, so an embed awaited under it stalls every
///   other request on the server — memory CRUD, `/memory/stream`'s poll loop and
///   liveness alike — until the whole batch finishes.
/// - **Runs under an [`crate::EmbedAdmission`] permit**, like every other
///   embed-consuming route, so a storage write cannot bypass the bound on how
///   many callers may wait on the embedder. The permit is released when this
///   returns.
///
/// The whole slice goes in one call: batching is both what keeps the lock-free
/// window short and what makes the embed itself cheaper.
pub(crate) async fn embed_for_storage(
    state: &AppState,
    texts: &[&str],
) -> Result<StorageEmbedding, AppError> {
    if texts.is_empty() {
        return Ok(StorageEmbedding::Vectors(Vec::new()));
    }
    // Only the *ready* backend embeds; loading/unavailable/disabled stores
    // text-only, since a memory write must not block on model warm-up.
    let Some(embedder) = state.embedder.backend() else {
        return Ok(StorageEmbedding::NotReady);
    };
    let _admission = state.embed_admission.try_acquire()?;
    match embedder.embed(texts).await {
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

/// Outcome of [`embed_for_storage`].
///
/// The two degraded arms are kept apart for [`crate::repair`], not for the
/// write paths: a write signals repair either way, but the repair pass must
/// stop on `NotReady` (nothing it retries can make progress) and fall back to
/// smaller units on `Failed` (one text in the page is poison and the rest are
/// still embeddable).
pub(crate) enum StorageEmbedding {
    /// One vector per input text, in input order.
    Vectors(Vec<Vec<f32>>),
    /// No backend is ready: loading, unavailable, or disabled.
    NotReady,
    /// The embedder answered with an error, or with a vector count that does
    /// not line up with the input. A count mismatch is a failure rather than a
    /// partial success on purpose: with the input-to-output mapping unknown,
    /// any assignment could attach a vector to the wrong text.
    Failed,
}

/// The one text a memory row is embedded from. Both write paths and the repair
/// pass call this, so a repaired row's vector cannot describe a differently
/// shaped string than the one the push path would have produced.
pub(crate) fn storage_embedding_text(title: &str, body: &str) -> String {
    format!("title: {title} | text: {body}")
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn require_project(db: &crate::db::ServerDb, slug: &str) -> Result<crate::db::Project, AppError> {
    validate_project_slug(slug)?;
    db.get_project(slug)?.ok_or(AppError::NotFound)
}
