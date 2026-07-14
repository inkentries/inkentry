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

    /// Run all rule sets over `chunks` and return `ConventionRecord`s, one per
    /// `(language, category)`. Caller applies confidence / evidence thresholds.
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

        // Both the language-specific set and the always-run generic set emit
        // overlapping categories (naming.functions, docs); tsx chunks also route
        // through the typescript set, which self-labels every record "typescript",
        // colliding with the typescript group. Tag each record with its source so
        // the merge below can prefer language-specific over generic.
        let mut tagged: Vec<(Source, ConventionRecord)> = Vec::new();
        for (lang, lang_chunks) in &by_lang {
            let is_specific = matches!(*lang, "rust" | "typescript" | "tsx");
            let specific = match *lang {
                "rust" => Some(rules::rust::extract(lang_chunks, now)),
                "typescript" | "tsx" => Some(rules::typescript::extract(lang_chunks, now)),
                _ => None,
            };
            if let Some(recs) = specific {
                tagged.extend(recs.into_iter().map(|r| (Source::LanguageSpecific, r)));
            }
            // Generic rules run for every language: as the sole set for
            // unspecialised languages, and layered on top otherwise.
            let source = if is_specific {
                Source::Generic
            } else {
                Source::LanguageSpecific
            };
            tagged.extend(
                rules::generic::extract(lang_chunks, now)
                    .into_iter()
                    .map(|r| (source, r)),
            );
        }

        dedup_by_language_category(tagged)
    }
}

/// Provenance of a record, used to break ties when merging duplicates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// A per-language rule set, or the generic set acting as a language's sole set.
    LanguageSpecific,
    /// The generic set layered on top of a language-specific set.
    Generic,
}

/// Collapse records to one per `(language, category)`.
///
/// Language-specific records win over generic ones. Between two records of the
/// same source, the higher-confidence description is kept and evidence counts
/// are summed (e.g. tsx chunks routed through the typescript set). Output is
/// ordered by `(language, category)` for determinism.
fn dedup_by_language_category(tagged: Vec<(Source, ConventionRecord)>) -> Vec<ConventionRecord> {
    use std::collections::BTreeMap;

    let mut merged: BTreeMap<(String, String), (Source, ConventionRecord)> = BTreeMap::new();
    for (source, rec) in tagged {
        let key = (rec.language.clone(), rec.category.clone());
        match merged.get_mut(&key) {
            None => {
                merged.insert(key, (source, rec));
            }
            Some((kept_source, kept)) => {
                if source == Source::LanguageSpecific && *kept_source == Source::Generic {
                    // Language-specific replaces generic outright.
                    *kept_source = source;
                    *kept = rec;
                } else if source == *kept_source {
                    // Same source: aggregate evidence, keep higher-confidence fields.
                    kept.evidence_count = kept.evidence_count.saturating_add(rec.evidence_count);
                    if rec.confidence > kept.confidence {
                        kept.description = rec.description;
                        kept.confidence = rec.confidence;
                    }
                }
                // else: incoming is Generic, kept is LanguageSpecific — discard.
            }
        }
    }

    merged.into_values().map(|(_, rec)| rec).collect()
}

impl Default for ConventionExtractor {
    fn default() -> Self {
        Self::new()
    }
}
