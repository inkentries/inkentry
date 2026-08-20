use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{AppError, AppState, ErrorBody};

use super::{
    MAX_BATCH_ENTRIES, StorageEmbedding, embed_for_storage, storage_embedding_text,
    validate_project_slug, validate_pushed_vector, validate_title_body,
};

// ── Batch push (wire parity with cloud-api's POST /memory/batch) ────────────

/// One entry in a `POST /memory/batch` request. Field-for-field match of the
/// CLI's `BatchPushItem` (`inkentry-core/src/storage/remote/sync.rs`) and of
/// the hosted peer's batch item, so one client payload means the same thing
/// wherever it is sent.
#[derive(Deserialize, ToSchema)]
pub struct BatchNoteItem {
    pub kind: String,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    /// Stable cross-machine identity: the server's idempotency key. Stored as
    /// `notes.remote_id` (unique). A re-push of the same `external_id` against
    /// a live note is skipped, not duplicated.
    pub external_id: String,
    /// Git SHA provenance, if any. No dedicated column on this schema; stored
    /// as a `git:<sha>` tag, the same convention `harvested_shas` already reads.
    #[serde(default)]
    pub source_commit: Option<String>,
    /// Locally-computed embedding, letting the client skip server-side
    /// embedding. Optional; without it the server embeds the text itself.
    #[serde(default)]
    pub vector: Option<Vec<f32>>,
    /// Model tag for a pushed `vector`. Required whenever `vector` is present.
    #[serde(default)]
    pub vector_model: Option<String>,
    /// Precision of a pushed `vector`; must be `fp32`. Required whenever
    /// `vector` is present.
    #[serde(default)]
    pub vector_precision: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct BatchPushRequest {
    pub entries: Vec<BatchNoteItem>,
}

/// Per-entry outcome in the batch response.
#[derive(Serialize, ToSchema)]
pub struct BatchItemResult {
    /// `"created"` or `"skipped"` (idempotent re-push).
    pub status: &'static str,
    pub external_id: String,
    /// The note's identity, the same UUIDv7 every other route carries.
    /// Present for `"created"` and also for a `"skipped"` dedupe-hit (the
    /// already-existing id), so a caller that lost track of an earlier create
    /// can still recover the id from a plain re-push.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Whether the stored row this result refers to is in the vector index
    /// **as of this response**. Present on every result, `"created"` and
    /// `"skipped"` alike, and always describing the stored row rather than the
    /// request: a dedupe-hit reports the state of the row already on the
    /// server, not of the payload that was skipped.
    ///
    /// `false` does not mean "never will be". A row stored without a vector
    /// raises a repair signal as it is written, so a background pass may
    /// already be embedding it by the time this response is read.
    pub embedded: bool,
}

/// Response body for `POST /memory/batch`; always `207 Multi-Status`.
#[derive(Serialize, ToSchema)]
pub struct BatchPushResponse {
    pub created: u32,
    pub skipped: u32,
    pub failed: u32,
    pub results: Vec<BatchItemResult>,
}

/// Batch-create memory entries. Idempotent on `external_id`: a live note
/// already carrying the given `external_id` is skipped, not duplicated.
///
/// Wire-compatible with the CLI's `BatchPushItem`/`CloudSyncClient::push_batch`
/// (the same client cloud-api's `/memory/batch` serves); `inkentry plumbing push`
/// and `inkentry sync` target this route on an OSS team server exactly as they
/// do against cloud-api. Always returns **207** with a per-entry result list;
/// a request-level validation failure (oversized batch, a title/body over the
/// configured caps, or an injection match) rejects the whole batch (4xx/422)
/// with nothing stored, before any entry is written. A batch needing
/// server-side embedding is shed with **429** when the embed admission queue is
/// full, on the same terms as every other embed-consuming route.
#[utoipa::path(
    post,
    path = "/v1/projects/{project_id}/memory/batch",
    params(
        ("project_id" = String, Path, description = "Project slug (e.g. `usercise/inkentry`)")
    ),
    request_body = BatchPushRequest,
    responses(
        (status = 207, description = "Per-entry outcomes", body = BatchPushResponse),
        (status = 400, description = "Bad request (oversized batch, bad field length)", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 422, description = "Entry rejected: prompt injection detected"),
        (status = 429, description = "Embed admission queue full; retry after the given delay", body = ErrorBody),
    ),
    security(("bearer_auth" = [])),
    tag = "memory"
)]
pub async fn push_memory_batch(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    Json(body): Json<BatchPushRequest>,
) -> Result<Response, AppError> {
    validate_project_slug(&project_id)?;

    if body.entries.len() > MAX_BATCH_ENTRIES {
        return Err(AppError::BadRequest(format!(
            "batch_too_large: maximum {MAX_BATCH_ENTRIES} entries per request (got {})",
            body.entries.len()
        )));
    }

    // ── Whole-batch validation up front: nothing is stored unless every entry
    // passes (mirrors cloud-api's batch; see routes/memory.rs). ────────────
    let configured_dim = state.db.lock().await.embedding_dim;
    for (i, entry) in body.entries.iter().enumerate() {
        validate_title_body(&entry.title, entry.body.as_deref().unwrap_or(""))
            .map_err(|e| prefix_batch_error(e, i))?;
        validate_pushed_vector(
            entry.vector.as_deref(),
            entry.vector_model.as_deref(),
            entry.vector_precision.as_deref(),
            configured_dim,
        )
        .map_err(|e| prefix_batch_error(e, i))?;
        if entry.external_id.is_empty() {
            return Err(AppError::BadRequest(format!(
                "entry {i}: external_id must not be empty"
            )));
        }
        if let Some(m) =
            crate::security::scan_for_injection(&entry.title, entry.body.as_deref().unwrap_or(""))
        {
            tracing::warn!(
                "batch entry rejected: injection pattern matched (project={project_id}, entry={i}, field={}, category={})",
                m.field,
                m.category,
            );
            return Ok((
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({
                    "error": "injection_detected",
                    "entry": i,
                    "field": m.field,
                    "category": m.category,
                    "message": "Entry contains patterns associated with prompt injection. \
                                Review and revise the entry.",
                })),
            )
                .into_response());
        }
    }

    // Embed every entry that needs a server-side vector in one batched call,
    // before the DB lock is taken — a routine `sync` push is 50 entries, and
    // embedding them one at a time under the global lock stalls every other
    // request on the server for the whole batch. See `embed_for_storage`.
    //
    // Entries that turn out to be dedupe-skips are embedded too and their
    // vectors dropped: which ones those are is only knowable from the DB, and
    // one batched embed off the lock beats N serialized embeds under it.
    let pending: Vec<usize> = body
        .entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.vector.is_none())
        .map(|(i, _)| i)
        .collect();
    let texts: Vec<String> = pending
        .iter()
        .map(|&i| {
            let entry = &body.entries[i];
            storage_embedding_text(&entry.title, entry.body.as_deref().unwrap_or(""))
        })
        .collect();
    let text_refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let mut server_embeddings: Vec<Option<Vec<f32>>> = vec![None; body.entries.len()];
    if let StorageEmbedding::Vectors(vectors) = embed_for_storage(&state, &text_refs).await? {
        for (&i, vector) in pending.iter().zip(vectors) {
            server_embeddings[i] = Some(vector);
        }
    }

    let db = state.db.lock().await;
    // Register the project once against the server's own configured dim/model
    // (not a per-entry value: entries needing server-side embedding don't
    // know their dim until embedded, and mixing per-entry 0/N would race the
    // dimension guard). Matches the single vec0 table, which is fixed-dim for
    // the whole server regardless of project.
    let model = db.embedding_model.clone();
    let project = db.upsert_project(&project_id, configured_dim, &model)?;

    let ext_ids: Vec<String> = body.entries.iter().map(|e| e.external_id.clone()).collect();
    // `mut`: an external_id repeated WITHIN this batch (not just across
    // requests) must also be treated as idempotent. Without updating this map
    // as entries are created below, a second occurrence of the same
    // external_id in one request would attempt a second INSERT and hit the
    // unique index (`remote_id` is unique per project), 500ing the whole
    // batch after the first occurrence already committed.
    let mut existing = db.find_by_remote_ids(project.id, &ext_ids)?;

    let mut results = Vec::with_capacity(body.entries.len());
    let mut created = 0u32;
    let mut skipped = 0u32;
    // Set by any result reporting `embedded: false`, whether it is a row this
    // request stored without a vector or a dedupe-hit on one stored earlier.
    // A dedupe hit writes nothing, but it is a free look at a row nothing else
    // was ever going to re-examine: the client's outbox believes that entry
    // landed, so a re-push is the last chance to notice.
    let mut needs_repair = false;

    for (i, entry) in body.entries.iter().enumerate() {
        if let Some(existing_note) = existing.get(&entry.external_id) {
            // Carry the already-assigned id even on a dedupe-skip: a caller
            // that lost track of a prior "created" ack (e.g. a local write
            // failure between receiving the ack and stamping it) must be able
            // to recover the id from a plain re-push, not just the original
            // create.
            results.push(BatchItemResult {
                status: "skipped",
                external_id: entry.external_id.clone(),
                id: Some(existing_note.sync_id.clone()),
                embedded: existing_note.has_embedding,
            });
            needs_repair |= !existing_note.has_embedding;
            skipped += 1;
            continue;
        }

        let embedding = entry.vector.as_deref().or(server_embeddings[i].as_deref());

        // `source_commit` has no dedicated column on this schema; fold it into
        // tags using the same `git:<sha>` convention `harvested_shas` reads.
        let tags: Vec<String> = entry
            .source_commit
            .as_deref()
            .map(|sha| vec![format!("git:{sha}")])
            .unwrap_or_default();

        let (_rowid, note_id) = db.add_note(
            project.id,
            &entry.kind,
            &entry.title,
            entry.body.as_deref().unwrap_or(""),
            &tags,
            &[],
            embedding,
            Some(&entry.external_id),
        )?;
        // Record it immediately so a later entry in this same batch sharing
        // the external_id is skipped instead of re-inserted (see the `mut`
        // comment on `existing` above).
        existing.insert(
            entry.external_id.clone(),
            crate::db::ExistingNote {
                sync_id: note_id.clone(),
                has_embedding: embedding.is_some(),
            },
        );
        results.push(BatchItemResult {
            status: "created",
            external_id: entry.external_id.clone(),
            id: Some(note_id),
            embedded: embedding.is_some(),
        });
        needs_repair |= embedding.is_none();
        created += 1;
    }
    drop(db);

    if needs_repair {
        state.repair_signal.raise();
    }

    Ok((
        StatusCode::MULTI_STATUS,
        Json(BatchPushResponse {
            created,
            skipped,
            failed: 0,
            results,
        }),
    )
        .into_response())
}

/// Prefix a validation `AppError::BadRequest` message with the failing entry's
/// index, matching cloud-api's `"entry {i}: ..."` batch-error convention.
fn prefix_batch_error(err: AppError, i: usize) -> AppError {
    match err {
        AppError::BadRequest(msg) => AppError::BadRequest(format!("entry {i}: {msg}")),
        other => other,
    }
}
