use serde::{Deserialize, Serialize};

use super::super::memory::{Note, NoteId};

#[derive(Serialize)]
pub(super) struct AddNoteRequest {
    pub(super) kind: String,
    pub(super) title: String,
    pub(super) body: String,
    pub(super) tags: Vec<String>,
    pub(super) linked_files: Vec<String>,
    // vector/vector_model/vector_precision travel together or not at all: the
    // accept side refuses a vector whose model and precision it can't check.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) vector: Option<Vec<f32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) vector_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) vector_precision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) source_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) valid_at: Option<i64>,
    // Flat fields, not a nested object, matching the rest of this request; a
    // server that predates them simply ignores the unknown fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) origin_actor_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) origin_tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) origin_model: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct AddNoteResponse {
    pub(super) id: NoteId,
    #[serde(default)]
    pub(super) conflicts: Vec<ConflictInfo>,
    // Server-assigned cross-machine id, if the server minted one. Absent on
    // older servers.
    #[serde(default)]
    pub(super) remote_id: Option<String>,
}

/// Conflict information returned by the server when a new note is semantically
/// close to an existing active entry (HTTP 409).
#[derive(Debug, Deserialize, Clone)]
pub struct ConflictInfo {
    pub id: NoteId,
    pub title: String,
    pub similarity: f32,
}

#[derive(Deserialize)]
pub(super) struct NoteResponse {
    pub(super) id: NoteId,
    pub(super) kind: String,
    pub(super) title: String,
    pub(super) body: String,
    pub(super) tags: Vec<String>,
    pub(super) linked_files: Vec<String>,
    pub(super) created_at: i64,
    pub(super) status: String,
    pub(super) superseded_by: Option<NoteId>,
    #[serde(default)]
    pub(super) source_ref: Option<String>,
    #[serde(default)]
    pub(super) valid_at: Option<i64>,
    #[serde(default)]
    pub(super) invalid_at: Option<i64>,
    // Canonical cross-machine id, if the server has one. Absent on older
    // servers.
    #[serde(default)]
    pub(super) remote_id: Option<String>,
    #[serde(default)]
    pub(super) distance: Option<f64>,
}

impl From<NoteResponse> for Note {
    fn from(r: NoteResponse) -> Self {
        let entity_id = crate::storage::entity_id(&r.kind, &r.title, &r.body);
        Note {
            id: r.id,
            entity_id,
            kind: r.kind,
            title: r.title,
            body: r.body,
            tags: r.tags,
            linked_files: r.linked_files,
            created_at: r.created_at,
            status: r.status,
            superseded_by: r.superseded_by,
            source_ref: r.source_ref,
            valid_at: r.valid_at,
            invalid_at: r.invalid_at,
            distance: r.distance,
            score: None,
            source_project: None,
            source_project_path: None,
            remote_id: r.remote_id,
            // The wire response doesn't carry origin back yet.
            origin: None,
        }
    }
}

// Tolerant reader for the `list` and `search` read endpoints. A newer team
// server wraps notes in `{ "entries": [...], "total": N }`; an older one still
// emits a bare `[...]` array. Accepting both keeps a newer CLI working against
// a team server running on its own upgrade schedule.
#[derive(Deserialize)]
#[serde(untagged)]
pub(super) enum NoteListPayload {
    Enveloped {
        #[serde(default)]
        entries: Vec<NoteResponse>,
    },
    Bare(Vec<NoteResponse>),
}

impl NoteListPayload {
    pub(super) fn into_notes(self) -> Vec<NoteResponse> {
        match self {
            NoteListPayload::Enveloped { entries } => entries,
            NoteListPayload::Bare(notes) => notes,
        }
    }
}

// Same tolerance as `NoteListPayload`: newer servers send `{ "shas": [...] }`,
// older ones a bare `["sha", ...]` array.
#[derive(Deserialize)]
#[serde(untagged)]
pub(super) enum HarvestedShasPayload {
    Enveloped {
        #[serde(default)]
        shas: Vec<String>,
    },
    Bare(Vec<String>),
}

impl HarvestedShasPayload {
    pub(super) fn into_shas(self) -> Vec<String> {
        match self {
            HarvestedShasPayload::Enveloped { shas } => shas,
            HarvestedShasPayload::Bare(shas) => shas,
        }
    }
}

#[derive(Serialize)]
pub(super) struct SearchRequest {
    pub(super) query: String,
    pub(super) limit: usize,
}

#[derive(Serialize)]
pub(super) struct SupersedeRequest {
    pub(super) new_id: NoteId,
}

#[derive(Deserialize)]
pub(super) struct BoolResponse {
    pub(super) changed: bool,
}

#[derive(Deserialize)]
pub(super) struct CountResponse {
    pub(super) count: i64,
}
