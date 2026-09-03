//! Resolving the id token a user typed to the id the backend holds.
//!
//! A user quotes the handle `memory list` and `memory show` display, which is a
//! prefix of the entry's portable `entity_id`; the backend's own token is a
//! UUIDv7 minted on this machine. ADR-093 D4 fixes the order the two are tried
//! in and requires an ambiguous handle to resolve nothing.

use anyhow::Result;

use crate::storage::memory::Note;
use crate::storage::{MemoryBackend, NoteId, is_entity_id_lookup};

/// The entry `token` names, or `None` when nothing matches.
///
/// `Err` is reserved for a handle that names more than one entry: picking one
/// would show, archive or supersede an entry the user did not mean.
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
    let mut candidates = backend
        .note_ids_for_entity_id_prefix(token.as_str())
        .await?;
    match candidates.len() {
        0 => Ok(None),
        1 => backend.get(candidates.remove(0)).await,
        n => anyhow::bail!(
            "'{token}' matches {n} memory entries. \
             Give more characters of the entity id to pick one; \
             `inkentry memory list` shows them."
        ),
    }
}
