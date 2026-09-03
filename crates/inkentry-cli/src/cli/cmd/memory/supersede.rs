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
    if backend
        .supersede(old_target.id.clone(), new_note.id.clone())
        .await
        .map_err(backend_err)?
    {
        println!("Archived #{old_handle} → superseded by #{new_handle}.");

        // ── Git-notes write-through carrier ──────────────────────────────────
        // Best-effort and non-fatal, matching `memory add`'s contract: SQLite
        // above already holds the authoritative archive + link, so a failed
        // carry means only that the edge stays local for now, never that the
        // command fails. `GitNotesBackend::supersede` stays unsupported
        // (ADR-068 D3), so this is the sole path that carries the edge when
        // git notes is the primary store (explicit `--backend git-notes`
        // never reaches here: `supersede` above already returned `Err`).
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

    // ADR-037 P2: best-effort, non-blocking nudge of the local relay so a
    // `local_first` supersede's outbox drains promptly. See `outbox.rs`.
    super::outbox::nudge_after_write(cfg, mem_path).await;
    Ok(())
}
