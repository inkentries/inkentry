use anyhow::Result;

use crate::storage::Database;

/// Compose deterministic structural summaries for every named chunk that does
/// not have one yet.
///
/// This is the built-in-tier replacement for the retired LLM summary pass: no
/// server, no key, no network. It runs after parse and before the first embed,
/// so a chunk's first (and, on the fresh path, only) embedding already carries
/// its summary. The composed slot bridges retrieval vocabulary (docstring
/// sentence, split symbol name, split callee names, salient literals) and is
/// bounded by a hard token cap so it never displaces the code tail.
///
/// The composed summary is secret-scanned before storage — the salient-literals
/// ingredient is a new exposure a chunk body's own scan would not have caught.
/// On a hit the slot is stored as `""` (composed but suppressed), so it is not
/// recomputed on a plain re-index.
///
/// Title-less chunks are left untouched here; their slot is built later by
/// tier-3 MMR selection.
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

        // Scan the composed summary (not just the raw chunk): the salient
        // literals folded in above can carry a credential the chunk body's own
        // scan cleared. Best-effort defense-in-depth, not a boundary.
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
