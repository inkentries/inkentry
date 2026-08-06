//! Convention extraction: heuristic AST pass over indexed chunks.
//!
//! Reads stored chunks from inkentry.db, dispatches to per-language rule sets,
//! and writes `ConventionRecord`s to the `conventions` table.
//!
//! No LLM, no network calls — pure heuristics.

pub mod extractor;
pub mod rules;

pub use extractor::{ChunkSummary, ConventionExtractor};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::storage::{ConventionRow, Database, RawChunkRow, has_doc_prefix};

/// A single detected convention (logical / API type).
///
/// The storage layer uses `crate::storage::ConventionRow` internally to avoid
/// a circular module dependency.  `run_extraction` converts between them.
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

impl From<ConventionRecord> for ConventionRow {
    fn from(r: ConventionRecord) -> Self {
        ConventionRow {
            language: r.language,
            category: r.category,
            description: r.description,
            confidence: r.confidence,
            evidence_count: r.evidence_count,
            extracted_at: r.extracted_at,
        }
    }
}

impl From<ConventionRow> for ConventionRecord {
    fn from(r: ConventionRow) -> Self {
        ConventionRecord {
            language: r.language,
            category: r.category,
            description: r.description,
            confidence: r.confidence,
            evidence_count: r.evidence_count,
            extracted_at: r.extracted_at,
        }
    }
}

/// Convert `RawChunkRow` (storage type) into `ChunkSummary` (extraction type).
fn to_chunk_summary(row: RawChunkRow) -> ChunkSummary {
    let has_docstring = has_doc_prefix(&row.content);
    ChunkSummary {
        language: row.language,
        node_type: row.node_type,
        name: row.name,
        content: row.content,
        file_path: row.file_path,
        has_docstring,
    }
}

/// Run the full extraction pipeline.
///
/// Reads all chunks from `db`, dispatches to per-language extractors, and
/// writes results to the `conventions` table (delete-all + re-insert).
///
/// Failures are surfaced to the caller; the index phase wraps this in a
/// non-fatal warning.
pub fn run_extraction(db: &Database) -> Result<Vec<ConventionRecord>> {
    let raw_chunks = db.all_chunks_for_conventions()?;
    let chunks: Vec<ChunkSummary> = raw_chunks.into_iter().map(to_chunk_summary).collect();

    let records: Vec<ConventionRecord> = ConventionExtractor::new()
        .extract(&chunks)
        .into_iter()
        .filter(|r| r.confidence >= 0.5 && r.evidence_count >= 5)
        .collect();

    let rows: Vec<ConventionRow> = records.iter().cloned().map(ConventionRow::from).collect();
    db.replace_conventions(&rows)?;
    Ok(records)
}

/// Fetch stored conventions from the DB and return them as `ConventionRecord`s.
pub fn list_conventions(db: &Database, language: Option<&str>) -> Result<Vec<ConventionRecord>> {
    let rows = db.list_conventions(language)?;
    Ok(rows.into_iter().map(ConventionRecord::from).collect())
}
