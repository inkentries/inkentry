use serde::{Deserialize, Serialize};

use super::super::backend::Resolution;
use super::super::memory::{Candidate, Note, NoteId};

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
    // `"block"` asks the server to refuse the write on a non-empty duplicate
    // band with no `resolutions`. Sent only when the server advertises
    // `memory.reconcile` on `/v1/health`; an older server ignores an
    // unrecognised field and behaves exactly as before.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) reconcile: Option<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) resolutions: Vec<ResolutionWire>,
}

#[derive(Serialize)]
pub(super) struct ResolutionWire {
    #[serde(rename = "type")]
    pub(super) kind: &'static str,
    pub(super) id: String,
}

impl From<&Resolution> for ResolutionWire {
    fn from(r: &Resolution) -> Self {
        let (kind, id) = match r {
            Resolution::Supersedes(id) => ("supersedes", id),
            Resolution::RelatesTo(id) => ("relates_to", id),
            Resolution::Contradicts(id) => ("contradicts", id),
            Resolution::Distinct(id) => ("distinct", id),
        };
        Self {
            kind,
            id: id.to_string(),
        }
    }
}

#[derive(Deserialize)]
pub(super) struct AddNoteResponse {
    // Absent when `stored` is `false`: nothing was written, so there is no id
    // to report.
    #[serde(default)]
    pub(super) id: Option<NoteId>,
    // `true` unless the server refused the write under `reconcile: "block"`.
    // Defaults to `true` so a server predating this field, whose response
    // carries no `stored` field at all, reads exactly as it always has.
    #[serde(default = "default_true")]
    pub(super) stored: bool,
    #[serde(default)]
    pub(super) conflicts: Vec<ConflictInfo>,
    // The duplicate band, present on both a blocked and a stored response.
    #[serde(default)]
    pub(super) candidates: Vec<CandidateWire>,
    // The related band, present only alongside a stored entry.
    #[serde(default)]
    pub(super) related: Vec<CandidateWire>,
    // Server-assigned cross-machine id, if the server minted one. Absent on
    // older servers.
    #[serde(default)]
    pub(super) remote_id: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, Clone)]
pub(super) struct CandidateWire {
    pub(super) id: String,
    pub(super) kind: String,
    pub(super) title: String,
    pub(super) created_at: i64,
    #[serde(default)]
    pub(super) distance: Option<f64>,
    pub(super) band: String,
}

impl From<CandidateWire> for Candidate {
    fn from(w: CandidateWire) -> Self {
        use super::super::memory::CandidateBand;
        Candidate {
            id: w.id,
            kind: w.kind,
            title: w.title,
            created_at: w.created_at,
            distance: w.distance,
            band: if w.band == "duplicate" {
                CandidateBand::Duplicate
            } else {
                CandidateBand::Related
            },
        }
    }
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
