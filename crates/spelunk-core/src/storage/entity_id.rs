//! Content-addressed identity for memory entries.
//!
//! `entity_id` is the canonical identity of a memory entry on every surface —
//! the local store, `refs/notes/spelunk`, and the server. It is a pure function
//! of the entry's semantic core, so any reader can recompute it from the entry
//! itself with no coordination, and two machines that independently record the
//! same decision land on the same id.
//!
//! See ADR-068 for the canonical form. The field set and encoding are frozen for
//! `schema_version` 1: changing either is a version bump and a new ADR.

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

use super::memory::Note;

/// Canonical JSON bytes hashed to produce an `entity_id`.
///
/// `BTreeMap` supplies the code-point-sorted keys; serde supplies the compact
/// separators and raw (non-`\u`-escaped) UTF-8. The exact stored bytes of each
/// field are hashed — no normalization, trimming, or case folding.
fn canonical_bytes(kind: &str, title: &str, body: &str) -> Vec<u8> {
    let map = BTreeMap::from([("body", body), ("kind", kind), ("title", title)]);
    let mut buf = Vec::new();
    let mut ser = serde_json::Serializer::new(&mut buf);
    // Infallible: a BTreeMap<&str, &str> has no non-string keys and no NaN.
    map.serialize(&mut ser).expect("canonical JSON");
    buf
}

/// The canonical identity of a memory entry: lowercase-hex `sha256` over the
/// canonical JSON of exactly `{body, kind, title}`.
///
/// Deliberately excludes `created_at`, `tags`, `linked_files`, `status`,
/// `superseded_by`, and every machine-local id: identity must not move when
/// mutable metadata does, or converge across machines becomes impossible.
pub fn entity_id(kind: &str, title: &str, body: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_bytes(kind, title, body));
    hex::encode(hasher.finalize())
}

/// `entity_id` for a stored note.
pub fn note_entity_id(n: &Note) -> String {
    entity_id(&n.kind, &n.title, &n.body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The worked example from ADR-068. Pins the canonical bytes and the digest
    /// against any future refactor of the encoder.
    #[test]
    fn matches_adr_worked_example() {
        assert_eq!(
            canonical_bytes("decision", "HTTP layer", "use axum"),
            br#"{"body":"use axum","kind":"decision","title":"HTTP layer"}"#
        );
        assert_eq!(
            entity_id("decision", "HTTP layer", "use axum"),
            "cc308a1ca5d849191e1710cc9def561377a9ef37e4fcb895e5aa3b1896e43603"
        );
    }

    #[test]
    fn is_lowercase_hex_sha256() {
        let id = entity_id("decision", "t", "b");
        assert_eq!(id.len(), 64);
        assert!(
            id.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
        );
    }

    /// Field values are not concatenated: moving text across the field boundary
    /// must change the id (JSON framing prevents the classic splice collision).
    #[test]
    fn fields_do_not_splice() {
        assert_ne!(
            entity_id("decision", "ab", "c"),
            entity_id("decision", "a", "bc")
        );
    }

    /// Raw UTF-8, not `\u`-escaped, and no Unicode normalization: NFC and NFD
    /// spellings of the same grapheme are distinct ids.
    #[test]
    fn no_unicode_normalization() {
        let nfc = "café";
        let nfd = "cafe\u{301}";
        assert_ne!(nfc, nfd, "test inputs must differ byte-wise");
        assert_ne!(
            entity_id("decision", nfc, "b"),
            entity_id("decision", nfd, "b")
        );
        assert!(canonical_bytes("decision", nfc, "b").ends_with("café\"}".as_bytes()));
    }

    /// Control characters and quotes are JSON-escaped, so a field value cannot
    /// forge the surrounding structure.
    #[test]
    fn json_escapes_are_applied() {
        let bytes = canonical_bytes("decision", "a\"b", "c\nd");
        assert_eq!(
            bytes,
            br#"{"body":"c\nd","kind":"decision","title":"a\"b"}"#
        );
    }

    #[test]
    fn whitespace_is_not_trimmed() {
        assert_ne!(
            entity_id("decision", "t", "b"),
            entity_id("decision", "t", "b ")
        );
    }
}
