use anyhow::Result;

use crate::storage::Database;

pub(super) fn generate_structural_summaries(db: &Database) -> Result<()> {
    let targets = db.named_chunks_needing_summary()?;
    if targets.is_empty() {
        return Ok(());
    }

    for (id, name, metadata, content) in targets {
        let docstring = metadata
            .as_deref()
            .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
            .and_then(|v| {
                v.get("docstring")
                    .and_then(|d| d.as_str().map(str::to_string))
            });
        let callees = db.callees_for_symbol(&name)?;
        let composed = inkentry_core::indexer::summariser::compose_structural_summary(
            &name,
            docstring.as_deref(),
            &callees,
            &content,
        );

        // Salient literals folded into the summary can carry a credential the
        // chunk's own scan cleared. `""` marks composed-but-suppressed, so a
        // plain re-index does not recompute it.
        let to_store =
            if composed.is_empty() || inkentry_core::indexer::secrets::contains_secret(&composed) {
                if !composed.is_empty() {
                    tracing::warn!(
                        "suppressing structural summary for '{name}' (possible secret detected)"
                    );
                }
                ""
            } else {
                composed.as_str()
            };
        if let Err(e) = db.update_chunk_summary(id, to_store) {
            tracing::warn!("failed to store structural summary for '{name}': {e}");
        }
    }
    Ok(())
}
