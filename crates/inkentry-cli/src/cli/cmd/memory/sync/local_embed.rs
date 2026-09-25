// Push repairs rows with `remote_id IS NULL`, pull the rows that carry one;
// the two scopes never overlap.

use anyhow::Result;

use super::pull::PullSummary;
use super::push::PushSummary;
use crate::{
    capability,
    config::{Config, SyncMode},
    embeddings::vec_to_blob,
    server_client::ServerInferenceClient,
    storage::{MemoryStore, SyncRow},
};

pub(in crate::cli::cmd) enum LocalEmbedPolicy<'a> {
    Repair {
        cfg: &'a Config,
        project_root: std::path::PathBuf,
    },
    Skip,
}

impl<'a> LocalEmbedPolicy<'a> {
    // `cloud_first` with a `server_url` moves the store off `memory.db`; the
    // same condition `memory reindex` refuses under.
    pub(in crate::cli::cmd) fn resolve(cfg: &'a Config, mem_path: &std::path::Path) -> Self {
        if cfg.resolve_mode() == SyncMode::CloudFirst && cfg.server_url.is_some() {
            return Self::Skip;
        }
        Self::Repair {
            cfg,
            project_root: mem_path.parent().unwrap_or(mem_path).to_path_buf(),
        }
    }
}

pub(super) struct RepairCounts {
    pub(super) embedded: usize,
    pub(super) without_vector: usize,
}

pub(in crate::cli::cmd::memory) fn pending_embedding_warning(count: usize) -> String {
    let entries = if count == 1 { "entry" } else { "entries" };
    format!(
        "warning: {count} synced {entries} could not be embedded locally, so \
         `inkentry memory search` cannot surface {} semantically yet. The next \
         sync or pull retries automatically; `inkentry memory reindex` does it now.",
        if count == 1 { "it" } else { "them" }
    )
}

pub(in crate::cli::cmd::memory) fn pull_embed_summary(summary: &PullSummary) -> String {
    match (summary.embedded_locally, summary.without_local_vector) {
        (0, 0) => String::new(),
        (embedded, 0) => format!(" Embedded {embedded} synced entries locally."),
        (0, pending) => format!(" {pending} synced entries pending embedding."),
        (embedded, pending) => {
            format!(" Embedded {embedded} synced entries locally, {pending} pending embedding.")
        }
    }
}

pub(in crate::cli::cmd::memory) fn local_embed_summary(summary: &PushSummary) -> String {
    match (summary.embedded_locally, summary.without_local_vector) {
        (0, 0) => String::new(),
        (embedded, 0) => format!(" Embedded {embedded} locally."),
        (embedded, missing) => {
            format!(" Embedded {embedded} locally, {missing} without a local embedding.")
        }
    }
}

// A blob that does not decode to exactly `EMBEDDING_DIM` floats (torn write)
// counts as no vector.
pub(super) fn usable_vector(blob: Option<Vec<u8>>) -> Option<Vec<f32>> {
    blob.map(|b| inkentry_core::embeddings::blob_to_vec(&b))
        .filter(|v| v.len() == inkentry_core::embeddings::EMBEDDING_DIM)
}

// `get_inference_tier` (not `get_tier`) probes loopback only outside
// `cloud_first`, so the embed never reaches a team `server_url`, which would
// re-embed server-side.
async fn resolve_local_embedder(
    cfg: &Config,
    project_root: &std::path::Path,
) -> Option<ServerInferenceClient> {
    let tier = capability::get_inference_tier(cfg).await;
    // An auto-discovered loopback server leaves `server_url` unset; bridge it
    // into an effective config, as `memory reindex` does.
    let eff_cfg = tier.effective_config(cfg, project_root);
    ServerInferenceClient::from_config(&eff_cfg)
}

pub(super) async fn repair_local_embeddings(
    local: &MemoryStore,
    live: &[&SyncRow],
    policy: &LocalEmbedPolicy<'_>,
) -> Result<RepairCounts> {
    let (cfg, project_root) = match policy {
        LocalEmbedPolicy::Skip => {
            return Ok(RepairCounts {
                embedded: 0,
                without_vector: 0,
            });
        }
        LocalEmbedPolicy::Repair { cfg, project_root } => (*cfg, project_root),
    };

    let mut missing: Vec<&SyncRow> = Vec::new();
    for r in live {
        if usable_vector(local.get_embedding(&r.id)?).is_none() {
            missing.push(r);
        }
    }
    // Resolve the embedder only once a row needs one: no discovery probe or
    // warning for an empty or fully-embedded set.
    if missing.is_empty() {
        return Ok(RepairCounts {
            embedded: 0,
            without_vector: 0,
        });
    }

    let Some(client) = resolve_local_embedder(cfg, project_root).await else {
        // Text-only, not a refusal: scripted and CI pushes never needed an embedder.
        return Ok(RepairCounts {
            embedded: 0,
            without_vector: missing.len(),
        });
    };

    let mut embedded = 0usize;
    let mut without_vector = 0usize;
    for r in missing {
        // Must match `memory reindex` / `memory add`'s document string and use
        // `embed_text` (`embed_query` prepends the `Instruct:` prefix), or the
        // vector lands in a different space.
        let doc = format!("title: {} | text: {}", r.title, r.body);
        match client.embed_text(&doc).await {
            Ok(vec) => {
                // Per-row commit: an interrupted push keeps its vectors.
                local.insert_embedding(&r.id, &vec_to_blob(&vec))?;
                embedded += 1;
            }
            Err(e) => {
                // One row's failure must not abort the push; it ships text-only.
                tracing::warn!("embedding note {} before push failed: {e:#}", r.id);
                without_vector += 1;
            }
        }
    }

    Ok(RepairCounts {
        embedded,
        without_vector,
    })
}
