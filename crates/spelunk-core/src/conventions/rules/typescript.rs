//! TypeScript / TSX convention heuristics.

use regex::Regex;
use std::sync::OnceLock;

use super::{
    ChunkSummary, ConventionRecord, count_cases, doc_coverage_record, dominant, function_names,
    naming_record, type_names,
};

fn patterns() -> &'static TsPatterns {
    static P: OnceLock<TsPatterns> = OnceLock::new();
    P.get_or_init(TsPatterns::new)
}

struct TsPatterns {
    async_keyword: Regex,
}

impl TsPatterns {
    fn new() -> Self {
        Self {
            async_keyword: Regex::new(r"\basync\b").expect("async regex"),
        }
    }
}

// ── Public entry point ────────────────────────────────────────────────────────

pub fn extract(chunks: &[&ChunkSummary], lang: &str, now: i64) -> Vec<ConventionRecord> {
    let mut records = Vec::new();

    // ── naming.functions ─────────────────────────────────────────────────────
    let fn_names = function_names(chunks);
    let (snake, camel, pascal, screaming, total_fn) = count_cases(&fn_names);
    let (dom_style, dom_count) = dominant(snake, camel, pascal, screaming);
    if let Some(r) = naming_record(
        lang,
        "naming.functions",
        dom_style,
        dom_count,
        total_fn,
        now,
    ) {
        records.push(r);
    }

    // ── naming.types ─────────────────────────────────────────────────────────
    let ty_names = type_names(chunks);
    let (snake_t, camel_t, pascal_t, screaming_t, total_ty) = count_cases(&ty_names);
    let (dom_ty, dom_ty_count) = dominant(snake_t, camel_t, pascal_t, screaming_t);
    if let Some(r) = naming_record(lang, "naming.types", dom_ty, dom_ty_count, total_ty, now) {
        records.push(r);
    }

    // ── async ─────────────────────────────────────────────────────────────────
    if let Some(r) = async_record(chunks, lang, now) {
        records.push(r);
    }

    // ── testing ───────────────────────────────────────────────────────────────
    if let Some(r) = testing_record(chunks, lang, now) {
        records.push(r);
    }

    // ── docs ──────────────────────────────────────────────────────────────────
    let function_chunks: Vec<_> = chunks
        .iter()
        .filter(|c| matches!(c.node_type.as_str(), "function" | "method"))
        .copied()
        .collect();
    if !function_chunks.is_empty() {
        let with_docs = function_chunks.iter().filter(|c| c.has_docstring).count() as u32;
        let total = function_chunks.len() as u32;
        if let Some(r) = doc_coverage_record(lang, with_docs, total, now) {
            records.push(r);
        }
    }

    records
}

// ── Private helpers ───────────────────────────────────────────────────────────

fn async_record(chunks: &[&ChunkSummary], lang: &str, now: i64) -> Option<ConventionRecord> {
    let p = patterns();
    let function_chunks: Vec<_> = chunks
        .iter()
        .filter(|c| matches!(c.node_type.as_str(), "function" | "method"))
        .copied()
        .collect();
    if function_chunks.is_empty() {
        return None;
    }
    let async_count = function_chunks
        .iter()
        .filter(|c| p.async_keyword.is_match(&c.content))
        .count() as u32;
    let total = function_chunks.len() as u32;
    let ratio = async_count as f32 / total as f32;
    if ratio <= 0.2 {
        return None;
    }
    Some(ConventionRecord {
        language: lang.to_string(),
        category: "async".to_string(),
        description: "async/await is widely used".to_string(),
        confidence: ratio,
        evidence_count: async_count,
        extracted_at: now,
    })
}

fn testing_record(chunks: &[&ChunkSummary], lang: &str, now: i64) -> Option<ConventionRecord> {
    let test_count = chunks
        .iter()
        .filter(|c| {
            c.file_path.ends_with(".test.ts")
                || c.file_path.ends_with(".test.tsx")
                || c.file_path.ends_with(".spec.ts")
                || c.file_path.ends_with(".spec.tsx")
                || c.file_path.contains("/__tests__/")
                || c.file_path.contains("__tests__/")
        })
        .count() as u32;

    if test_count == 0 {
        return None;
    }

    Some(ConventionRecord {
        language: lang.to_string(),
        category: "testing".to_string(),
        description: "Tests in .test.ts / .spec.ts / __tests__/ files".to_string(),
        confidence: 1.0,
        evidence_count: test_count,
        extracted_at: now,
    })
}
