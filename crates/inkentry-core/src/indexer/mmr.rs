//! Maximal-marginal-relevance selection for title-less chunks (tier 3).
//!
//! Markdown `Section`s and oversized sliding-window (`Verbatim`) chunks carry no
//! symbol name, so a structural summary has little to work from. Instead their
//! `summary:` slot is built by splitting the chunk into short units, embedding
//! the units, and selecting a representative subset by greedy MMR against the
//! chunk's **already-stored primary vector** as the centroid — never a fresh
//! whole-chunk embed, so tier 3 never enters the single-chunk seq² path that
//! OOMs on CPU. Only the pure, embedder-free selection and unit-splitting live
//! here; the embed + write pass wires them to the queue.

use crate::indexer::chunker::ChunkKind;

/// MMR relevance/diversity trade-off. Fixed and documented (not a config
/// surface): higher favours relevance to the centroid, lower favours diversity
/// among picks. Recorded as part of [`SUMMARY_SCHEME`], so a change to it is a
/// scheme change that invalidates stored tier-3 vectors.
///
/// [`SUMMARY_SCHEME`]: crate::indexer::summariser::SUMMARY_SCHEME
pub const MMR_LAMBDA: f32 = 0.5;

/// Upper bound on units extracted from one chunk, and on the characters of one
/// unit — a crafted or generated chunk cannot drive unbounded splitting work or
/// unbounded per-unit embed cost.
pub const MAX_UNITS: usize = 64;
const MAX_UNIT_CHARS: usize = 2000;
const MIN_UNIT_CHARS: usize = 3;

/// Split a title-less chunk into short units: sentences for Markdown, blank-line
/// separated statement groups for windowed code. Deterministic, bounded to
/// [`MAX_UNITS`]; each unit is trimmed and length-filtered.
pub fn split_into_units(content: &str, kind: &ChunkKind) -> Vec<String> {
    let raw = match kind {
        ChunkKind::Section => split_sentences(content),
        _ => split_statement_groups(content),
    };
    raw.into_iter()
        .map(|u| clamp_chars(u.trim(), MAX_UNIT_CHARS))
        .filter(|u| u.chars().count() >= MIN_UNIT_CHARS)
        .take(MAX_UNITS)
        .collect()
}

fn split_sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        cur.push(ch);
        if matches!(ch, '.' | '!' | '?' | '\n') {
            let sentence = cur.split_whitespace().collect::<Vec<_>>().join(" ");
            if !sentence.is_empty() {
                out.push(sentence);
            }
            cur.clear();
        }
    }
    let tail = cur.split_whitespace().collect::<Vec<_>>().join(" ");
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

fn split_statement_groups(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            if !cur.is_empty() {
                out.push(cur.join("\n"));
                cur.clear();
            }
        } else {
            cur.push(line);
        }
    }
    if !cur.is_empty() {
        out.push(cur.join("\n"));
    }
    out
}

fn clamp_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

/// Greedy maximal-marginal-relevance ranking of `unit_vectors` against
/// `centroid` (the chunk's stored primary vector). Returns every unit index in
/// selection order; the caller takes the prefix that fits the summary token cap.
///
/// Each step picks the unit maximising
/// `λ·sim(unit, centroid) − (1−λ)·max sim(unit, already-picked)`.
/// Ties are broken by unit index (the strictly-greater comparison keeps the
/// earliest), never by float equality or iteration order, so the ranking is
/// byte-identical for the same units and `λ`.
pub fn mmr_rank(unit_vectors: &[Vec<f32>], centroid: &[f32], lambda: f32) -> Vec<usize> {
    let n = unit_vectors.len();
    let relevance: Vec<f32> = unit_vectors.iter().map(|v| cosine(v, centroid)).collect();

    let mut selected: Vec<usize> = Vec::with_capacity(n);
    let mut remaining: Vec<usize> = (0..n).collect();

    while !remaining.is_empty() {
        let mut best_pos = 0usize;
        let mut best_score = f32::NEG_INFINITY;
        for (pos, &idx) in remaining.iter().enumerate() {
            let max_sim_to_selected = selected
                .iter()
                .map(|&s| cosine(&unit_vectors[idx], &unit_vectors[s]))
                .fold(0.0f32, f32::max);
            let score = lambda * relevance[idx] - (1.0 - lambda) * max_sim_to_selected;
            // Strictly greater: the earliest (lowest-index) unit wins a tie,
            // because `remaining` is kept in ascending index order.
            if score > best_score {
                best_score = score;
                best_pos = pos;
            }
        }
        selected.push(remaining.remove(best_pos));
    }
    selected
}

/// Cosine similarity of two equal-length vectors; 0.0 for a zero vector or a
/// length mismatch (never a NaN or a panic).
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_splits_into_sentences_bounded() {
        let content = "First sentence. Second one! A third? And more.";
        let units = split_into_units(content, &ChunkKind::Section);
        assert_eq!(
            units,
            vec!["First sentence.", "Second one!", "A third?", "And more.",]
        );
    }

    #[test]
    fn code_splits_into_statement_groups() {
        let content = "let a = 1;\nlet b = 2;\n\nfn helper() {\n    do_it();\n}\n";
        let units = split_into_units(content, &ChunkKind::Verbatim);
        assert_eq!(units.len(), 2, "two blank-line separated groups");
        assert!(units[0].contains("let a = 1;"));
        assert!(units[1].contains("fn helper()"));
    }

    #[test]
    fn unit_count_is_bounded() {
        let content = "x. ".repeat(1000);
        let units = split_into_units(&content, &ChunkKind::Section);
        assert!(units.len() <= MAX_UNITS, "unit count must be bounded");
    }

    #[test]
    fn mmr_ranks_most_relevant_first_then_diversifies() {
        // The centroid is distinct from every unit, so relevance and
        // redundancy are genuinely different quantities (when the centroid
        // equals a unit, λ=0.5 makes every marginal score collapse to zero).
        let centroid = vec![1.0, 0.2, 0.0];
        // unit 0: most relevant to the centroid.
        // unit 1: a scalar multiple of unit 0 — equally relevant but fully
        //         redundant once unit 0 is picked.
        // unit 2: less relevant, but orthogonal to unit 0 (diverse).
        let units = vec![
            vec![1.0, 0.0, 0.0],
            vec![0.5, 0.0, 0.0],
            vec![0.0, 1.0, 0.0],
        ];
        let order = mmr_rank(&units, &centroid, 0.5);
        assert_eq!(
            order[0], 0,
            "the unit closest to the centroid is picked first"
        );
        assert_eq!(
            order[1], 2,
            "diversity beats the redundant near-duplicate at λ=0.5"
        );
        assert_eq!(order.len(), 3, "ranking covers every unit");
    }

    #[test]
    fn mmr_breaks_ties_by_unit_index() {
        let centroid = vec![1.0, 0.0];
        // Two identical, equally-relevant units: the lower index must come first.
        let units = vec![vec![1.0, 0.0], vec![1.0, 0.0]];
        assert_eq!(mmr_rank(&units, &centroid, 0.5), vec![0, 1]);
    }

    #[test]
    fn mmr_is_byte_identical_across_runs() {
        let centroid = vec![0.2, 0.5, 0.1, 0.8];
        let units = vec![
            vec![0.1, 0.4, 0.2, 0.7],
            vec![0.9, 0.1, 0.0, 0.2],
            vec![0.3, 0.3, 0.3, 0.3],
        ];
        assert_eq!(
            mmr_rank(&units, &centroid, MMR_LAMBDA),
            mmr_rank(&units, &centroid, MMR_LAMBDA)
        );
    }

    #[test]
    fn cosine_handles_zero_and_mismatch_without_panic() {
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
        assert_eq!(cosine(&[1.0], &[1.0, 1.0]), 0.0);
    }
}
