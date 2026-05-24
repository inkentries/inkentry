//! Convention extraction: heuristic AST pass over indexed chunks.
//!
//! Reads stored chunks from spelunk.db, dispatches to per-language rule sets,
//! and writes `ConventionRecord`s to the `conventions` table.
//!
//! No LLM, no network calls — pure heuristics.

mod extractor;
pub mod rules;

pub use extractor::ConventionExtractor;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::storage::Database;

/// A single detected convention.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConventionRecord {
    pub language: String,
    /// Dot-separated category, e.g. `"naming.functions"`, `"error_handling"`.
    pub category: String,
    /// Human-readable description, e.g. `"Functions use snake_case"`.
    pub description: String,
    /// 0.0–1.0; only records with `>= 0.5` are stored.
    pub confidence: f32,
    /// Number of evidence data points behind this record.
    pub evidence_count: u32,
    /// Unix timestamp when extraction ran.
    pub extracted_at: i64,
}

/// Run the full extraction pipeline.
///
/// Reads all chunks from `db`, dispatches to per-language extractors, and
/// writes results to the `conventions` table (delete-all + re-insert).
///
/// Failures are surfaced to the caller; the index phase wraps this in a
/// non-fatal warning.
pub fn run_extraction(db: &Database) -> Result<Vec<ConventionRecord>> {
    let chunks = db.all_chunks_for_conventions()?;

    let records = extractor::ConventionExtractor::new()
        .extract(&chunks)
        .into_iter()
        .filter(|r| r.confidence >= 0.5 && r.evidence_count >= 5)
        .collect::<Vec<_>>();

    db.replace_conventions(&records)?;
    Ok(records)
}
