use anyhow::Result;

use super::{backend_err, cross_project};
use crate::{
    config::Config,
    storage::{NoteId, memory::Note, open_memory_backend},
};

/// The memory corpus's contribution to a unified search: notes the query
/// actually ranked, and notes attached to those without being ranked at all.
///
/// The split exists because only `ranked` may enter cross-corpus fusion.
/// Attachments have no position in the memory pipeline's order — a `relates_to`
/// neighbour was reached from a hit, and a cross-project entry was selected by
/// its tags — so giving them a `corpus_rank` would invent a relevance the
/// retrieval never measured and let them displace a genuinely matched code
/// chunk. ADR-081 gives attachments null fusion metadata; this is the memory
/// side of the same rule the `--graph` appendix follows on the code side.
pub(crate) struct MemoryCorpus {
    pub ranked: Vec<Note>,
    pub attachments: Vec<Note>,
}

/// Retrieve the memory corpus for unified search — the fold-in of the former
/// `memory search` command (ADR-082).
///
/// `qa_blob` is the QA-prefix query embedding: `Some` runs hybrid (note vector
/// KNN fused with `memory_fts` BM25), `None` runs full-text only — the
/// `--only-text` path and the embedder-unavailable degrade. `as_of` restricts to
/// the temporal window; `expand_graph` attaches `relates_to` 1-hop neighbours;
/// and unless `local_only`, locked / cross-project decisions and requirements
/// from linked stores are attached (text-only, as they have no CLI-side
/// embedder).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn memory_corpus_search(
    cfg: &Config,
    index_db_path: &std::path::Path,
    mem_path: &std::path::Path,
    query: &str,
    qa_blob: Option<&[u8]>,
    limit: usize,
    as_of: Option<i64>,
    expand_graph: bool,
    local_only: bool,
) -> Result<MemoryCorpus> {
    // Fold in any fetched teammate notes before searching, so a teammate's
    // newly-published entry is searchable on the default path without a re-init
    // (the read-path refresh the former `memory search` performed).
    super::reconcile::refresh_read_path_from_git_notes(cfg, mem_path, None).await;

    let backend = open_memory_backend(cfg, mem_path, None).await?;

    let notes = match qa_blob {
        Some(blob) => backend
            .search_hybrid(blob, query, limit, as_of)
            .await
            .map_err(backend_err)?,
        None => backend
            .search_text(query, limit, as_of)
            .await
            .map_err(backend_err)?,
    };

    let mut attachments: Vec<Note> = vec![];

    if expand_graph {
        let mut seen: std::collections::HashSet<i64> =
            notes.iter().filter_map(|n| n.id.as_i64()).collect();
        for n in &notes {
            let Some(rowid) = n.id.as_i64() else {
                continue;
            };
            let (outgoing, incoming) = backend.get_edges(rowid).await.map_err(backend_err)?;
            for e in outgoing.iter().chain(incoming.iter()) {
                if e.kind != "relates_to" {
                    continue;
                }
                let neighbour_id = if e.from_id == rowid {
                    e.to_id
                } else {
                    e.from_id
                };
                if seen.insert(neighbour_id)
                    && let Some(nb) = backend.get(NoteId::from_i64(neighbour_id)).await?
                {
                    attachments.push(nb);
                }
            }
        }
    }

    // Cross-project dep pass (ADR-003): locked/cross-project decisions and
    // requirements from linked projects unless --local-only. Dep stores are
    // selected by tag, not by the query, and are deduped against local results.
    if !local_only {
        let mut seen: std::collections::HashSet<(String, NoteId)> = notes
            .iter()
            .chain(attachments.iter())
            .map(|n| (String::new(), n.id.clone()))
            .collect();
        let dep_notes = cross_project::collect_dep_cross_cutting(index_db_path, &mut seen).await;
        attachments.extend(dep_notes);
    }

    Ok(MemoryCorpus {
        ranked: notes,
        attachments,
    })
}
