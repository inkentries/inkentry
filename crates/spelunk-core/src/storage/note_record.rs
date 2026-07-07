use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

use super::memory::Note;

/// Serialised form stored as JSON in a memory backend (git-notes blob or SQLite).
///
/// `schema_version` 0 = legacy (field absent in old blobs), 1 = current.
#[derive(Debug, Serialize, Deserialize)]
pub struct NoteRecord {
    /// Absent in legacy blobs — treated as version 0 via `#[serde(default)]`.
    #[serde(default)]
    pub schema_version: u8,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<i64>,
    /// Canonical cross-machine id (uuid), set on sync to a remote server.
    /// Optional and additive: absent on the wire means `None`; an old blob
    /// without this key reads as `None`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub remote_id: Option<String>,
}

pub fn record_to_note(r: NoteRecord) -> Note {
    Note {
        id: r.id,
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
}
