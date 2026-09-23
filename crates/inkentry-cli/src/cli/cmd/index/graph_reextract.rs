//! ADR-097's graph-only refresh pass: `index_018.sql` migrates `graph_edges`
//! in place and leaves every row `target_file = NULL` (unresolved, exactly
//! today's behaviour), then marks a re-extraction owed
//! (`Database::mark_pass_owed`). This module is what pays that off, the next
//! time `inkentry index` runs: re-extract every indexed file's edges — not
//! only changed ones — without touching chunks or embeddings, then clear the
//! marker (`Database::clear_pass_owed`).

use anyhow::Result;

use super::mentions::extract_mention_tokens;
use crate::{indexer::graph::EdgeExtractor, storage::Database};

const GRAPH_EDGES_REEXTRACT_PASS: &str = "graph_edges_reextract";

/// No-op when nothing is owed, so a normal `inkentry index` run pays only the
/// cost of one `index_meta` lookup.
pub(super) fn run_if_owed(root: &std::path::Path, db: &Database) -> Result<()> {
    if !db.pass_owed(GRAPH_EDGES_REEXTRACT_PASS)? {
        return Ok(());
    }
    let files = db.file_records_under("")?;
    if !files.is_empty() {
        eprintln!(
            "Re-extracting graph edges for {} file(s) (schema step 18)\u{2026}",
            files.len()
        );
    }
    for file in &files {
        let Some(language) = file.language.as_deref() else {
            continue;
        };
        let source = match std::fs::read_to_string(root.join(&file.path)) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("skipping graph re-extraction for {}: {e}", file.path);
                continue;
            }
        };
        if !replace_structural_edges(db, &source, &file.path, language) {
            continue;
        }
        restore_mention_edges(db, &file.path, language)?;
    }
    db.clear_pass_owed(GRAPH_EDGES_REEXTRACT_PASS)
}

/// Re-extracts and stores the structural (calls/imports/extends/implements)
/// edges for one file. Returns `false` on a warned-and-skipped failure, so
/// the caller does not then try to restore mentions onto edges that were
/// never replaced.
fn replace_structural_edges(db: &Database, source: &str, path: &str, language: &str) -> bool {
    let edges = match EdgeExtractor::extract(source, path, language) {
        Ok(edges) => edges,
        Err(e) => {
            tracing::warn!("graph extraction failed for {path}: {e}");
            return false;
        }
    };
    if let Err(e) = db.replace_edges(path, &edges) {
        tracing::warn!("graph edge storage failed for {path}: {e}");
        return false;
    }
    true
}

/// `replace_edges` just cleared every edge kind for this file, mentions
/// included. Restore them from the chunks already stored rather than
/// re-parsing: this pass's whole point is touching no chunk or embedding row.
fn restore_mention_edges(db: &Database, path: &str, language: &str) -> Result<()> {
    let mention_owned: Vec<(Option<String>, String)> = db
        .chunks_for_file(path)?
        .into_iter()
        .filter(|c| c.file_path == path && c.name.is_some())
        .flat_map(|c| {
            let name = c.name.clone().unwrap();
            extract_mention_tokens(&c.content, language)
                .into_iter()
                .map(move |sym| (Some(name.clone()), sym))
        })
        .collect();
    let mention_refs: Vec<(Option<&str>, &str)> = mention_owned
        .iter()
        .map(|(n, s)| (n.as_deref(), s.as_str()))
        .collect();
    if !mention_refs.is_empty()
        && let Err(e) = db.append_mention_edges(path, &mention_refs)
    {
        tracing::warn!("mention edge storage failed for {path}: {e}");
    }
    Ok(())
}
