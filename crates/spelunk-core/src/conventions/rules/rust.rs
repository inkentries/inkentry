//! Rust-specific convention heuristics.

use regex::Regex;
use std::sync::OnceLock;

use super::{
    ChunkSummary, ConventionRecord, count_cases, doc_coverage_record, dominant, function_names,
    naming_record, type_names,
};

// ── Compiled regex patterns ───────────────────────────────────────────────────

fn patterns() -> &'static RustPatterns {
    static P: OnceLock<RustPatterns> = OnceLock::new();
    P.get_or_init(RustPatterns::new)
}

struct RustPatterns {
    anyhow: Regex,
    thiserror: Regex,
    app_error: Regex,
    async_fn: Regex,
    tokio: Regex,
    cfg_test: Regex,
}

impl RustPatterns {
    fn new() -> Self {
        Self {
            anyhow: Regex::new(r"\banyhow\b").expect("anyhow regex"),
            thiserror: Regex::new(r"\bthiserror\b").expect("thiserror regex"),
            app_error: Regex::new(r"\bAppError\b").expect("AppError regex"),
            async_fn: Regex::new(r"\basync\s+fn\b").expect("async fn regex"),
            tokio: Regex::new(r"\btokio::").expect("tokio regex"),
            cfg_test: Regex::new(r"#\s*\[cfg\s*\(\s*test\s*\)\]").expect("cfg test regex"),
        }
    }
}

// ── Public entry point ────────────────────────────────────────────────────────

pub fn extract(chunks: &[&ChunkSummary], now: i64) -> Vec<ConventionRecord> {
    let lang = "rust";
    let mut records = Vec::new();

    // ── naming.functions ─────────────────────────────────────────────────────
    let fn_names = function_names(chunks);
    let (snake, camel, pascal, screaming, total_fn) = count_cases(&fn_names);
    let (dom_style, dom_count) = dominant(snake, camel, pascal, screaming);
    if let Some(r) = naming_record(lang, "naming.functions", dom_style, dom_count, total_fn, now) {
        records.push(r);
    }

    // ── naming.types ─────────────────────────────────────────────────────────
    let ty_names = type_names(chunks);
    let (snake_t, camel_t, pascal_t, screaming_t, total_ty) = count_cases(&ty_names);
    let (dom_ty, dom_ty_count) = dominant(snake_t, camel_t, pascal_t, screaming_t);
    if let Some(r) = naming_record(lang, "naming.types", dom_ty, dom_ty_count, total_ty, now) {
        records.push(r);
    }

    // ── error_handling ────────────────────────────────────────────────────────
    if let Some(r) = error_handling_record(chunks, now) {
        records.push(r);
    }

    // ── async ─────────────────────────────────────────────────────────────────
    if let Some(r) = async_record(chunks, now) {
        records.push(r);
    }

    // ── testing ───────────────────────────────────────────────────────────────
    if let Some(r) = testing_record(chunks, now) {
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

fn error_handling_record(chunks: &[&ChunkSummary], now: i64) -> Option<ConventionRecord> {
    let p = patterns();
    let mut anyhow_count = 0u32;
    let mut thiserror_count = 0u32;
    let mut app_error_count = 0u32;

    for c in chunks {
        if p.anyhow.is_match(&c.content) {
            anyhow_count += 1;
        }
        if p.thiserror.is_match(&c.content) {
            thiserror_count += 1;
        }
        if p.app_error.is_match(&c.content) {
            app_error_count += 1;
        }
    }

    let total = anyhow_count + thiserror_count + app_error_count;
    if total == 0 {
        return None;
    }

    let (description, dominant_count) = if anyhow_count >= thiserror_count
        && anyhow_count >= app_error_count
    {
        ("Error handling via anyhow::Result".to_string(), anyhow_count)
    } else if thiserror_count >= app_error_count {
        (
            "Error types defined with thiserror".to_string(),
            thiserror_count,
        )
    } else {
        (
            "Custom AppError type for error handling".to_string(),
            app_error_count,
        )
    };

    let confidence = dominant_count as f32 / total as f32;
    Some(ConventionRecord {
        language: "rust".to_string(),
        category: "error_handling".to_string(),
        description,
        confidence,
        evidence_count: dominant_count,
        extracted_at: now,
    })
}

fn async_record(chunks: &[&ChunkSummary], now: i64) -> Option<ConventionRecord> {
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
        .filter(|c| p.async_fn.is_match(&c.content))
        .count() as u32;

    let total = function_chunks.len() as u32;
    if total == 0 {
        return None;
    }

    let ratio = async_count as f32 / total as f32;
    if ratio <= 0.2 {
        return None;
    }

    // Detect runtime from content of all chunks.
    let has_tokio = chunks.iter().any(|c| p.tokio.is_match(&c.content));
    let runtime = if has_tokio { "tokio" } else { "async-std" };
    let confidence = ratio;

    Some(ConventionRecord {
        language: "rust".to_string(),
        category: "async".to_string(),
        description: format!("Async runtime: {runtime}"),
        confidence,
        evidence_count: async_count,
        extracted_at: now,
    })
}

fn testing_record(chunks: &[&ChunkSummary], now: i64) -> Option<ConventionRecord> {
    let p = patterns();
    // Detect files in test locations.
    let test_files_count = chunks
        .iter()
        .filter(|c| {
            c.file_path.ends_with("_test.rs")
                || c.file_path.contains("/tests/")
                || c.file_path.contains("tests/")
        })
        .count() as u32;

    let cfg_test_count = chunks
        .iter()
        .filter(|c| p.cfg_test.is_match(&c.content))
        .count() as u32;

    let total = test_files_count + cfg_test_count;
    if total == 0 {
        return None;
    }

    let (description, dominant_count) = if test_files_count >= cfg_test_count {
        (
            "Tests in separate *_test.rs / tests/ files".to_string(),
            test_files_count,
        )
    } else {
        (
            "Tests in #[cfg(test)] inline modules".to_string(),
            cfg_test_count,
        )
    };

    let confidence = dominant_count as f32 / total as f32;
    Some(ConventionRecord {
        language: "rust".to_string(),
        category: "testing".to_string(),
        description,
        confidence,
        evidence_count: dominant_count,
        extracted_at: now,
    })
}
