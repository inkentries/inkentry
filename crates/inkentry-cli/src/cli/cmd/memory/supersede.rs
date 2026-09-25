use anyhow::Result;

use super::{MemorySupersededArgs, backend_err};
use crate::{
    config::Config,
    storage::{append_state_update, note_entity_id, now_secs, open_memory_backend},
};

pub(super) async fn memory_supersede(
    args: MemorySupersededArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
    backend_override: Option<&str>,
) -> Result<()> {
    let started = std::time::Instant::now();
    let backend = open_memory_backend(cfg, mem_path, backend_override).await?;
    let Some(new_note) = super::resolve::resolve_note(backend.as_ref(), &args.new_id).await? else {
        anyhow::bail!("No memory entry with id {} (new).", args.new_id);
    };
    let Some(old_target) = super::resolve::resolve_note(backend.as_ref(), &args.old_id).await?
    else {
        anyhow::bail!("No active memory entry with id {} (old).", args.old_id);
    };
    let old_handle = crate::storage::entity_id_handle(&old_target.entity_id).to_string();
    let new_handle = crate::storage::entity_id_handle(&new_note.entity_id).to_string();
    // A handle, a longer prefix of it and the local id all name one entry;
    // letting that through archives it and links its successor to itself.
    if old_target.id == new_note.id {
        anyhow::bail!(
            "'{old}' and '{new}' name the same memory entry (#{old_handle}), \
             so it would supersede itself. Give the id of the entry that \
             replaces it; `inkentry memory add` prints one for a new entry.",
            old = args.old_id,
            new = args.new_id,
        );
    }
    if backend
        .supersede(old_target.id.clone(), new_note.id.clone())
        .await
        .map_err(backend_err)?
    {
        println!("Archived #{old_handle} → superseded by #{new_handle}.");

        // Best-effort: SQLite already holds the authoritative archive and link.
        // `GitNotesBackend::supersede` is unsupported, so this is the only path
        // that carries the edge to git notes.
        let write_through = cfg.store_in_git_notes && backend_override != Some("git-notes");
        if write_through {
            match backend.get(old_target.id.clone()).await {
                Ok(Some(old_note)) => {
                    let new_entity_id = note_entity_id(&new_note);
                    let invalid_at = old_note.invalid_at.or_else(|| Some(now_secs()));
                    if let Err(e) = append_state_update(
                        None,
                        &old_note,
                        "archived",
                        invalid_at,
                        Some(new_entity_id),
                    )
                    .await
                    {
                        eprintln!(
                            "Warning: #{old_handle} archived locally, but carrying its \
                             supersede edge to git notes failed, so it will not travel with \
                             the repo: {e:#}"
                        );
                    }
                }
                Ok(None) => {
                    eprintln!(
                        "Warning: could not re-read #{old_handle} after archiving it, so its \
                         supersede edge was not carried to git notes."
                    );
                }
                Err(e) => {
                    eprintln!(
                        "Warning: could not re-read #{old_handle} after archiving it, so its \
                         supersede edge was not carried to git notes: {e:#}"
                    );
                }
            }
        }
    } else {
        anyhow::bail!("No active memory entry with id {} (old).", args.old_id);
    }

    super::outbox::nudge_after_write(cfg, mem_path).await;

    super::super::events::record(
        cfg,
        mem_path,
        backend_override,
        "memory.supersede",
        None,
        Some(1),
        std::slice::from_ref(&new_note.entity_id),
        None,
        started,
        true,
    );
    Ok(())
}
