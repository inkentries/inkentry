use anyhow::Result;

use super::MemoryArchiveArgs;
use crate::{
    config::Config,
    storage::{append_state_update, now_secs, open_memory_backend},
};

pub(super) async fn memory_archive(
    args: MemoryArchiveArgs,
    mem_path: &std::path::Path,
    cfg: &Config,
    backend_override: Option<&str>,
) -> Result<()> {
    let backend = open_memory_backend(cfg, mem_path, backend_override).await?;
    let resolved = super::resolve::resolve_note(backend.as_ref(), &args.id).await?;
    let Some(target) = resolved else {
        anyhow::bail!("No active memory entry with id {}.", args.id);
    };
    let handle = crate::storage::entity_id_handle(&target.entity_id).to_string();
    if backend.archive(target.id.clone()).await? {
        println!("Archived memory entry #{handle}.");

        // Non-fatal: the primary store already holds the archive. Explicit
        // `--backend git-notes` is excluded because it is the primary store then.
        let write_through = cfg.store_in_git_notes && backend_override != Some("git-notes");
        if write_through {
            match backend.get(target.id.clone()).await {
                Ok(Some(note)) => {
                    let invalid_at = note.invalid_at.or_else(|| Some(now_secs()));
                    if let Err(e) =
                        append_state_update(None, &note, "archived", invalid_at, None).await
                    {
                        eprintln!(
                            "Warning: #{handle} archived locally, but the git-notes carry \
                             failed, so it will not travel with the repo: {e:#}"
                        );
                    }
                }
                Ok(None) => {
                    eprintln!(
                        "Warning: could not re-read #{handle} after archiving it, so it was \
                         not carried to git notes."
                    );
                }
                Err(e) => {
                    eprintln!(
                        "Warning: could not re-read #{handle} after archiving it, so it was \
                         not carried to git notes: {e:#}"
                    );
                }
            }
        }
    } else {
        anyhow::bail!("No active memory entry with id {}.", args.id);
    }

    super::outbox::nudge_after_write(cfg, mem_path).await;
    Ok(())
}
