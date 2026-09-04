use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use super::entity_id::entity_id;
use super::memory::{Note, NoteId};

/// Serialised form stored as JSON in a memory backend (git-notes blob or SQLite).
///
/// `schema_version` 0 = legacy (field absent in old blobs), 1 = current.
#[derive(Debug, Serialize, Deserialize)]
pub struct NoteRecord {
    /// Absent in legacy blobs — treated as version 0 via `#[serde(default)]`.
    #[serde(default)]
    pub schema_version: u8,
    /// Machine-local SQLite rowid. NOT an identity: it renumbers on re-`init`
    /// and is assigned independently per machine. Kept for backward
    /// compatibility only — use `resolve_entity_id()` to identify an entry.
    pub id: i64,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    pub linked_files: Vec<String>,
    pub created_at: i64,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invalid_at: Option<i64>,
    /// Machine-local rowid of the successor. Not portable — see `id`. Prefer
    /// `superseded_by_entity_id`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<i64>,
    /// Canonical cross-machine id (uuid), set on sync to a remote server.
    /// Optional and additive: absent on the wire means `None`; an old blob
    /// without this key reads as `None`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub remote_id: Option<String>,
    /// Content-addressed canonical identity. Optional only because legacy blobs
    /// predate it; a reader recovers it with `resolve_entity_id()`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub entity_id: Option<String>,
    /// Portable supersede edge: the successor's `entity_id`. Survives a rowid
    /// renumber and resolves on any machine.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub superseded_by_entity_id: Option<String>,
    /// This entry's outgoing `relates_to` and `contradicts` edges, each naming
    /// its target by `entity_id`. Outgoing only: `memory_edges` is directed, so
    /// carrying each edge once from its source reconstructs the table exactly,
    /// and a second copy on the target would be a second place to disagree.
    /// `supersedes` never appears here; it stays on `superseded_by_entity_id`.
    /// Additive under `schema_version` 1: an older reader ignores the key.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub edges: Vec<CarriedEdge>,
}

/// One outgoing graph edge as the carrier records it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CarriedEdge {
    /// `relates_to` or `contradicts`.
    pub kind: String,
    /// The target entry's `entity_id`, resolved to a local row at import.
    pub to_entity_id: String,
}

impl CarriedEdge {
    pub fn new(kind: &str, to_entity_id: String) -> Self {
        Self {
            kind: kind.to_string(),
            to_entity_id,
        }
    }
}

impl NoteRecord {
    /// The record's canonical identity: the stored `entity_id`, or recomputed
    /// from `{kind, title, body}` when absent (legacy blob).
    pub fn resolve_entity_id(&self) -> String {
        self.entity_id
            .clone()
            .unwrap_or_else(|| entity_id(&self.kind, &self.title, &self.body))
    }
}

/// The git-notes carrier's record id, as the opaque token it is.
///
/// ADR-059 froze the carrier format, and its `id` field is an integer that the
/// carrier itself documents as non-identity. It is rendered rather than
/// reinterpreted: minting a UUID here would produce a different one on every
/// read, since the carrier has nowhere to persist it.
pub fn carrier_token(id: i64) -> NoteId {
    NoteId::from_str(&id.to_string())
        .unwrap_or_else(|e| unreachable!("an integer never renders as an empty token: {e}"))
}

pub fn record_to_note(r: NoteRecord) -> Note {
    let entity_id = super::entity_id::entity_id(&r.kind, &r.title, &r.body);
    Note {
        id: carrier_token(r.id),
        entity_id,
        kind: r.kind,
        title: r.title,
        body: r.body,
        tags: r.tags,
        linked_files: r.linked_files,
        created_at: r.created_at,
        status: r.status,
        superseded_by: r.superseded_by.map(carrier_token),
        source_ref: r.source_ref,
        valid_at: r.valid_at,
        invalid_at: r.invalid_at,
        distance: None,
        score: None,
        source_project: None,
        source_project_path: None,
        remote_id: r.remote_id,
    }
}

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_record() -> NoteRecord {
        NoteRecord {
            schema_version: 1,
            id: 42,
            kind: "decision".to_string(),
            title: "t".to_string(),
            body: "b".to_string(),
            tags: vec![],
            linked_files: vec![],
            created_at: 100,
            status: "active".to_string(),
            source_ref: None,
            valid_at: None,
            invalid_at: None,
            superseded_by: None,
            remote_id: None,
            entity_id: None,
            superseded_by_entity_id: None,
            edges: vec![],
        }
    }

    /// (d) A record with a `remote_id` serializes the key and round-trips.
    #[test]
    fn note_record_round_trips_with_remote_id() {
        let mut rec = base_record();
        rec.remote_id = Some("11111111-1111-7111-8111-111111111111".to_string());

        let json = serde_json::to_string(&rec).expect("serialize");
        assert!(json.contains("\"remote_id\""), "key present when Some");

        let back: NoteRecord = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.remote_id, rec.remote_id);
    }

    /// (d) A record without a `remote_id` omits the key, and an old blob that
    /// never had the key still deserializes (reads as `None`).
    #[test]
    fn note_record_round_trips_without_remote_id() {
        let rec = base_record();
        let json = serde_json::to_string(&rec).expect("serialize");
        assert!(!json.contains("remote_id"), "key omitted when None: {json}");

        // Old blob shape: no remote_id key at all.
        let old = r#"{"schema_version":1,"id":7,"kind":"note","title":"t","body":"b","tags":[],"linked_files":[],"created_at":1,"status":"active"}"#;
        let back: NoteRecord = serde_json::from_str(old).expect("deserialize old blob");
        assert_eq!(back.remote_id, None, "absent key reads as None");
        assert_eq!(back.id, 7);
    }

    /// A record carrying both identity fields round-trips.
    #[test]
    fn note_record_round_trips_with_entity_id() {
        let mut rec = base_record();
        rec.entity_id = Some(entity_id(&rec.kind, &rec.title, &rec.body));
        rec.superseded_by_entity_id = Some(entity_id("decision", "newer", "b2"));

        let json = serde_json::to_string(&rec).expect("serialize");
        assert!(json.contains("\"entity_id\""));
        assert!(json.contains("\"superseded_by_entity_id\""));

        let back: NoteRecord = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.entity_id, rec.entity_id);
        assert_eq!(back.superseded_by_entity_id, rec.superseded_by_entity_id);
    }

    /// The edge list is omitted when empty and carries `(kind, target
    /// entity_id)` pairs verbatim when not.
    #[test]
    fn edges_are_omitted_when_empty_and_round_trip_when_present() {
        let rec = base_record();
        let json = serde_json::to_string(&rec).expect("serialize");
        assert!(!json.contains("edges"), "key omitted when empty: {json}");

        let mut rec = base_record();
        rec.edges = vec![
            CarriedEdge::new("relates_to", "aaaa".to_string()),
            CarriedEdge::new("contradicts", "bbbb".to_string()),
        ];
        let json = serde_json::to_string(&rec).expect("serialize");
        assert!(json.contains("\"edges\""), "{json}");
        assert_eq!(rec.schema_version, 1, "additive: no version bump");

        let back: NoteRecord = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.edges, rec.edges);
    }

    /// Every record written before edges existed has no `edges` key; it must
    /// read exactly as before, with an empty list.
    #[test]
    fn a_blob_without_the_edges_key_reads_as_no_edges() {
        let old = r#"{"schema_version":1,"id":7,"kind":"note","title":"t","body":"b","tags":[],"linked_files":[],"created_at":1,"status":"active","entity_id":"e7"}"#;
        let back: NoteRecord = serde_json::from_str(old).expect("deserialize old blob");
        assert!(back.edges.is_empty());
        assert_eq!(back.schema_version, 1);
    }

    /// The reader side of the additive contract: a key this build does not
    /// know is ignored, which is what lets a field be added under the same
    /// `schema_version` without every older reader refusing the note.
    #[test]
    fn an_unknown_extra_key_is_ignored() {
        let future = r#"{"schema_version":1,"id":7,"kind":"note","title":"t","body":"b","tags":[],"linked_files":[],"created_at":1,"status":"active","edges":[{"kind":"relates_to","to_entity_id":"e1"}],"not_yet_invented":{"x":1}}"#;
        let back: NoteRecord = serde_json::from_str(future).expect("unknown key must not refuse");
        assert_eq!(back.id, 7);
        assert_eq!(
            back.edges,
            vec![CarriedEdge::new("relates_to", "e1".to_string())]
        );
    }

    /// A legacy blob with no `entity_id` key recomputes the same id a fresh
    /// writer would have stored — absence is fully recoverable.
    #[test]
    fn legacy_blob_recomputes_entity_id() {
        let legacy = r#"{"schema_version":1,"id":1,"kind":"decision","title":"HTTP layer","body":"use axum","tags":["x"],"linked_files":["a.rs"],"created_at":123,"status":"active"}"#;
        let back: NoteRecord = serde_json::from_str(legacy).expect("deserialize legacy blob");

        assert_eq!(back.entity_id, None, "key absent in legacy blob");
        assert_eq!(
            back.resolve_entity_id(),
            "cc308a1ca5d849191e1710cc9def561377a9ef37e4fcb895e5aa3b1896e43603"
        );

        // A record that stores the field resolves to the identical value.
        let mut fresh = base_record();
        fresh.kind = "decision".to_string();
        fresh.title = "HTTP layer".to_string();
        fresh.body = "use axum".to_string();
        fresh.entity_id = Some(entity_id(&fresh.kind, &fresh.title, &fresh.body));
        assert_eq!(fresh.resolve_entity_id(), back.resolve_entity_id());
    }

    /// The bug this fixes: a re-`init` renumbers the rowid, so two different
    /// entries can carry the same `id` in one notes ref. Their `entity_id`s
    /// must still distinguish them.
    #[test]
    fn colliding_rowids_have_distinct_entity_ids() {
        let mut first = base_record();
        first.id = 1;
        first.title = "first decision".to_string();
        first.body = "body one".to_string();

        let mut second = base_record();
        second.id = 1; // re-init reset the counter
        second.title = "second decision".to_string();
        second.body = "body two".to_string();

        assert_eq!(first.id, second.id, "rowids collide, as observed live");
        assert_ne!(
            first.resolve_entity_id(),
            second.resolve_entity_id(),
            "distinct content must yield distinct identity despite the rowid collision"
        );
    }
}
