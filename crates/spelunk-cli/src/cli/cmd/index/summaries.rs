use anyhow::{Context, Result};
use std::sync::Arc;

use crate::{
    config::Config,
    server_client::{ServerInferenceClient, ServerLlmAdapter},
    storage::Database,
};

/// Run the optional LLM summary generation pass.
///
/// Fetches chunks without summaries in batches, calls the LLM via
/// spelunk-server, and stores results.
/// If `server_url` is not configured or `no_summaries` is true, returns early.
pub(super) async fn generate_summaries(
    no_summaries: bool,
    summary_batch_size: usize,
    cfg: &Config,
    db: &Database,
) -> Result<()> {
    if no_summaries {
        return Ok(());
    }

    if cfg.server_url.is_none() {
        eprintln!("  Skipping summaries (no server_url configured)");
        return Ok(());
    }

    // Count total chunks needing summaries for progress reporting.
    let batch_size = summary_batch_size.max(1);
    let first_batch = db.chunks_without_summaries(1)?;
    if first_batch.is_empty() {
        return Ok(());
    }

    // Build the LLM adapter via spelunk-server.
    let client = ServerInferenceClient::from_config(cfg).with_context(
        || "server_url is set but could not build ServerInferenceClient for summaries",
    )?;
    let llm = ServerLlmAdapter(Arc::new(client));

    // Count pending chunks for progress display.
    let pending = db.chunks_without_summaries(usize::MAX)?;
    let total_chunks = pending.len();
    let total_batches = total_chunks.div_ceil(batch_size);

    eprintln!("Generating summaries ({total_chunks} chunks, batch size {batch_size})\u{2026}");

    let mut batch_num = 0usize;
    loop {
        let batch = db.chunks_without_summaries(batch_size)?;
        if batch.is_empty() {
            break;
        }
        batch_num += 1;
        eprintln!("  Summarising batch {batch_num}/{total_batches}\u{2026}");

        match crate::indexer::summariser::summarise_batch(&llm, &batch).await {
            Ok(summaries) => {
                let mut summarised_ids = std::collections::HashSet::new();
                for (chunk_id, summary) in summaries {
                    if let Err(e) = db.update_chunk_summary(chunk_id, &summary) {
                        tracing::warn!("failed to store summary for chunk {chunk_id}: {e}");
                    } else {
                        summarised_ids.insert(chunk_id);
                    }
                }
                // Mark chunks that received no summary with "" so they aren't
                // re-fetched on the next pass (chunks_without_summaries checks IS NULL).
                for (id, _, _, _) in &batch {
                    if !summarised_ids.contains(id) {
                        let _ = db.update_chunk_summary(*id, "");
                    }
                }
            }
            Err(e) => {
                tracing::warn!("summarise_batch failed: {e}");
                // Mark the batch as attempted so we don't loop forever.
                for (id, _, _, _) in &batch {
                    let _ = db.update_chunk_summary(*id, "");
                }
            }
        }
    }

    eprintln!("  Summarised {batch_num} batch(es).");
    Ok(())
}
