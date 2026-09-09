//! Cross-corpus rank fusion for unified search (ADR-081).
//!
//! Two retrieval pipelines produce two independently-ranked lists — code chunks
//! and memory entries — whose per-corpus relevance numbers are on incomparable
//! scales (a code-search `distance` from one instruction prefix versus a QA
//! `distance` from another). Merging by those numbers is silently wrong. This
//! module fuses the two lists by **rank position only**: each item's score is
//! `1 / (RRF_K + corpus_rank)`, so the cross-corpus order never reads the
//! incomparable magnitudes. With equal weights and disjoint corpora this is a
//! pure interleave (code rank *i* ties memory rank *i*, broken code-before-
//! memory), and the whole merge is a total, float-free, deterministic order.

use inkentry_core::search::{RRF_K, SearchResult};
use inkentry_core::storage::memory::Note;
use serde::Serialize;

/// One entry in the fused, heterogeneous result list — a `SearchResult` or a
/// `Note` under its own key, tagged with a `type` discriminator and its fusion
/// metadata. Ranked members carry `fused_rank`/`fused_score`/`corpus_rank`;
/// `--graph` enrichment neighbours are appended after the ranked members with
/// all three `null`, since they are attachments, not ranked members.
#[derive(Debug, Serialize)]
pub(crate) struct UnifiedResult {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub fused_rank: Option<usize>,
    pub fused_score: Option<f64>,
    pub corpus_rank: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<SearchResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<Note>,
}

enum Payload {
    Code(SearchResult),
    Memory(Note),
}

struct Ranked {
    corpus_rank: usize,
    /// Code before memory at equal `corpus_rank` (the cross-corpus tie-break).
    corpus_priority: u8,
    payload: Payload,
}

/// Fuse the two per-corpus ranked lists into one, truncated to `limit`.
///
/// `code` and `memory` must already be in each corpus's own ranked order; their
/// 1-based positions become `corpus_rank`. The merge is stable-sorted by
/// `(corpus_rank, corpus_priority)` — equivalent to `fused_score` descending but
/// with no float comparison — so it is a total, deterministic order independent
/// of `HashMap` iteration or float equality.
pub(crate) fn fuse(code: Vec<SearchResult>, memory: Vec<Note>, limit: usize) -> Vec<UnifiedResult> {
    let mut ranked: Vec<Ranked> = Vec::with_capacity(code.len() + memory.len());
    for (i, r) in code.into_iter().enumerate() {
        ranked.push(Ranked {
            corpus_rank: i + 1,
            corpus_priority: 0,
            payload: Payload::Code(r),
        });
    }
    for (i, n) in memory.into_iter().enumerate() {
        ranked.push(Ranked {
            corpus_rank: i + 1,
            corpus_priority: 1,
            payload: Payload::Memory(n),
        });
    }

    // Stable sort keeps each corpus's internal order at equal keys; the key
    // orders by rank first, then code-before-memory. No float comparison.
    ranked.sort_by_key(|r| (r.corpus_rank, r.corpus_priority));
    ranked.truncate(limit);

    ranked
        .into_iter()
        .enumerate()
        .map(|(idx, r)| {
            let fused_score = 1.0 / (RRF_K + r.corpus_rank as f64);
            let (code, memory) = match r.payload {
                Payload::Code(c) => (Some(c), None),
                Payload::Memory(m) => (None, Some(m)),
            };
            UnifiedResult {
                kind: if code.is_some() { "code" } else { "memory" },
                fused_rank: Some(idx + 1),
                fused_score: Some(fused_score),
                corpus_rank: Some(r.corpus_rank),
                code,
                memory,
            }
        })
        .collect()
}

/// Wrap `--graph` enrichment neighbours as unranked appendix members: `type`
/// `code`, `from_graph` already set by the caller, and every fusion-metadata
/// field `null`.
pub(crate) fn graph_appendix(neighbours: Vec<SearchResult>) -> Vec<UnifiedResult> {
    neighbours.into_iter().map(unranked_code).collect()
}

/// The memory side of the same rule: `--expand-graph` neighbours and
/// cross-project entries were reached from a hit or picked by tag, never ranked
/// against the query, so they get null fusion metadata rather than a
/// `corpus_rank` that would let them displace a ranked result.
pub(crate) fn memory_appendix(attachments: Vec<Note>) -> Vec<UnifiedResult> {
    attachments.into_iter().map(unranked_memory).collect()
}

fn unranked_code(r: SearchResult) -> UnifiedResult {
    UnifiedResult {
        kind: "code",
        fused_rank: None,
        fused_score: None,
        corpus_rank: None,
        code: Some(r),
        memory: None,
    }
}

fn unranked_memory(n: Note) -> UnifiedResult {
    UnifiedResult {
        kind: "memory",
        fused_rank: None,
        fused_score: None,
        corpus_rank: None,
        code: None,
        memory: Some(n),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(chunk_id: i64, distance: f32) -> SearchResult {
        SearchResult {
            chunk_id,
            file_path: format!("src/f{chunk_id}.rs"),
            language: "rust".into(),
            node_type: "function".into(),
            name: Some(format!("fn_{chunk_id}")),
            start_line: 1,
            end_line: 2,
            content: "code".into(),
            distance,
            from_graph: false,
            governing_specs: vec![],
            token_count: 0,
            project_name: None,
            project_path: None,
            summary: None,
        }
    }

    fn note(id: &str, distance: f64) -> Note {
        let title = format!("note {id}");
        Note {
            id: id.parse().unwrap(),
            entity_id: crate::storage::entity_id("decision", &title, "body"),
            kind: "decision".into(),
            title,
            body: "body".into(),
            tags: vec![],
            linked_files: vec![],
            created_at: 0,
            status: "active".into(),
            superseded_by: None,
            source_ref: None,
            valid_at: None,
            invalid_at: None,
            distance: Some(distance),
            score: None,
            source_project: None,
            source_project_path: None,
            remote_id: None,
        }
    }

    // Code rank i ties memory rank i, resolved code-before-memory, so a
    // balanced pair of lists interleaves code, memory, code, memory…
    #[test]
    fn interleaves_code_before_memory_at_equal_rank() {
        let out = fuse(
            vec![code(10, 0.1), code(11, 0.2)],
            vec![note("m20", 5.0), note("m21", 6.0)],
            10,
        );
        let types: Vec<&str> = out.iter().map(|u| u.kind).collect();
        assert_eq!(types, vec!["code", "memory", "code", "memory"]);
        assert_eq!(out[0].code.as_ref().unwrap().chunk_id, 10);
        assert_eq!(out[1].memory.as_ref().unwrap().id.to_string(), "m20");
        assert_eq!(out[2].code.as_ref().unwrap().chunk_id, 11);
        assert_eq!(out[3].memory.as_ref().unwrap().id.to_string(), "m21");
        let ranks: Vec<Option<usize>> = out.iter().map(|u| u.fused_rank).collect();
        assert_eq!(ranks, vec![Some(1), Some(2), Some(3), Some(4)]);
    }

    // fused_score is exactly 1/(RRF_K + corpus_rank); code rank 1 and memory
    // rank 1 therefore carry the SAME fused_score (the tie the priority breaks).
    #[test]
    fn fused_score_is_one_over_k_plus_corpus_rank() {
        let out = fuse(vec![code(1, 0.0)], vec![note("m2", 0.0)], 10);
        let expect_r1 = 1.0 / (60.0 + 1.0);
        assert!((out[0].fused_score.unwrap() - expect_r1).abs() < 1e-12);
        assert_eq!(out[0].corpus_rank, Some(1));
        assert!((out[1].fused_score.unwrap() - expect_r1).abs() < 1e-12);
        assert_eq!(out[1].corpus_rank, Some(1));
    }

    // Fusion is rank-based, never distance-based: a code item with a huge
    // per-corpus distance but corpus_rank 1 still outranks a memory item with a
    // tiny distance but corpus_rank 1 — the raw magnitudes are never compared.
    #[test]
    fn orders_by_rank_not_by_incomparable_distance() {
        let out = fuse(vec![code(1, 999.0)], vec![note("m2", 0.0001)], 10);
        assert_eq!(out[0].kind, "code");
        assert_eq!(out[0].fused_rank, Some(1));
    }

    #[test]
    fn code_only_corpus_yields_pure_code_list() {
        let out = fuse(vec![code(1, 0.1), code(2, 0.2)], vec![], 10);
        assert!(out.iter().all(|u| u.kind == "code"));
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn memory_only_corpus_yields_pure_memory_list() {
        let out = fuse(vec![], vec![note("m1", 1.0), note("m2", 2.0)], 10);
        assert!(out.iter().all(|u| u.kind == "memory"));
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn truncates_to_limit() {
        let out = fuse(
            vec![code(1, 0.1), code(2, 0.2)],
            vec![note("m3", 1.0), note("m4", 2.0)],
            3,
        );
        assert_eq!(out.len(), 3);
        // Interleave: code1, mem3, code2 — the 4th (mem4) is dropped.
        assert_eq!(out[2].code.as_ref().unwrap().chunk_id, 2);
    }

    // Same inputs must serialise byte-identically across runs (no HashMap /
    // float-equality dependence in the merge).
    #[test]
    fn deterministic_ordering_across_runs() {
        let build = || {
            fuse(
                vec![code(1, 0.5), code(2, 0.1), code(3, 0.9)],
                vec![note("m4", 3.0), note("m5", 1.0)],
                10,
            )
        };
        let a = serde_json::to_string(&build()).unwrap();
        let b = serde_json::to_string(&build()).unwrap();
        assert_eq!(a, b);
    }

    // Every ranked member carries the discriminator and all three fusion-metadata
    // fields, and exactly one of code/memory is present, matching `type`.
    #[test]
    fn ranked_member_envelope_shape() {
        let out = fuse(vec![code(1, 0.1)], vec![note("m2", 1.0)], 10);
        for u in &out {
            let v = serde_json::to_value(u).unwrap();
            assert!(v.get("type").is_some());
            assert!(v.get("fused_rank").unwrap().is_number());
            assert!(v.get("fused_score").unwrap().is_number());
            assert!(v.get("corpus_rank").unwrap().is_number());
            let has_code = v.get("code").is_some();
            let has_memory = v.get("memory").is_some();
            assert!(has_code ^ has_memory, "exactly one payload key");
            let ty = v.get("type").unwrap().as_str().unwrap();
            assert_eq!(has_code, ty == "code");
        }
    }

    // A memory attachment must carry the same null fusion metadata as a code
    // graph neighbour: no corpus_rank, so it cannot displace a ranked result.
    #[test]
    fn memory_appendix_members_are_unranked_memory() {
        let out = memory_appendix(vec![note("m7", 1.0)]);
        let v = serde_json::to_value(&out[0]).unwrap();
        assert_eq!(v.get("type").unwrap(), "memory");
        assert!(v.get("fused_rank").unwrap().is_null());
        assert!(v.get("fused_score").unwrap().is_null());
        assert!(v.get("corpus_rank").unwrap().is_null());
        assert!(v.get("memory").is_some());
    }

    // Graph-appendix members append after the ranked list with null fusion
    // metadata and type "code".
    #[test]
    fn graph_appendix_members_are_unranked_code() {
        let mut neighbour = code(99, 0.0);
        neighbour.from_graph = true;
        let out = graph_appendix(vec![neighbour]);
        assert_eq!(out.len(), 1);
        let v = serde_json::to_value(&out[0]).unwrap();
        assert_eq!(v.get("type").unwrap(), "code");
        assert!(v.get("fused_rank").unwrap().is_null());
        assert!(v.get("fused_score").unwrap().is_null());
        assert!(v.get("corpus_rank").unwrap().is_null());
        assert_eq!(v.get("code").unwrap().get("from_graph").unwrap(), true);
    }
}
