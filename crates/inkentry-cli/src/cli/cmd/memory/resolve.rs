use anyhow::Result;

use crate::storage::memory::Note;
use crate::storage::{MemoryBackend, NoteId, is_entity_id_lookup};

// `Err` rather than `None` wherever an answer would be a guess: a handle naming
// several entries, or a backend that could not read far enough to know.
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
    // A full `entity_id` is its own longest prefix and the column is unique,
    // so exact and prefix match are the same read.
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
