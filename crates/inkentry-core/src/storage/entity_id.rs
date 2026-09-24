//! Content-addressed identity for memory entries.
//!
//! An entry's `entity_id` is a pure function of its kind, title and body, so any
//! reader can recompute it without coordination and two machines that record the
//! same decision get the same id. The field set and encoding are frozen: changing
//! either changes every id.

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

use super::memory::Note;

// The encoding is frozen: sorted keys, compact separators, raw UTF-8, and the
// field text hashed exactly as stored (no trimming, normalisation or case folding).
fn canonical_bytes(kind: &str, title: &str, body: &str) -> Vec<u8> {
    let map = BTreeMap::from([("body", body), ("kind", kind), ("title", title)]);
    let mut buf = Vec::new();
    let mut ser = serde_json::Serializer::new(&mut buf);
    // Infallible: a BTreeMap<&str, &str> has no non-string keys and no NaN.
    map.serialize(&mut ser).expect("canonical JSON");
    buf
}

/// The identity of a memory entry: lowercase-hex SHA-256 over the canonical
/// JSON of `{body, kind, title}`.
///
/// Timestamps, tags, linked files, status and machine-local ids are excluded, so
/// the identity does not change when mutable metadata does.
pub fn entity_id(kind: &str, title: &str, body: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_bytes(kind, title, body));
    hex::encode(hasher.finalize())
}

/// `entity_id` for a stored note.
pub fn note_entity_id(n: &Note) -> String {
    entity_id(&n.kind, &n.title, &n.body)
}

/// How many characters of an `entity_id` are shown to and quoted by users.
///
/// A display width only; nothing stored depends on it.
pub const ENTITY_ID_HANDLE_LEN: usize = 12;

/// The shortest prefix looked up as an `entity_id`.
///
/// A shorter token is never tried as a handle, because an accidental match
/// against unrelated input would stop being implausible.
pub const ENTITY_ID_MIN_PREFIX_LEN: usize = 8;

/// The handle of an entry: the leading [`ENTITY_ID_HANDLE_LEN`] characters of
/// its `entity_id`.
pub fn entity_id_handle(entity_id: &str) -> &str {
    &entity_id[..entity_id.len().min(ENTITY_ID_HANDLE_LEN)]
}

/// Whether `token` can be looked up as an `entity_id` or a prefix of one.
///
/// `entity_id` is lowercase hex, and a UUIDv7 carries hyphens, so the two id
/// forms can never claim the same token.
pub fn is_entity_id_lookup(token: &str) -> bool {
    (ENTITY_ID_MIN_PREFIX_LEN..=64).contains(&token.len())
        && token
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn fields_do_not_splice() {
        assert_ne!(
            entity_id("decision", "ab", "c"),
            entity_id("decision", "a", "bc")
        );
    }

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

    #[test]
    fn the_handle_is_the_leading_twelve_characters() {
        let id = entity_id("decision", "HTTP layer", "use axum");
        assert_eq!(entity_id_handle(&id), "cc308a1ca5d8");
        assert_eq!(entity_id_handle(&id).len(), ENTITY_ID_HANDLE_LEN);
    }

    #[test]
    fn a_token_shorter_than_the_floor_is_not_a_handle() {
        let id = entity_id("decision", "HTTP layer", "use axum");
        assert!(!is_entity_id_lookup(&id[..ENTITY_ID_MIN_PREFIX_LEN - 1]));
        assert!(is_entity_id_lookup(&id[..ENTITY_ID_MIN_PREFIX_LEN]));
        assert!(is_entity_id_lookup(&id));
    }

    #[test]
    fn a_uuid_is_never_read_as_a_handle() {
        assert!(!is_entity_id_lookup("0199a0f1-4d3c-7c2a-9b1e-6f0a2c5d8e33"));
        assert!(!is_entity_id_lookup("cc308a1ca5d8ZZ"));
        assert!(!is_entity_id_lookup("CC308A1CA5D8"));
    }
}
