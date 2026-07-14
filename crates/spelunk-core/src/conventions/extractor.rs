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

        // Group by canonical language, so every rule set sees one group per label.
        let mut by_lang: std::collections::HashMap<&str, Vec<&ChunkSummary>> =
            std::collections::HashMap::new();
        for c in chunks {
            by_lang
                .entry(canonical_language(&c.language))
                .or_default()
                .push(c);
        }

        // The language-specific set and the always-run generic set emit
        // overlapping categories (naming.functions, docs). Tag each record with
        // its source so the merge below can prefer language-specific over generic.
        let mut tagged: Vec<(Source, ConventionRecord)> = Vec::new();
        for (lang, lang_chunks) in &by_lang {
            let specific = match *lang {
                "rust" => Some(rules::rust::extract(lang_chunks, lang, now)),
                "typescript" => Some(rules::typescript::extract(lang_chunks, lang, now)),
                _ => None,
            };
            let is_specific = specific.is_some();
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
                rules::generic::extract(lang_chunks, lang, now)
                    .into_iter()
                    .map(|r| (source, r)),
            );
        }

        dedup_by_language_category(tagged)
    }
}

/// Fold dialects onto the language they share conventions with.
/// tsx is typescript plus JSX: naming, async, testing and docs are identical,
/// so both surface under one label instead of duplicating every record.
fn canonical_language(lang: &str) -> &str {
    match lang {
        "tsx" => "typescript",
        other => other,
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
/// Language-specific records win over generic ones. Two records of the same
/// source cannot collide: there is one group per canonical language, each rule
/// set labels with that group's language and emits each category at most once.
/// Output is ordered by `(language, category)` for determinism.
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
                // Language-specific replaces generic outright; the reverse is
                // discarded. Never merge two partial views: confidence is a rate
                // over a group's chunks, so it is only correct when pooled by the
                // rule set itself, not recombined here.
                if source == Source::LanguageSpecific && *kept_source == Source::Generic {
                    *kept_source = source;
                    *kept = rec;
                }
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
