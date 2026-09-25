use anyhow::Result;

use inkentry_core::indexer::chunker::ChunkKind;
use inkentry_core::indexer::mmr::{self, MMR_LAMBDA};
use inkentry_core::indexer::summariser::SUMMARY_TOKEN_CAP;
use inkentry_core::search::tokens::estimate_tokens;

use crate::config::Config;
use crate::server_client::ServerInferenceClient;
use crate::storage::Database;

pub(super) async fn run_tier3_selection(cfg: &Config, db: &Database) -> Result<usize> {
    let candidates = db.titleless_chunks_needing_selection()?;
    if candidates.is_empty() {
        return Ok(0);
    }
    let Some(client) = ServerInferenceClient::from_config(cfg) else {
        return Ok(0);
    };

    let mut refined = 0usize;
    for (id, node_type, content) in candidates {
        // Stored primary vector as centroid: a fresh whole-chunk embed would
        // hit the single-chunk seq² path that OOMs on CPU.
        let Some(centroid) = db.embedding_for_chunk(id)? else {
            continue;
        };
        let units = mmr::split_into_units(&content, &kind_from_node_type(&node_type));
        if units.len() < 2 {
            // `""` so the chunk is not retried.
            db.update_chunk_summary(id, "")?;
            continue;
        }

        let mut unit_vectors = Vec::with_capacity(units.len());
        for unit in &units {
            unit_vectors.push(client.embed_text(unit).await?);
        }
        let order = mmr::mmr_rank(&unit_vectors, &centroid, MMR_LAMBDA);
        let summary = select_prefix(&units, &order, SUMMARY_TOKEN_CAP);

        if summary.is_empty() || inkentry_core::indexer::secrets::contains_secret(&summary) {
            if !summary.is_empty() {
                tracing::warn!(
                    "suppressing tier-3 summary for chunk {id} (possible secret detected)"
                );
            }
            db.update_chunk_summary(id, "")?;
            continue;
        }
        db.set_summary_and_mark_pending(id, &summary)?;
        refined += 1;
    }
    Ok(refined)
}

fn select_prefix(units: &[String], order: &[usize], cap: usize) -> String {
    let mut composed = String::new();
    for &idx in order {
        let unit = &units[idx];
        let candidate = if composed.is_empty() {
            unit.clone()
        } else {
            format!("{composed} {unit}")
        };
        if estimate_tokens(&candidate) > cap {
            break;
        }
        composed = candidate;
    }
    composed
}

fn kind_from_node_type(node_type: &str) -> ChunkKind {
    if node_type == "section" {
        ChunkKind::Section
    } else {
        ChunkKind::Verbatim
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_prefix_stops_at_the_token_cap() {
        let units = vec!["one two three".to_string(), "four five six".to_string()];
        assert_eq!(
            select_prefix(&units, &[0, 1], 96),
            "one two three four five six"
        );
        assert_eq!(select_prefix(&units, &[1, 0], 3), "four five six");
    }

    #[test]
    fn node_type_section_maps_to_markdown_split() {
        assert!(matches!(kind_from_node_type("section"), ChunkKind::Section));
        assert!(matches!(
            kind_from_node_type("verbatim"),
            ChunkKind::Verbatim
        ));
    }
}
