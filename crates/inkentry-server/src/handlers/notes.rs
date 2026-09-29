use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{AppError, AppState, ErrorBody};

use super::{
    StorageEmbedding, embed_for_storage, require_embedder, require_project, storage_embedding_text,
    validate_project_slug, validate_pushed_vector, validate_title_body,
};

#[derive(Deserialize, ToSchema)]
pub struct AddNoteRequest {
    /// Kind of memory entry: `decision`, `requirement`, `note`, `question`, `handoff`, `intent`.
    pub kind: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    /// Optional tags for filtering.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Source file paths this entry is linked to.
    #[serde(default)]
    pub linked_files: Vec<String>,
    /// Locally-computed embedding vector from the client. Optional: if omitted
    /// and the server's embedder is ready, the server embeds the entry.
    /// If neither is available, the entry is stored without a vector (text
    /// search only).
    ///
    /// A supplied vector must be L2-normalised: its magnitude must fall inside
    /// `[0.5, 1.5]`. The index ranks by Euclidean distance, which tracks
    /// similarity only across vectors of equal length, so one of another
    /// magnitude is refused rather than stored and silently mis-ranked.
    pub vector: Option<Vec<f32>>,
    /// Model tag for a pushed `vector`. Required whenever `vector` is present.
    pub vector_model: Option<String>,
    /// Precision of a pushed `vector`; must be `fp32`. Required whenever
    /// `vector` is present.
    pub vector_precision: Option<String>,
    /// `"block"` refuses the write when the pre-store candidate pool's
    /// duplicate band is non-empty and `resolutions` is empty. Any other
    /// value (including absent) stores unconditionally. Only meaningful when
    /// the client also sees `memory.reconcile` on `GET /v1/health`; a server
    /// that predates this field simply ignores it.
    #[serde(default)]
    pub reconcile: Option<String>,
    /// One resolution per blocking candidate, applied in the same
    /// transaction as the write. `type` is one of `supersedes`,
    /// `relates_to`, `contradicts`, `distinct`; `distinct` records nothing.
    /// An id naming an entry outside the reported candidate set is accepted,
    /// as long as it resolves to a real active-or-archived entry in the
    /// project.
    #[serde(default)]
    pub resolutions: Vec<ResolutionRequest>,
}

/// One element of [`AddNoteRequest::resolutions`].
#[derive(Deserialize, ToSchema)]
pub struct ResolutionRequest {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
}

#[derive(Serialize, ToSchema)]
pub struct AddNoteResponse {
    /// Whether the note was stored. `false` only on a 409 refused under
    /// `reconcile: "block"` — every other status this route returns means
    /// the entry was written.
    pub stored: bool,
    /// Identity of the created note: a UUIDv7 minted by this server. Absent
    /// when `stored` is `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Always empty from this build. Kept on the wire, never populated, so
    /// an older client reading it sees no conflicts rather than a missing
    /// field.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<ConflictEntry>,
    /// The duplicate band, computed before the write. Present on both a
    /// refused (`stored: false`) and a stored response.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<CandidateEntry>,
    /// The related band. Present only alongside a stored entry.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub related: Vec<CandidateEntry>,
}

/// A single conflicting memory entry returned in a 409 response. Never
/// populated; the type is kept only so `AddNoteResponse::conflicts`'s wire
/// shape doesn't break for a client still reading it.
#[derive(Serialize, ToSchema)]
pub struct ConflictEntry {
    pub id: String,
    pub title: String,
    /// Cosine similarity to the new entry (0.0–1.0).
    pub similarity: f32,
}

/// One pre-write candidate, as reported over the wire.
#[derive(Serialize, ToSchema)]
pub struct CandidateEntry {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance: Option<f64>,
    /// `"duplicate"` or `"related"`.
    pub band: String,
}

impl From<inkentry_core::storage::Candidate> for CandidateEntry {
    fn from(c: inkentry_core::storage::Candidate) -> Self {
        Self {
            id: c.id,
            kind: c.kind,
            title: c.title,
            created_at: c.created_at,
            distance: c.distance,
            band: match c.band {
                inkentry_core::storage::CandidateBand::Duplicate => "duplicate",
                inkentry_core::storage::CandidateBand::Related => "related",
            }
            .to_string(),
        }
    }
}

#[derive(Deserialize, ToSchema, utoipa::IntoParams)]
pub struct ListQuery {
    /// Filter by kind (`decision`, `requirement`, `note`, `question`, `handoff`, `intent`).
    pub kind: Option<String>,
    /// Maximum number of results to return (default: 20).
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Include archived entries (default: false).
    #[serde(default)]
    pub archived: bool,
    /// Number of entries to skip before this page, for walking a store larger
    /// than one page (default: 0).
    #[serde(default)]
    pub offset: usize,
}
fn default_limit() -> usize {
    20
}

#[derive(Deserialize, ToSchema)]
pub struct SearchRequest {
    /// Text query: the server encodes this using its configured embedder.
    pub query: String,
    /// Maximum number of results to return (default: 20).
    #[serde(default = "default_limit")]
    pub limit: usize,
}

#[derive(Serialize, ToSchema)]
pub struct BoolResponse {
    /// Whether the operation modified a record.
    pub changed: bool,
}

#[derive(Serialize, ToSchema)]
pub struct CountResponse {
    pub count: i64,
}

/// Object envelope for the list and search read endpoints.
///
/// A JSON response root must be an object, never a bare array. `entries`
/// carries the notes; `total` is their count in this response, which is
/// already `limit`-capped, not a project-wide count (that has its own
/// `/stats` route).
#[derive(Serialize, ToSchema)]
pub struct NoteListResponse {
    pub entries: Vec<crate::db::ServerNote>,
    pub total: usize,
}

#[derive(Deserialize, ToSchema)]
pub struct SupersedeRequest {
    /// Identity of the new note that replaces the superseded one.
    pub new_id: String,
}

/// An entry can sync before a commit claims it; this carries the anchor once
/// it exists.
#[derive(Deserialize, ToSchema)]
pub struct AnchorUpdateRequest {
    /// The commit sha the client resolved `source_ref` to.
    pub source_ref: String,
}

/// Add a memory entry to a project. The project is auto-created on first write.
///
/// The `vector` field is optional. If omitted and the server's embedder is ready,
/// the server embeds the entry before storage. If neither is available, the
/// entry is stored without a vector (text search only, no KNN). A `vector` must
/// arrive with its `vector_model` and `vector_precision`, or it is refused.
///
/// Candidates are computed before the write: active entries within the
/// duplicate or related distance band. Returns **201** and stores
/// the entry unless `reconcile: "block"` is set and the duplicate band is
/// non-empty with no `resolutions` supplied, in which case it returns **409**
/// with `stored: false` and nothing is written. `resolutions` (`supersedes`,
/// `relates_to`, `contradicts`, `distinct`) are applied in the same
/// transaction as a stored write.
/// Returns **422** when the entry contains prompt-injection patterns.
/// Returns **429** (with `Retry-After`) when the entry needs server-side
/// embedding and the interactive embed admission lane is full.
#[utoipa::path(
    post,
    path = "/v1/projects/{project_id}/memory",
    params(
        ("project_id" = String, Path, description = "Project slug (e.g. `inkentries/inkentry`)")
    ),
    request_body = AddNoteRequest,
    responses(
        (status = 201, description = "Note created", body = AddNoteResponse),
        (status = 400, description = "Invalid request, or a pushed vector that violates the contract: wrong model tag, wrong precision, wrong dimension, a non-finite component, or an L2 magnitude outside [0.5, 1.5]", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 409, description = "Refused: reconcile: \"block\" and an unresolved duplicate-band candidate (stored: false)", body = AddNoteResponse),
        (status = 422, description = "Entry rejected: prompt injection detected"),
        (status = 429, description = "Interactive embed admission lane full; retry after the given delay", body = ErrorBody),
    ),
    security(("bearer_auth" = [])),
    tag = "memory"
)]
pub async fn add_note(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    Json(body): Json<AddNoteRequest>,
) -> Result<Response, AppError> {
    validate_project_slug(&project_id)?;
    validate_title_body(&body.title, &body.body)?;
    {
        let configured_dim = state.db.lock().await.embedding_dim;
        validate_pushed_vector(
            body.vector.as_deref(),
            body.vector_model.as_deref(),
            body.vector_precision.as_deref(),
            configured_dim,
        )?;
    }

    // Reject entries that contain prompt-injection patterns.
    if let Some(m) = crate::security::scan_for_injection(&body.title, &body.body) {
        // Audit only non-sensitive locators; never echo the matched text.
        tracing::warn!(
            "note rejected: injection pattern matched (project={project_id}, field={}, category={}, title_len={}, body_len={})",
            m.field,
            m.category,
            body.title.len(),
            body.body.len()
        );
        return Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "injection_detected",
                "field": m.field,
                "category": m.category,
                "message": "Entry contains patterns associated with prompt injection. \
                            Review and revise the entry.",
            })),
        )
            .into_response());
    }

    // Embed before the DB lock is taken, under an admission permit: the
    // candidate pool below is computed against this same vector.
    let server_embedding: Option<Vec<f32>> = if body.vector.is_none() {
        let text = storage_embedding_text(&body.title, &body.body);
        // `add_note` is a person waiting on their own write: interactive lane.
        match embed_for_storage(&state, &[text.as_str()], crate::EmbedLane::Interactive).await? {
            StorageEmbedding::Vectors(mut vectors) => vectors.pop(),
            StorageEmbedding::NotReady | StorageEmbedding::Failed => None,
        }
    } else {
        None
    };

    let embedding = body.vector.as_deref().or(server_embedding.as_deref());
    let dim = embedding.map(|v| v.len()).unwrap_or(0);

    let db = state.db.lock().await;
    let model = db.embedding_model.clone();
    let project = db.upsert_project(&project_id, dim, &model)?;

    // No FTS half exists on this schema, so with no embedding this is empty
    // and the write proceeds unconditionally either way.
    let candidates = db.find_candidates(project.id, embedding, None)?;
    let (duplicate, related): (Vec<_>, Vec<_>) = candidates
        .into_iter()
        .partition(|c| matches!(c.band, inkentry_core::storage::CandidateBand::Duplicate));

    // Refuse rather than store, only under an explicit opt-in and only with
    // nothing to resolve the duplicate. Nothing is written here.
    if body.reconcile.as_deref() == Some("block")
        && !duplicate.is_empty()
        && body.resolutions.is_empty()
    {
        return Ok((
            StatusCode::CONFLICT,
            Json(AddNoteResponse {
                stored: false,
                id: None,
                conflicts: vec![],
                candidates: duplicate.into_iter().map(Into::into).collect(),
                related: related.into_iter().map(Into::into).collect(),
            }),
        )
            .into_response());
    }

    let resolutions: Vec<(String, String)> = body
        .resolutions
        .iter()
        .map(|r| (r.kind.clone(), r.id.clone()))
        .collect();
    let (rowid, note_id) = db.add_note_with_resolutions(
        project.id,
        &body.kind,
        &body.title,
        &body.body,
        &body.tags,
        &body.linked_files,
        embedding,
        &resolutions,
    )?;

    // This route stores text-only rather than failing, exactly as the batch
    // route does, so it leaves the same repairable rows behind and owes the
    // same signal. It does not yet report the state on the wire.
    //
    // Raised after the insert, never before it. The sweep contends for the
    // very lock this write holds, so a signal raised earlier can wake a
    // worker that takes the lock first, finds an empty backlog, and parks,
    // leaving this row stored with nothing pending and no edge to bring
    // anyone back.
    if embedding.is_none() {
        state.repair_signal.raise();
    }
    let _ = rowid;

    Ok((
        StatusCode::CREATED,
        Json(AddNoteResponse {
            stored: true,
            id: Some(note_id),
            conflicts: vec![],
            candidates: duplicate.into_iter().map(Into::into).collect(),
            related: related.into_iter().map(Into::into).collect(),
        }),
    )
        .into_response())
}

/// List memory entries for a project, optionally filtered by kind.
#[utoipa::path(
    get,
    path = "/v1/projects/{project_id}/memory",
    params(
        ("project_id" = String, Path, description = "Project slug"),
        ListQuery,
    ),
    responses(
        (status = 200, description = "List of notes", body = NoteListResponse),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Project not found", body = ErrorBody),
    ),
    security(("bearer_auth" = [])),
    tag = "memory"
)]
pub async fn list_notes(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    Query(params): Query<ListQuery>,
) -> Result<impl IntoResponse, AppError> {
    let db = state.db.lock().await;
    let project = require_project(&db, &project_id)?;
    let notes = db.list_notes(
        project.id,
        params.kind.as_deref(),
        params.limit,
        params.archived,
        params.offset,
    )?;
    let total = notes.len();
    Ok(Json(NoteListResponse {
        entries: notes,
        total,
    }))
}

/// Get a single memory entry by ID.
#[utoipa::path(
    get,
    path = "/v1/projects/{project_id}/memory/{note_id}",
    params(
        ("project_id" = String, Path, description = "Project slug"),
        ("note_id" = String, Path, description = "Note identity (UUIDv7)"),
    ),
    responses(
        (status = 200, description = "Note found", body = crate::db::ServerNote),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Note not found", body = ErrorBody),
    ),
    security(("bearer_auth" = [])),
    tag = "memory"
)]
pub async fn get_note(
    State(state): State<AppState>,
    Path((project_id, note_id)): Path<(String, String)>,
) -> Result<impl IntoResponse, AppError> {
    let db = state.db.lock().await;
    let project = require_project(&db, &project_id)?;
    match db.get_note(project.id, &note_id)? {
        Some(note) => Ok(Json(note).into_response()),
        None => Err(AppError::NotFound),
    }
}

/// Semantic search over memory entries. The server encodes the text query using its
/// configured embedder. Returns 400 if no embedder is configured.
#[utoipa::path(
    post,
    path = "/v1/projects/{project_id}/memory/search",
    params(
        ("project_id" = String, Path, description = "Project slug"),
    ),
    request_body = SearchRequest,
    responses(
        (status = 200, description = "Nearest neighbours", body = NoteListResponse),
        (status = 400, description = "No embedder configured", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Project not found", body = ErrorBody),
        (status = 429, description = "Interactive embed admission lane full; retry after the given delay", body = ErrorBody),
    ),
    security(("bearer_auth" = [])),
    tag = "memory"
)]
pub async fn search_notes(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    Json(body): Json<SearchRequest>,
) -> Result<impl IntoResponse, AppError> {
    let embedder = require_embedder(
        &state,
        "This server has no embedder configured. Semantic memory search is unavailable.",
    )?;

    // A memory search query is interactive: it takes the reserved lane and is
    // never shed nor left waiting behind a bulk index batch; a full
    // interactive lane still sheds with 429 rather than queuing silently.
    let _admission = state
        .embed_admission
        .try_acquire(crate::EmbedLane::Interactive)?;

    // F2LLM QA query prefix: matches the instruction format used for memory documents.
    let query_text = format!(
        "Instruct: Given a question, retrieve passages that answer the question\nQuery: {}",
        body.query
    );
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let query_vecs = embedder
        .embed_lane(
            &[query_text.as_str()],
            cancel,
            crate::EmbedLane::Interactive,
        )
        .await
        .map_err(AppError::Internal)?;
    let query_vec = query_vecs
        .into_iter()
        .next()
        .ok_or_else(|| AppError::BadRequest("Embedder returned no vectors".to_string()))?;

    let db = state.db.lock().await;
    let project = require_project(&db, &project_id)?;
    let notes = db.search_notes(project.id, &query_vec, body.limit)?;
    let total = notes.len();
    Ok(Json(NoteListResponse {
        entries: notes,
        total,
    }))
}

/// Delete a memory entry permanently.
#[utoipa::path(
    delete,
    path = "/v1/projects/{project_id}/memory/{note_id}",
    params(
        ("project_id" = String, Path, description = "Project slug"),
        ("note_id" = String, Path, description = "Note identity (UUIDv7)"),
    ),
    responses(
        (status = 200, description = "Deletion result", body = BoolResponse),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Note not found", body = ErrorBody),
    ),
    security(("bearer_auth" = [])),
    tag = "memory"
)]
pub async fn delete_note(
    State(state): State<AppState>,
    Path((project_id, note_id)): Path<(String, String)>,
) -> Result<impl IntoResponse, AppError> {
    let db = state.db.lock().await;
    let project = require_project(&db, &project_id)?;
    let changed = db.delete_note(project.id, &note_id)?;
    Ok(Json(BoolResponse { changed }))
}

/// Archive a memory entry. Archived entries are excluded from search and `ask`
/// context but remain visible via `?archived=true`.
#[utoipa::path(
    post,
    path = "/v1/projects/{project_id}/memory/{note_id}/archive",
    params(
        ("project_id" = String, Path, description = "Project slug"),
        ("note_id" = String, Path, description = "Note identity (UUIDv7)"),
    ),
    responses(
        (status = 200, description = "Archive result", body = BoolResponse),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Note not found", body = ErrorBody),
    ),
    security(("bearer_auth" = [])),
    tag = "memory"
)]
pub async fn archive_note(
    State(state): State<AppState>,
    Path((project_id, note_id)): Path<(String, String)>,
) -> Result<impl IntoResponse, AppError> {
    let db = state.db.lock().await;
    let project = require_project(&db, &project_id)?;
    let changed = db.archive_note(project.id, &note_id)?;
    Ok(Json(BoolResponse { changed }))
}

/// Mark a memory entry as superseded by a newer one. The old entry is archived
/// and linked to the new one.
#[utoipa::path(
    post,
    path = "/v1/projects/{project_id}/memory/{note_id}/supersede",
    params(
        ("project_id" = String, Path, description = "Project slug"),
        ("note_id" = String, Path, description = "Identity of the note to supersede (UUIDv7)"),
    ),
    request_body = SupersedeRequest,
    responses(
        (status = 200, description = "Supersede result", body = BoolResponse),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Note not found", body = ErrorBody),
    ),
    security(("bearer_auth" = [])),
    tag = "memory"
)]
pub async fn supersede_note(
    State(state): State<AppState>,
    Path((project_id, note_id)): Path<(String, String)>,
    Json(body): Json<SupersedeRequest>,
) -> Result<impl IntoResponse, AppError> {
    let db = state.db.lock().await;
    let project = require_project(&db, &project_id)?;
    let changed = db.supersede_note(project.id, &note_id, &body.new_id)?;
    Ok(Json(BoolResponse { changed }))
}

/// Set `source_ref` on an already-synced entry once a commit claims it
/// locally. Idempotent and unconditional (unlike `archive`/
/// `supersede`, which guard on `status = 'active'`): the client has no local
/// record of whether a previous push already delivered this, so it resends
/// on every push/sync, and a repeat must not error.
#[utoipa::path(
    post,
    path = "/v1/projects/{project_id}/memory/{note_id}/anchor",
    params(
        ("project_id" = String, Path, description = "Project slug"),
        ("note_id" = String, Path, description = "Note identity (UUIDv7)"),
    ),
    request_body = AnchorUpdateRequest,
    responses(
        (status = 200, description = "Anchor result", body = BoolResponse),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Note not found", body = ErrorBody),
    ),
    security(("bearer_auth" = [])),
    tag = "memory"
)]
pub async fn update_note_anchor(
    State(state): State<AppState>,
    Path((project_id, note_id)): Path<(String, String)>,
    Json(body): Json<AnchorUpdateRequest>,
) -> Result<impl IntoResponse, AppError> {
    let db = state.db.lock().await;
    let project = require_project(&db, &project_id)?;
    let changed = db.set_note_source_ref(project.id, &note_id, &body.source_ref)?;
    Ok(Json(BoolResponse { changed }))
}
