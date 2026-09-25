use anyhow::Result;

use super::{backend_err, cross_project};
use crate::{
    config::Config,
    storage::{NoteId, memory::Note, open_memory_backend},
};

// Only `ranked` may enter cross-corpus fusion: attachments were never ranked by
// the query, so a rank would invent relevance and let them displace matched code.
pub(crate) struct MemoryCorpus {
    pub ranked: Vec<Note>,
    pub attachments: Vec<Note>,
}

// `qa_blob` `None` means full-text only. `gate` applies the relevance floor to
// the hybrid path only. `tag`/`file` post-filter the already-bounded result
// set rather than being pushed into the search SQL.
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
    gate: bool,
    tag: Option<&str>,
    file: Option<&str>,
) -> Result<MemoryCorpus> {
    // So a teammate's newly fetched entry is searchable without a re-init.
    super::reconcile::refresh_read_path_from_git_notes(cfg, mem_path, None).await;

    let backend = open_memory_backend(cfg, mem_path, None).await?;

    let notes = match qa_blob {
        Some(blob) => backend
            .search_hybrid(blob, query, limit, as_of, gate)
            .await
            .map_err(backend_err)?,
        None => backend
            .search_text(query, limit, as_of)
            .await
            .map_err(backend_err)?,
    };

    let normalised_tag = tag.map(crate::storage::normalize_tag);
    let matches_filters = |n: &Note, normalised_tag: &Option<Option<String>>| -> bool {
        if let Some(t) = normalised_tag
            && !t
                .as_deref()
                .is_some_and(|t| n.tags.iter().any(|nt| nt == t))
        {
            return false;
        }
        if let Some(f) = file
            && !n.linked_files.iter().any(|nf| nf == f)
        {
            return false;
        }
        true
    };
    let notes: Vec<Note> = notes
        .into_iter()
        .filter(|n| matches_filters(n, &normalised_tag))
        .collect();

    let mut attachments: Vec<Note> = vec![];

    if expand_graph {
        let mut seen: std::collections::HashSet<NoteId> =
            notes.iter().map(|n| n.id.clone()).collect();
        for n in &notes {
            let (outgoing, incoming) = backend.get_edges(&n.id).await.map_err(backend_err)?;
            for e in outgoing.iter().chain(incoming.iter()) {
                if e.kind != "relates_to" {
                    continue;
                }
                let neighbour_id = if e.from_id == n.id {
                    e.to_id.clone()
                } else {
                    e.from_id.clone()
                };
                if seen.insert(neighbour_id.clone())
                    && let Some(nb) = backend.get(neighbour_id).await?
                {
                    attachments.push(nb);
                }
            }
        }
    }

    // Dep stores are selected by tag, not by the query, and deduped against
    // local results.
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
