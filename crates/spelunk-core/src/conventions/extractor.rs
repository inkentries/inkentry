//! `ConventionExtractor` — aggregates evidence from stored chunks and produces
//! `ConventionRecord`s by dispatching to per-language rule sets.

use super::ConventionRecord;
use super::rules;

/// A lightweight chunk summary passed to the extractors.
/// Contains only the fields the heuristics need.
#[derive(Debug, Clone)]
pub struct ChunkSummary {
    pub language: String,
    pub node_type: String,
    pub name: Option<String>,
    pub content: String,
    pub file_path: String,
    pub has_docstring: bool,
}

/// Aggregates `ChunkSummary` slices and dispatches to per-language rule sets.
pub struct ConventionExtractor;

impl ConventionExtractor {
    pub fn new() -> Self {
        Self
    }

    /// Run all rule sets over `chunks` and return raw `ConventionRecord`s.
    /// Caller is responsible for applying confidence / evidence-count thresholds.
    pub fn extract(&self, chunks: &[ChunkSummary]) -> Vec<ConventionRecord> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        // Group chunks by language.
        let mut by_lang: std::collections::HashMap<&str, Vec<&ChunkSummary>> =
            std::collections::HashMap::new();
        for c in chunks {
            by_lang.entry(c.language.as_str()).or_default().push(c);
        }

        let mut records = Vec::new();
        for (lang, lang_chunks) in &by_lang {
            let mut lang_records = match *lang {
                "rust" => rules::rust::extract(lang_chunks, now),
                "typescript" | "tsx" => rules::typescript::extract(lang_chunks, now),
                _ => rules::generic::extract(lang_chunks, now),
            };
            // Generic rules always run as a fallback for all languages.
            if *lang != "rust" && *lang != "typescript" && *lang != "tsx" {
                // Already handled by the generic branch above.
            } else {
                // Append generic rules on top of language-specific ones.
                let mut generic = rules::generic::extract(lang_chunks, now);
                lang_records.append(&mut generic);
            }
            records.extend(lang_records);
        }
        records
    }
}

impl Default for ConventionExtractor {
    fn default() -> Self {
        Self::new()
    }
}
