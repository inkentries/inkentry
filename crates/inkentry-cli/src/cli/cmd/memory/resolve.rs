//! Resolving the id token a user typed to the id the backend holds.
//!
//! A user quotes the handle `memory list` and `memory show` display, which is a
//! prefix of the entry's portable `entity_id`; the backend's own token is a
//! UUIDv7 minted on this machine. ADR-093 D4 fixes the order the two are tried
//! in and requires an ambiguous handle to resolve nothing.

use anyhow::Result;

use crate::storage::memory::Note;
use crate::storage::{MemoryBackend, NoteId, is_entity_id_lookup};

/// The entry `token` names, or `None` when the store does not hold it.
///
/// `Err` covers the two cases where an answer would be a guess: a handle naming
/// more than one entry, where picking one would show, archive or supersede an
/// entry the user did not mean, and a backend that could not read far enough to
/// know, where `None` would be a denial it never established.
pub(super) async fn resolve_note(
    backend: &dyn MemoryBackend,
    token: &NoteId,
) -> Result<Option<Note>> {
    if let Some(note) = backend.get(token.clone()).await? {
        return Ok(Some(note));
    }
    if !is_entity_id_lookup(token.as_str()) {
        return Ok(None);
    }
    // A full `entity_id` is the longest prefix of itself and the column is
    // unique, so the exact match and the prefix match are the same read.
    let (mut matches, examined) = backend
        .note_ids_for_entity_id_prefix(token.as_str())
        .await?
        .into_parts();
    match matches.len() {
        0 => match examined {
            None => Ok(None),
            Some(examined) => anyhow::bail!(
                "'{token}' is not among the {examined} most recent memory entries, \
                 and this server offers no lookup by entity id, so older entries \
                 were not searched. Run `inkentry memory list --archived --limit N` \
                 with a larger N to reach further back."
            ),
        },
        1 => backend.get(matches.remove(0)).await,
        n => anyhow::bail!(
            "'{token}' matches {n} memory entries. \
             Give more characters of the entity id to pick one; \
             `inkentry memory list` shows them."
        ),
    }
}
