//! Language-agnostic convention heuristics.
//! Applied to languages that have no dedicated rule set, and also layered on
//! top of language-specific rules (for naming / docs).

use super::{
    ChunkSummary, ConventionRecord, count_cases, doc_coverage_record, dominant, function_names,
    naming_record,
};

pub fn extract(chunks: &[&ChunkSummary], now: i64) -> Vec<ConventionRecord> {
    // Use the first chunk's language as the label (all chunks here share it).
    let lang = chunks
        .first()
        .map(|c| c.language.as_str())
        .unwrap_or("unknown");

    let mut records = Vec::new();

    // ── naming.functions ─────────────────────────────────────────────────────
    let fn_names = function_names(chunks);
    let (snake, camel, pascal, screaming, total_fn) = count_cases(&fn_names);
    let (dom_style, dom_count) = dominant(snake, camel, pascal, screaming);
    if let Some(r) = naming_record(lang, "naming.functions", dom_style, dom_count, total_fn, now) {
        records.push(r);
    }

    // ── docs ──────────────────────────────────────────────────────────────────
    let callable_chunks: Vec<_> = chunks
        .iter()
        .filter(|c| matches!(c.node_type.as_str(), "function" | "method"))
        .copied()
        .collect();
    if !callable_chunks.is_empty() {
        let with_docs = callable_chunks.iter().filter(|c| c.has_docstring).count() as u32;
        let total = callable_chunks.len() as u32;
        if let Some(r) = doc_coverage_record(lang, with_docs, total, now) {
            records.push(r);
        }
    }

    records
}
