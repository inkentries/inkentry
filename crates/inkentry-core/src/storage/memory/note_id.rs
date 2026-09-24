//! The identity of a memory entry.
//!
//! Every store exports a UUIDv7 as an entry's id. The integer rowid the local
//! store keys on is a storage surrogate that never leaves `crate::storage::memory`.

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::fmt;
use std::str::FromStr;

/// The identity of a memory entry: an opaque, backend-minted token.
///
/// Ordering is lexicographic over the token, which for UUIDv7s is creation order.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NoteId(String);

impl NoteId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NoteId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Rejects only the empty string. The type cannot know which backend will
/// resolve an id, so it imposes no shape: a bare integer parses, then fails to
/// resolve with [`unresolvable_id_message`].
impl FromStr for NoteId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err("memory entry id must not be empty".to_string());
        }
        Ok(NoteId(s.to_string()))
    }
}

impl Serialize for NoteId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for NoteId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;

        impl de::Visitor<'_> for V {
            type Value = NoteId;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a memory entry id")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<NoteId, E> {
                NoteId::from_str(v).map_err(E::custom)
            }
        }

        d.deserialize_str(V)
    }
}

/// The message for an id that parsed but resolves to nothing.
///
/// A numeric id gets its own wording: entries were numbered with integers before
/// UUIDs became the identity, and the old numbers do not map onto the new ids.
pub fn unresolvable_id_message(id: &NoteId) -> String {
    if id.as_str().parse::<i64>().is_ok() {
        format!(
            "'{id}' is not a memory entry id. Entries are identified by UUID; the integer \
             ids used before are gone and do not map onto the new ones. \
             Run `inkentry memory list` to see the ids this project uses."
        )
    } else {
        format!(
            "No memory entry with id '{id}'. \
             Run `inkentry memory list` to see the ids this project uses."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_the_only_rejected_token() {
        assert!("".parse::<NoteId>().is_err());
        assert!("not-a-uuid".parse::<NoteId>().is_ok());
        assert!(" ".parse::<NoteId>().is_ok());
    }

    #[test]
    fn every_id_serializes_as_a_json_string() {
        let uuid: NoteId = "0199a0f1-4d3c-7c2a-9b1e-6f0a2c5d8e33".parse().unwrap();
        assert_eq!(
            serde_json::to_string(&uuid).unwrap(),
            "\"0199a0f1-4d3c-7c2a-9b1e-6f0a2c5d8e33\""
        );
        let numeric: NoteId = "42".parse().unwrap();
        assert_eq!(serde_json::to_string(&numeric).unwrap(), "\"42\"");
    }

    #[test]
    fn a_json_number_is_not_an_id() {
        assert!(serde_json::from_str::<NoteId>("42").is_err());
    }

    #[test]
    fn serialize_deserialize_round_trips() {
        let id: NoteId = "0199a0f1-4d3c-7c2a-9b1e-6f0a2c5d8e33".parse().unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(serde_json::from_str::<NoteId>(&json).unwrap(), id);
    }

    #[test]
    fn a_numeric_id_is_told_that_entries_are_identified_by_uuid() {
        let msg = unresolvable_id_message(&"42".parse().unwrap());
        assert!(msg.contains("identified by UUID"), "{msg}");
        assert!(msg.contains("inkentry memory list"), "{msg}");
    }

    #[test]
    fn a_non_numeric_miss_is_a_plain_not_found() {
        let msg = unresolvable_id_message(&"0199a0f1-4d3c-7c2a-9b1e-6f0a2c5d8e33".parse().unwrap());
        assert!(!msg.contains("identified by UUID"), "{msg}");
    }
}
