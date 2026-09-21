use anyhow::Result;

use super::notes::{row_to_note, row_to_note_with_distance};
use super::{MemoryStore, Note, NoteId};

/// The within-corpus relevance floor for a memory candidate's vector distance
/// to the QA-prefixed query embedding (ADR-083). `note_embeddings` is a `vec0`
/// `FLOAT[896]` table with the default L2 metric over L2-normalised vectors, so
/// a reported `distance` is `sqrt(2 - 2·cos)`; `1.2032` L2 is cosine `0.2762`.
/// Calibrated 2026-08-11 against `F2LLM-v2-330M@896`
/// ([`crate::embeddings::MODEL_ID`]) under the QA instruction prefix "Given a
/// question, retrieve passages that answer the question", from 10,000
/// (CodeSearchNet query, unrelated note) negative pairs and 20 hand-written
/// paraphrase positives (see `docs/adr/083-memory-relevance-gate-in-unified-search.md`).
/// Changing `MODEL_ID` or that instruction string invalidates the calibration.
pub const MEMORY_MAX_QA_DISTANCE: f64 = 1.2032;

impl MemoryStore {
    /// Semantic KNN search. Returns active notes ordered by ascending distance.
    /// When `as_of` is `Some(ts)`, only entries valid at that Unix timestamp are returned.
    pub fn search(&self, query_blob: &[u8], limit: usize, as_of: Option<i64>) -> Result<Vec<Note>> {
        let limit = limit.min(100);
        // A point-in-time query is governed entirely by the temporal window,
        // independent of archived status: an entry superseded/archived AFTER T
        // was live at T and must be returned. So with `as_of` set the
        // active-only gate is dropped and the window alone filters. COALESCE
        // reads a NULL valid_at (no explicit --valid-at) as created_at rather
        // than treating NULL as "valid since forever".
        let where_clause = if as_of.is_some() {
            "WHERE COALESCE(n.valid_at, n.created_at) <= ?2 AND (n.invalid_at IS NULL OR n.invalid_at > ?2)"
        } else {
            "WHERE n.status = 'active'"
        };
        let sql = format!(
            "WITH knn AS (
                 SELECT note_id, distance
                 FROM   note_embeddings
                 WHERE  embedding MATCH ?1
                   AND  k = {limit}
             )
             SELECT n.uuid, n.kind, n.title, n.body, n.tags, n.linked_files,
                    n.created_at, n.status, n.superseded_by, n.source_ref,
                    n.valid_at, n.invalid_at, n.entity_id, CAST(k.distance AS REAL)
             FROM   knn k
             JOIN   notes n ON n.id = k.note_id
             {where_clause}
             ORDER  BY k.distance, n.uuid"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let notes = if let Some(ts) = as_of {
            stmt.query_map(rusqlite::params![query_blob, ts], row_to_note_with_distance)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            stmt.query_map(rusqlite::params![query_blob], row_to_note_with_distance)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(notes)
    }

    /// BM25 full-text search over notes (title, body, tags).
    /// Returns active notes ordered by descending relevance.
    /// When `as_of` is `Some(ts)`, only entries valid at that Unix timestamp are returned.
    pub fn search_text(&self, query: &str, limit: usize, as_of: Option<i64>) -> Result<Vec<Note>> {
        let limit = limit.min(1_000);
        // See `search`: with `as_of` set the temporal window filters on its own,
        // independent of archived status, so a since-superseded entry live at T
        // is still retrieved; COALESCE reads a NULL valid_at as created_at.
        let live_clause = if as_of.is_some() {
            "COALESCE(n.valid_at, n.created_at) <= ?2 AND (n.invalid_at IS NULL OR n.invalid_at > ?2)"
        } else {
            "n.status = 'active'"
        };
        let sql = format!(
            "SELECT n.uuid, n.kind, n.title, n.body, n.tags, n.linked_files,
                    n.created_at, n.status, n.superseded_by, n.source_ref,
                    n.valid_at, n.invalid_at, n.entity_id, bm25(memory_fts) AS bm25_score
             FROM memory_fts
             JOIN notes n ON memory_fts.rowid = n.id
             WHERE memory_fts MATCH ?1
               AND {live_clause}
             ORDER BY bm25_score, n.uuid
             LIMIT {limit}"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let fts_query = crate::utils::fts5_quote_literal(query);
        let notes = if let Some(ts) = as_of {
            stmt.query_map(rusqlite::params![fts_query, ts], |row| {
                let bm25_score: f64 = row.get(13)?;
                let mut note = row_to_note(row)?;
                note.distance = Some(-bm25_score);
                Ok(note)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            stmt.query_map(rusqlite::params![fts_query], |row| {
                let bm25_score: f64 = row.get(13)?;
                let mut note = row_to_note(row)?;
                // Negate so that higher relevance → lower distance (ascending convention).
                note.distance = Some(-bm25_score);
                Ok(note)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(notes)
    }

    /// Vector-only memory retrieval, admitted candidates ranked by RRF (ADR-083).
    ///
    /// An earlier version of this method fused FTS5 BM25 with vector KNN. That
    /// lexical door is retired: `memory_fts` matches the whole query as one
    /// contiguous phrase, and measurement found it matched 0 of 500 negative and
    /// 0 of 20 positive natural-language queries — inert in practice, not merely
    /// weak. `query` is accepted for trait-signature parity with the code
    /// corpus's hybrid search and is otherwise unused.
    ///
    /// When `gate` is `true`, a vector candidate is admitted only if its
    /// distance to the QA-prefixed query embedding is at most
    /// [`MEMORY_MAX_QA_DISTANCE`] — this is what stops an unrelated memory store
    /// from taking half of every unified-search page. `gate` is `false` for
    /// `--only-memory`, which has no code corpus to protect slots from and is an
    /// explicit request to see the full page (ADR-083 decision 5).
    ///
    /// RRF score: `1 / (k + rank_i)` where `k` is the shared [`crate::search::RRF_K`].
    /// With one candidate source the ranking is already distance order; RRF
    /// keeps `score`/`distance` in the same shape hybrid search has always
    /// returned. When `as_of` is `Some(ts)`, only entries valid at that
    /// timestamp are considered.
    pub fn search_hybrid(
        &self,
        query_blob: &[u8],
        _query: &str,
        limit: usize,
        as_of: Option<i64>,
        gate: bool,
    ) -> Result<Vec<Note>> {
        use std::collections::HashMap;

        let candidates = (limit * 3).max(20);

        let vec_results = self.search(query_blob, candidates, as_of)?;
        let vec_results: Vec<Note> = if gate {
            vec_results
                .into_iter()
                .filter(|n| n.distance.is_some_and(|d| d <= MEMORY_MAX_QA_DISTANCE))
                .collect()
        } else {
            vec_results
        };

        const K: f64 = crate::search::RRF_K;

        let mut scores: HashMap<NoteId, f64> = HashMap::new();
        let mut by_id: HashMap<NoteId, Note> = HashMap::new();

        for (rank, note) in vec_results.into_iter().enumerate() {
            let rrf = 1.0 / (K + (rank + 1) as f64);
            *scores.entry(note.id.clone()).or_insert(0.0) += rrf;
            by_id.entry(note.id.clone()).or_insert(note);
        }

        // Sort descending by RRF score, take top `limit`. `scores` is a HashMap
        // whose iteration order is reseeded per instance; the id tie-break keeps
        // this order reproducible rather than a reshuffle per call, and NoteId
        // is the backend-minted UUID, so the order agrees across machines too.
        let mut ranked: Vec<(NoteId, f64)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        ranked.truncate(limit);

        let results = ranked
            .into_iter()
            .filter_map(|(id, rrf_score)| {
                by_id.remove(&id).map(|mut n| {
                    n.score = Some(rrf_score);
                    // Keep distance as inverted RRF so callers can sort ascending.
                    n.distance = Some(1.0 / rrf_score);
                    n
                })
            })
            .collect();

        Ok(results)
    }

    /// Full-text (FTS5) search over ALL notes regardless of status, for the
    /// timeline view. Returns entries whose title/body/tags are relevant to
    /// `query`, ordered by `COALESCE(valid_at, created_at) ASC` so the caller
    /// can trace how understanding of a topic evolved over time.
    ///
    /// This reuses the same FTS5 matcher as [`MemoryStore::search_text`] (the
    /// no-server text path behind `memory search --mode text`), via
    /// [`crate::utils::fts5_quote_literal`], so `timeline` and `search` share a
    /// single relevance path and `timeline` needs no running inference server.
    /// Unlike `search_text`, it deliberately omits the `status = 'active'`
    /// filter: a timeline shows superseded/archived entries alongside active
    /// ones, which is what makes the evolution visible.
    pub fn search_timeline(&self, query: &str, limit: usize) -> Result<Vec<Note>> {
        let limit = limit.min(200);
        // Two steps, matching the documented behaviour: first take the `limit`
        // most relevant matches (inner FTS/BM25 ranking), then re-sort that set
        // ascending by valid_at so the timeline reads oldest → newest. Ranking
        // before the LIMIT means a store with more than `limit` matches keeps
        // the most relevant entries, not merely the oldest ones.
        let sql = format!(
            "SELECT uuid, kind, title, body, tags, linked_files,
                    created_at, status, superseded_by, source_ref,
                    valid_at, invalid_at, entity_id
             FROM (
                 SELECT n.uuid, n.kind, n.title, n.body, n.tags, n.linked_files,
                        n.created_at, n.status, n.superseded_by, n.source_ref,
                        n.valid_at, n.invalid_at, n.entity_id
                 FROM   memory_fts
                 JOIN   notes n ON memory_fts.rowid = n.id
                 WHERE  memory_fts MATCH ?1
                 ORDER  BY bm25(memory_fts), n.uuid
                 LIMIT  {limit}
             )
             ORDER BY COALESCE(valid_at, created_at) ASC, uuid"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let fts_query = crate::utils::fts5_quote_literal(query);
        let notes = stmt
            .query_map(rusqlite::params![fts_query], row_to_note)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(notes)
    }
}

#[cfg(test)]
mod tests {
    use super::{MemoryStore, NoteId};
    use std::sync::OnceLock;

    fn register_sqlite_vec() {
        static INIT: OnceLock<()> = OnceLock::new();
        INIT.get_or_init(|| {
            #[allow(clippy::missing_transmute_annotations)]
            unsafe {
                rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                    sqlite_vec::sqlite3_vec_init as *const (),
                )));
            }
        });
    }

    fn open_store() -> MemoryStore {
        register_sqlite_vec();
        MemoryStore::open(std::path::Path::new(":memory:"))
            .expect("failed to open in-memory MemoryStore")
    }

    fn unit_vec(first: f32) -> Vec<f32> {
        let mut v = vec![0.0_f32; crate::embeddings::EMBEDDING_DIM];
        v[0] = first;
        v[1] = (1.0 - first * first).max(0.0).sqrt();
        v
    }

    // ADR-083 retired the lexical door inside `search_hybrid`: a note reachable
    // only by an FTS match, with no embedding row at all, must never surface
    // here. This proves the vector-only mechanism rather than merely asserting
    // it in prose — pre-fix this note would have entered via `text_results`.
    #[test]
    fn search_hybrid_never_surfaces_a_text_only_match() {
        let store = open_store();
        store
            .add_note(
                "note",
                "Text only hit",
                "rrftieterm appears here and nowhere else",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();
        let query_blob = crate::embeddings::vec_to_blob(&unit_vec(1.0));

        assert!(
            !store
                .search_text("rrftieterm", 20, None)
                .expect("text ok")
                .is_empty(),
            "the note must be genuinely reachable by text for this assertion to mean anything"
        );

        for gate in [true, false] {
            let hits = store
                .search_hybrid(&query_blob, "rrftieterm", 10, None, gate)
                .expect("hybrid ok");
            assert!(
                hits.is_empty(),
                "a text-only match must not surface via search_hybrid (gate={gate}), got {hits:?}"
            );
        }
    }

    // `query` = `unit_vec(1.0)` = (1, 0, 0, ...), so `dot(query, unit_vec(w)) = w`
    // and `distance = sqrt(2 - 2w)`. `w = 0.9` lands well inside
    // `MEMORY_MAX_QA_DISTANCE`; `w = 0.0` gives `distance = sqrt(2) ≈ 1.4142`,
    // well beyond it.
    fn embedded_note(store: &MemoryStore, title: &str, w: f32) -> NoteId {
        let (id, _) = store
            .add_note("note", title, "body", &[], &[], None, None)
            .unwrap();
        store
            .insert_embedding(&id, &crate::embeddings::vec_to_blob(&unit_vec(w)))
            .unwrap();
        id
    }

    #[test]
    fn search_hybrid_gated_drops_a_candidate_beyond_the_distance_floor() {
        let store = open_store();
        let near = embedded_note(&store, "Near", 0.9);
        embedded_note(&store, "Far", 0.0);
        let query_blob = crate::embeddings::vec_to_blob(&unit_vec(1.0));

        let hits = store
            .search_hybrid(&query_blob, "q", 10, None, true)
            .expect("hybrid ok");
        let ids: Vec<NoteId> = hits.into_iter().map(|n| n.id).collect();
        assert_eq!(
            ids,
            vec![near],
            "only the within-floor candidate survives gating"
        );
    }

    // `--only-memory` passes `gate = false`: no code corpus competes for slots,
    // so every vector candidate is admitted (ADR-083 decision 5).
    #[test]
    fn search_hybrid_ungated_admits_a_candidate_beyond_the_distance_floor() {
        let store = open_store();
        let near = embedded_note(&store, "Near", 0.9);
        let far = embedded_note(&store, "Far", 0.0);
        let query_blob = crate::embeddings::vec_to_blob(&unit_vec(1.0));

        let hits = store
            .search_hybrid(&query_blob, "q", 10, None, false)
            .expect("hybrid ok");
        let ids: Vec<NoteId> = hits.into_iter().map(|n| n.id).collect();
        assert_eq!(
            ids,
            vec![near, far],
            "ungated keeps every candidate, ranked by distance"
        );
    }

    #[test]
    fn search_hybrid_with_every_candidate_gated_out_returns_empty() {
        let store = open_store();
        embedded_note(&store, "Far", 0.0);
        let query_blob = crate::embeddings::vec_to_blob(&unit_vec(1.0));

        let hits = store
            .search_hybrid(&query_blob, "q", 10, None, true)
            .expect("hybrid ok");
        assert!(
            hits.is_empty(),
            "an entirely irrelevant store contributes nothing, got {hits:?}"
        );
    }

    // Determinism: `self.search()` already tie-breaks by `NoteId` at the SQL
    // level (`ORDER BY k.distance, n.uuid`), and with one candidate source
    // every admitted note gets a distinct rank, so `search_hybrid`'s own
    // `HashMap`-keyed re-sort cannot introduce a reshuffle across calls.
    #[test]
    fn search_hybrid_ordering_is_identical_across_calls() {
        let store = open_store();
        for (i, w) in [0.99_f32, 0.95, 0.90].into_iter().enumerate() {
            embedded_note(&store, &format!("Note {i}"), w);
        }
        let query_blob = crate::embeddings::vec_to_blob(&unit_vec(1.0));

        let first = store
            .search_hybrid(&query_blob, "q", 10, None, true)
            .expect("hybrid ok")
            .into_iter()
            .map(|n| n.id)
            .collect::<Vec<_>>();
        for call in 0..20 {
            let hits = store
                .search_hybrid(&query_blob, "q", 10, None, true)
                .expect("hybrid ok")
                .into_iter()
                .map(|n| n.id)
                .collect::<Vec<_>>();
            assert_eq!(hits, first, "call {call} reordered the result");
        }
    }

    /// A search term containing FTS5-special punctuation must never surface a
    /// raw FTS5 parse error from `search_text` — it's always treated as a
    /// literal term (quoted internally), so the call returns `Ok` (results or
    /// empty) regardless of punctuation.
    #[test]
    fn search_text_with_punctuation_never_errors() {
        let store = open_store();
        store
            .add_note(
                "note",
                "Config parsing",
                "handles foo:bar style keys",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();

        let queries = [
            "foo:bar",
            "\"unterminated quote",
            "a OR NOT b",
            "weird (parens",
            "trailing*",
            "-leading-dash",
            "",
            "a NEAR b",
            "a NEAR/3 b",
            "content:secret",
            "\"\"\"\"\"",
            "^prefix",
            "((()))",
        ];
        for q in queries {
            let result = store.search_text(q, 10, None);
            assert!(
                result.is_ok(),
                "query {q:?} must not surface a raw FTS5 parse error, got: {:?}",
                result.err()
            );
        }
    }

    /// A query term containing an embedded NUL byte must not surface a raw
    /// FTS5 "unterminated string" parse error via this memory-search path
    /// too (same fix as the code-search path in `storage::search::tests`).
    #[test]
    fn search_text_embedded_nul_byte_still_leaks_raw_parse_error() {
        let store = open_store();
        store
            .add_note(
                "note",
                "Config parsing",
                "handles foo:bar style keys",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();

        let result = store.search_text("\0embedded nul", 10, None);
        assert!(
            result.is_ok(),
            "query with embedded NUL must not surface a raw FTS5 parse error, got: {:?}",
            result.err()
        );
    }

    /// Quoting the term as an FTS5 literal must not break normal matching.
    #[test]
    fn search_text_plain_term_still_matches() {
        let store = open_store();
        store
            .add_note(
                "note",
                "Config parsing",
                "handles foo:bar style keys",
                &[],
                &[],
                None,
                None,
            )
            .unwrap();

        let results = store.search_text("parsing", 10, None).expect("search ok");
        assert!(
            !results.is_empty(),
            "expected the seeded note to match a plain-word query"
        );
    }

    // Seed a store with two clearly distinct topics so a topic query has a
    // strict subset to return and unrelated entries to exclude. `valid_at`
    // values are set out of insertion order so the ascending-by-valid_at
    // ordering can be verified independently of row order.
    fn seed_two_topic_store() -> MemoryStore {
        let store = open_store();
        // Topic A: retry backoff. valid_at chosen out of order (300, 100, 200).
        store
            .add_note(
                "decision",
                "Retry backoff",
                "use exponential backoff on retry",
                &[],
                &[],
                None,
                Some(300),
            )
            .unwrap();
        store
            .add_note(
                "context",
                "Backoff ceiling",
                "cap the retry backoff delay",
                &[],
                &[],
                None,
                Some(100),
            )
            .unwrap();
        store
            .add_note(
                "note",
                "Jitter",
                "add jitter to the backoff schedule",
                &[],
                &[],
                None,
                Some(200),
            )
            .unwrap();
        // Topic B: pagination. Unrelated — must never surface for a backoff query.
        store
            .add_note(
                "decision",
                "Cursor pagination",
                "paginate results with an opaque cursor",
                &[],
                &[],
                None,
                Some(150),
            )
            .unwrap();
        store
            .add_note(
                "context",
                "Page size",
                "clamp the pagination page size to 100",
                &[],
                &[],
                None,
                Some(250),
            )
            .unwrap();
        store
    }

    // A topic query returns ONLY the entries related to that topic (a strict
    // subset when the store also holds unrelated entries), ordered ascending by
    // valid_at. Pre-fix `search_timeline` ignored the topic entirely and
    // returned every entry, so this asserts the relevance filter now works.
    #[test]
    fn search_timeline_filters_to_relevant_subset_sorted_by_valid_at() {
        let store = seed_two_topic_store();

        let results = store.search_timeline("backoff", 20).expect("timeline ok");

        // Strict subset: the 3 backoff entries, none of the 2 pagination ones.
        assert_eq!(
            results.len(),
            3,
            "expected only the 3 backoff entries, got {}: {:?}",
            results.len(),
            results.iter().map(|n| &n.title).collect::<Vec<_>>()
        );
        assert!(
            results
                .iter()
                .all(|n| n.title.to_lowercase().contains("backoff")
                    || n.body.to_lowercase().contains("backoff")),
            "every returned entry must be topic-relevant, got {:?}",
            results.iter().map(|n| &n.title).collect::<Vec<_>>()
        );

        // Ordering: ascending by valid_at (100, 200, 300).
        let order: Vec<Option<i64>> = results.iter().map(|n| n.valid_at).collect();
        assert_eq!(
            order,
            vec![Some(100), Some(200), Some(300)],
            "timeline must be sorted ascending by valid_at"
        );
    }

    // A nonsense topic that matches nothing returns few or zero entries — NOT
    // the whole store. Pre-fix this returned every entry regardless of topic.
    #[test]
    fn search_timeline_nonsense_topic_returns_nothing() {
        let store = seed_two_topic_store();

        let results = store
            .search_timeline("quantumferretunicycles", 20)
            .expect("timeline ok");

        assert!(
            results.is_empty(),
            "a nonsense topic must not return the whole store, got {} entries: {:?}",
            results.len(),
            results.iter().map(|n| &n.title).collect::<Vec<_>>()
        );
    }

    // A timeline traces how understanding evolved, so it must keep archived /
    // superseded entries (unlike `search_text`, which is active-only). Guards
    // the deliberate omission of the `status = 'active'` filter.
    #[test]
    fn search_timeline_includes_archived_entries() {
        let store = open_store();
        let (id, _) = store
            .add_note(
                "decision",
                "Old backoff policy",
                "retry backoff was linear",
                &[],
                &[],
                None,
                Some(100),
            )
            .unwrap();
        store
            .add_note(
                "decision",
                "New backoff policy",
                "retry backoff is now exponential",
                &[],
                &[],
                None,
                Some(200),
            )
            .unwrap();
        assert!(
            store.archive(&id).expect("archive ok"),
            "note should archive"
        );

        let timeline = store.search_timeline("backoff", 20).expect("timeline ok");
        assert_eq!(
            timeline.len(),
            2,
            "timeline must include the archived entry alongside the active one"
        );

        // `search_text` is active-only, so it drops the archived entry — this is
        // exactly the difference that justifies timeline's own query.
        let active_only = store.search_text("backoff", 20, None).expect("search ok");
        assert_eq!(
            active_only.len(),
            1,
            "search_text must exclude the archived entry (active-only)"
        );
    }
}
