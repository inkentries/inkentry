use anyhow::Result;

use super::{backend_err, cross_project};
use crate::{
    config::Config,
    storage::{NoteId, memory::Note, open_memory_backend},
};

/// Retrieve the memory-corpus ranked list for unified search — the fold-in of
/// the former `memory search` command (ADR-082). Returns notes in the memory
/// pipeline's own ranked order; cross-corpus fusion (ADR-081) then assigns each
/// its rank position.
///
/// `qa_blob` is the QA-prefix query embedding: `Some` runs hybrid (note vector
/// KNN fused with `memory_fts` BM25), `None` runs full-text only — the
/// `--only-text` path and the embedder-unavailable degrade. `as_of` restricts to
/// the temporal window; `expand_graph` adds `relates_to` 1-hop neighbours; and
/// unless `local_only`, locked / cross-project decisions and requirements from
/// linked stores are appended (text-only, as they have no CLI-side embedder).
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
) -> Result<Vec<Note>> {
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

    let mut notes = if expand_graph {
        let mut seen: std::collections::HashSet<i64> =
            notes.iter().filter_map(|n| n.id.as_i64()).collect();
        let mut expanded = notes;
        let mut neighbours = vec![];
        for n in &expanded {
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
                    neighbours.push(nb);
                }
            }
        }
        expanded.extend(neighbours);
        expanded
    } else {
        notes
    };

    // Cross-project dep pass (ADR-003): append locked/cross-project decisions and
    // requirements from linked projects unless --local-only. Dep stores are
    // queried by text (no CLI-side embedder), filtered to the locked/cross-project
    // tag set, deduped against local results.
    if !local_only {
        let mut seen: std::collections::HashSet<(String, NoteId)> = Default::default();
        for n in &notes {
            seen.insert((String::new(), n.id.clone()));
        }
        let dep_notes = cross_project::collect_dep_cross_cutting(index_db_path, &mut seen).await;
        notes.extend(dep_notes);
    }

    Ok(notes)
}
