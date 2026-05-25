//! Per-language heuristic rule sets for convention extraction.

pub mod generic;
pub mod rust;
pub mod typescript;

use super::ConventionRecord;
use super::extractor::ChunkSummary;

// ── Shared helpers ────────────────────────────────────────────────────────────

/// Classify a symbol name into a case style.
/// Names shorter than 3 characters are treated as `CaseStyle::Unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseStyle {
    /// all_lower_with_underscores (no uppercase)
    SnakeCase,
    /// startsLower, containsUpper, no underscores
    CamelCase,
    /// StartsUpper, no underscores
    PascalCase,
    /// ALL_UPPER_WITH_UNDERSCORES
    ScreamingSnake,
    /// Anything else (mixed, very short, numeric-heavy, etc.)
    Unknown,
}

pub fn detect_case(name: &str) -> CaseStyle {
    if name.len() < 3 {
        return CaseStyle::Unknown;
    }
    let has_upper = name.chars().any(|c| c.is_uppercase());
    let has_lower = name.chars().any(|c| c.is_lowercase());
    let has_underscore = name.contains('_');
    let starts_upper = name.starts_with(|c: char| c.is_uppercase());

    if has_upper && !has_lower && has_underscore {
        CaseStyle::ScreamingSnake
    } else if starts_upper && !has_underscore {
        CaseStyle::PascalCase
    } else if !starts_upper && has_upper && !has_underscore {
        CaseStyle::CamelCase
    } else if has_lower && !has_upper {
        CaseStyle::SnakeCase
    } else {
        CaseStyle::Unknown
    }
}

/// Build a confidence-weighted `ConventionRecord` for naming conventions.
/// Returns `None` when there are no meaningful samples.
pub fn naming_record(
    language: &str,
    category: &str,
    dominant_style: CaseStyle,
    dominant_count: u32,
    total_named: u32,
    now: i64,
) -> Option<ConventionRecord> {
    if total_named == 0 || dominant_count == 0 {
        return None;
    }
    let confidence = dominant_count as f32 / total_named as f32;
    let style_name = match dominant_style {
        CaseStyle::SnakeCase => "snake_case",
        CaseStyle::CamelCase => "camelCase",
        CaseStyle::PascalCase => "PascalCase",
        CaseStyle::ScreamingSnake => "SCREAMING_SNAKE_CASE",
        CaseStyle::Unknown => return None,
    };
    let description = match category {
        "naming.functions" => format!("Functions use {style_name}"),
        "naming.types" => format!("Types use {style_name}"),
        _ => format!("Names use {style_name}"),
    };
    Some(ConventionRecord {
        language: language.to_string(),
        category: category.to_string(),
        description,
        confidence,
        evidence_count: dominant_count,
        extracted_at: now,
    })
}

/// Build a doc-coverage `ConventionRecord`.
pub fn doc_coverage_record(
    language: &str,
    with_docs: u32,
    total: u32,
    now: i64,
) -> Option<ConventionRecord> {
    if total == 0 {
        return None;
    }
    let ratio = with_docs as f32 / total as f32;
    let (level, confidence) = if ratio > 0.7 {
        ("high", ratio)
    } else if ratio >= 0.3 {
        ("medium", ratio.max(0.5))
    } else {
        ("low", (1.0 - ratio).max(0.5))
    };
    Some(ConventionRecord {
        language: language.to_string(),
        category: "docs".to_string(),
        description: format!("Doc comment coverage: {level}"),
        confidence,
        evidence_count: total,
        extracted_at: now,
    })
}

/// Count `CaseStyle` occurrences in a name slice.
/// Returns `(snake, camel, pascal, screaming, total_named)`.
pub fn count_cases(names: &[Option<&str>]) -> (u32, u32, u32, u32, u32) {
    let mut snake = 0u32;
    let mut camel = 0u32;
    let mut pascal = 0u32;
    let mut screaming = 0u32;
    let mut total = 0u32;
    for name in names.iter().flatten() {
        match detect_case(name) {
            CaseStyle::SnakeCase => {
                snake += 1;
                total += 1;
            }
            CaseStyle::CamelCase => {
                camel += 1;
                total += 1;
            }
            CaseStyle::PascalCase => {
                pascal += 1;
                total += 1;
            }
            CaseStyle::ScreamingSnake => {
                screaming += 1;
                total += 1;
            }
            CaseStyle::Unknown => {}
        }
    }
    (snake, camel, pascal, screaming, total)
}

/// Return the dominant style and count from raw counts.
/// Ties go to the first-listed (more conventional) style.
pub fn dominant(snake: u32, camel: u32, pascal: u32, screaming: u32) -> (CaseStyle, u32) {
    let mut best = (CaseStyle::Unknown, 0u32);
    for (style, count) in [
        (CaseStyle::SnakeCase, snake),
        (CaseStyle::CamelCase, camel),
        (CaseStyle::PascalCase, pascal),
        (CaseStyle::ScreamingSnake, screaming),
    ] {
        if count > best.1 {
            best = (style, count);
        }
    }
    best
}

/// Collect `ChunkSummary` slices into name samples for functions and types.
pub fn function_names<'a>(chunks: &[&'a ChunkSummary]) -> Vec<Option<&'a str>> {
    chunks
        .iter()
        .filter(|c| matches!(c.node_type.as_str(), "function" | "method"))
        .map(|c| c.name.as_deref())
        .collect()
}

pub fn type_names<'a>(chunks: &[&'a ChunkSummary]) -> Vec<Option<&'a str>> {
    chunks
        .iter()
        .filter(|c| {
            matches!(
                c.node_type.as_str(),
                "struct" | "enum" | "trait" | "class" | "interface" | "type_alias"
            )
        })
        .map(|c| c.name.as_deref())
        .collect()
}
