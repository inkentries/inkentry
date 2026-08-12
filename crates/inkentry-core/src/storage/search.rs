use anyhow::Result;

use super::Database;

impl Database {
    /// K-nearest-neighbour search using sqlite-vec.
    ///
    /// Takes the raw float query vector; it is int8-quantised here to match the
    /// `embeddings` `int8[896]` column (see `embeddings::vec_to_int8_blob`).
    /// Returns results ordered by ascending distance (closest first).
    pub fn search_similar(
        &self,
        query: &[f32],
        limit: usize,
    ) -> Result<Vec<crate::search::SearchResult>> {
        let query_blob = crate::embeddings::vec_to_int8_blob(query);
        let limit = limit.min(1_000);
        let sql = format!(
            "WITH knn AS (
                 SELECT chunk_id, distance
                 FROM   embeddings
                 WHERE  embedding MATCH vec_int8(?1)
                   AND  k = {limit}
             )
             SELECT  k.chunk_id,
                     CAST(k.distance AS REAL),
                     c.node_type,
                     c.name,
                     CAST(c.start_line AS INTEGER),
                     CAST(c.end_line   AS INTEGER),
                     c.content,
                     f.path,
                     f.language,
                     c.token_count,
                     c.graph_rank
             FROM knn k
             JOIN chunks c ON c.id = k.chunk_id
             JOIN files  f ON f.id = c.file_id
             ORDER BY k.distance, f.path, c.start_line, c.end_line, k.chunk_id"
        );

        const GRAPH_RANK_ALPHA: f32 = 0.15;

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params![&query_blob], |row| {
            // int8 L2 distance is ~127× the f32 distance; rescale to keep the
            // graph-rank blend (and downstream `1 - distance`) on the old scale.
            let raw_distance: f32 = row.get::<_, f32>(1)? / crate::embeddings::INT8_SCALE;
            let graph_rank: f32 = row.get(10)?;
            let blended = raw_distance * (1.0 - GRAPH_RANK_ALPHA) - graph_rank * GRAPH_RANK_ALPHA;
            Ok(crate::search::SearchResult {
                chunk_id: row.get(0)?,
                distance: blended,
                node_type: row.get(2)?,
                name: row.get(3)?,
                start_line: row.get::<_, i64>(4)? as usize,
                end_line: row.get::<_, i64>(5)? as usize,
                content: row.get(6)?,
                file_path: row.get(7)?,
                language: row.get(8)?,
                from_graph: false,
                governing_specs: vec![],
                token_count: row.get::<_, i64>(9)? as usize,
                project_name: None,
                project_path: None,
                summary: None,
            })
        })?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// FTS5 full-text search. Returns results ranked by BM25 (best match first).
    ///
    /// The query's words are scored as **independent terms** (BM25 bag-of-words
    /// via [`crate::utils::fts5_match_query`]): a multi-word query ranks chunks
    /// that contain the terms regardless of their order or adjacency, and a
    /// chunk containing more of the terms ranks above one containing fewer. It
    /// is deliberately not matched as a contiguous phrase. Tokenisation follows
    /// the `chunks_fts` tokenizer (default `unicode61`: case-folded, no
    /// stemming).
    ///
    /// BM25 in FTS5 returns negative values (more negative = better match).
    /// We negate the score so that higher `distance` values indicate better matches,
    /// consistent with the convention used in `SearchResult`.
    pub fn search_text(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<crate::search::SearchResult>> {
        let limit = limit.min(1_000);
        let mut stmt = self.conn.prepare(
            "SELECT c.id, c.node_type, c.name,
                    CAST(c.start_line AS INTEGER), CAST(c.end_line AS INTEGER),
                    c.content, f.path, f.language,
                    bm25(chunks_fts) AS score
             FROM chunks_fts
             JOIN chunks c ON chunks_fts.rowid = c.id
             JOIN files  f ON c.file_id = f.id
             WHERE chunks_fts MATCH ?1
             ORDER BY score, f.path, c.start_line, c.end_line, c.id
             LIMIT ?2",
        )?;
        let fts_query = crate::utils::fts5_match_query(query);
        let rows = stmt.query_map(rusqlite::params![fts_query, limit as i64], |row| {
            let bm25_score: f64 = row.get(8)?;
            Ok(crate::search::SearchResult {
                chunk_id: row.get(0)?,
                node_type: row.get(1)?,
                name: row.get(2)?,
                start_line: row.get::<_, i64>(3)? as usize,
                end_line: row.get::<_, i64>(4)? as usize,
                content: row.get(5)?,
                file_path: row.get(6)?,
                language: row.get(7)?,
                // Negate so that more-relevant results have a lower distance,
                // matching the ascending-distance convention of vector search.
                distance: (-bm25_score) as f32,
                from_graph: false,
                governing_specs: vec![],
                token_count: 0,
                project_name: None,
                project_path: None,
                summary: None,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Hybrid search: fuses FTS5 BM25 ranking with vector KNN via Reciprocal Rank Fusion.
    ///
    /// RRF score: `Σ 1 / (k + rank_i)` where `k` is the shared [`crate::search::RRF_K`].
    pub fn search_hybrid(
        &self,
        query: &str,
        embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<crate::search::SearchResult>> {
        use std::collections::HashMap;

        let candidates = (limit * 3).max(20);
        let vec_results = self.search_similar(embedding, candidates)?;
        let text_results = self.search_text(query, candidates).unwrap_or_default();

        const K: f64 = crate::search::RRF_K;

        let mut scores: HashMap<i64, f64> = HashMap::new();
        let mut by_id: HashMap<i64, crate::search::SearchResult> = HashMap::new();

        for (rank, result) in vec_results.into_iter().enumerate() {
            let rrf = 1.0 / (K + (rank + 1) as f64);
            *scores.entry(result.chunk_id).or_insert(0.0) += rrf;
            by_id.entry(result.chunk_id).or_insert(result);
        }

        for (rank, result) in text_results.into_iter().enumerate() {
            let rrf = 1.0 / (K + (rank + 1) as f64);
            *scores.entry(result.chunk_id).or_insert(0.0) += rrf;
            by_id.entry(result.chunk_id).or_insert(result);
        }

        // `scores` is a HashMap, so its iteration order is reseeded per instance
        // and differs between two calls in ONE process. RRF ties are the norm,
        // not the exception — with disjoint lists, vector rank i and text rank i
        // always score identically — so a sort keyed on score alone would leave
        // most of the result order to that reseeding. The tie-break makes the
        // order total, and keys it on the corpus position rather than the rowid
        // so two machines indexing the same tree agree.
        let mut ranked: Vec<(i64, f64)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| {
            b.1.total_cmp(&a.1)
                .then_with(|| tie_break_key(&by_id, a.0).cmp(&tie_break_key(&by_id, b.0)))
        });
        ranked.truncate(limit);

        let results = ranked
            .into_iter()
            .filter_map(|(id, rrf_score)| {
                by_id.remove(&id).map(|mut r| {
                    r.distance = (1.0 / rrf_score) as f32;
                    r
                })
            })
            .collect();

        Ok(results)
    }
}

/// Source position of a chunk, as the final sort key for equal scores.
///
/// Deliberately not the bare `chunk_id`: that rowid is assigned by indexing
/// order, so two machines indexing the same tree can disagree on it. The full
/// span is a property of the source. `end_line` earns its place because the
/// walker emits a matched node and then recurses into it, so a nested node
/// beginning on its parent's line yields two chunks sharing `(path,
/// start_line)` that only the end distinguishes. The id settles the residual
/// case of two chunks over one identical span.
fn tie_break_key(
    by_id: &std::collections::HashMap<i64, crate::search::SearchResult>,
    chunk_id: i64,
) -> (&str, usize, usize, i64) {
    by_id.get(&chunk_id).map_or(("", 0, 0, chunk_id), |r| {
        (r.file_path.as_str(), r.start_line, r.end_line, chunk_id)
    })
}

#[cfg(test)]
mod tests {
    use super::super::Database;
    use std::sync::OnceLock;

    /// Register the sqlite-vec extension exactly once per test process.
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

    fn open_db() -> Database {
        register_sqlite_vec();
        Database::open(std::path::Path::new(":memory:")).expect("failed to open in-memory Database")
    }

    fn seed_chunk(db: &Database, content: &str) {
        let file_id = db
            .upsert_file("src/lib.rs", Some("rust"), "deadbeef", 0)
            .expect("upsert file");
        db.insert_chunk(file_id, "function", Some("f"), 1, 5, content, None, 4)
            .expect("insert chunk");
    }

    fn seed_chunk_in(db: &Database, path: &str, content: &str) -> i64 {
        let file_id = db
            .upsert_file(path, Some("rust"), path, 0)
            .expect("upsert file");
        db.insert_chunk(file_id, "function", Some("f"), 1, 5, content, None, 4)
            .expect("insert chunk")
    }

    fn unit_vec(first: f32) -> Vec<f32> {
        let mut v = vec![0.0_f32; crate::embeddings::EMBEDDING_DIM];
        v[0] = first;
        v[1] = (1.0 - first * first).max(0.0).sqrt();
        v
    }

    fn seed_span(db: &Database, path: &str, start: usize, end: usize, content: &str) -> i64 {
        let file_id = db
            .upsert_file(path, Some("rust"), path, 0)
            .expect("upsert file");
        db.insert_chunk(file_id, "function", Some("f"), start, end, content, None, 4)
            .expect("insert chunk")
    }

    // `ts_walker` emits a matched node and then recurses into it, so a nested
    // node beginning on its parent's line yields two chunks that share
    // (path, start_line) and differ only in end_line — `impl Foo { fn bar() ->
    // u32 { 1 }` followed by a closing brace parses to Impl 1..2 and Function
    // 1..1. The span must therefore outrank the rowid, which is assigned by
    // indexing order and disagrees between machines. Both fixtures insert the
    // wider span FIRST, so rowid order is the opposite of span order and a key
    // that stopped at start_line would return them the other way round.
    #[test]
    fn search_text_breaks_ties_on_end_line_before_rowid() {
        let db = open_db();
        let outer = seed_span(&db, "src/a.rs", 1, 2, "fn nestedspanterm() {}");
        let inner = seed_span(&db, "src/a.rs", 1, 1, "fn nestedspanterm() {}");

        let hits = db.search_text("nestedspanterm", 10).expect("search ok");
        let ids: Vec<i64> = hits.iter().map(|h| h.chunk_id).collect();
        assert_eq!(
            ids,
            vec![inner, outer],
            "equal BM25 over one start line must order by end_line, not by rowid"
        );
    }

    #[test]
    fn search_hybrid_breaks_rrf_ties_on_end_line_before_rowid() {
        let db = open_db();
        let text_only = seed_span(&db, "src/h.rs", 1, 20, "fn hybridspanterm() {}");
        let vec_only = seed_span(&db, "src/h.rs", 1, 5, "fn other() { alpha beta gamma; }");
        db.insert_embedding(vec_only, &unit_vec(1.0))
            .expect("embed");

        // Each is rank 1 in exactly one list, so the RRF scores are identical
        // and the tie-break alone orders them.
        let hits = db
            .search_hybrid("hybridspanterm", &unit_vec(1.0), 10)
            .expect("hybrid ok");
        let ids: Vec<i64> = hits.iter().map(|h| h.chunk_id).collect();
        assert_eq!(
            ids,
            vec![vec_only, text_only],
            "an RRF tie over one start line must order by end_line, not by rowid"
        );
    }

    // Two chunks with byte-identical content score identical BM25, so only the
    // tie-break decides their order. Pinned to source position, not to whatever
    // SQLite's sorter happens to emit.
    #[test]
    fn search_text_breaks_bm25_ties_by_source_position() {
        let db = open_db();
        for path in ["src/c.rs", "src/a.rs", "src/b.rs"] {
            seed_chunk_in(&db, path, "fn handler() { tiebreakterm(); }");
        }

        let hits = db.search_text("tiebreakterm", 10).expect("search ok");
        let paths: Vec<&str> = hits.iter().map(|h| h.file_path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["src/a.rs", "src/b.rs", "src/c.rs"],
            "equal BM25 scores must order by path, not by insertion or scan order"
        );
    }

    // Issue #47. A chunk reachable only by vector and a chunk reachable only by
    // text land at the same rank in their respective lists, so RRF scores them
    // identically — the pervasive tie in hybrid search. Before the tie-break,
    // the winner was decided by `HashMap` iteration order, which is reseeded per
    // instance and so differed between two calls in ONE process.
    #[test]
    fn search_hybrid_breaks_rrf_ties_by_source_position() {
        let db = open_db();

        // Text-only: matches the FTS query, never embedded.
        for path in ["src/t1.rs", "src/t2.rs", "src/t3.rs"] {
            seed_chunk_in(&db, path, "fn f() { rrftieterm(); }");
        }
        // Vector-only: embedded at increasing distance, no query term in content.
        for (path, w) in [("src/v1.rs", 1.0), ("src/v2.rs", 0.98), ("src/v3.rs", 0.95)] {
            let id = seed_chunk_in(&db, path, "fn f() { alpha beta gamma; }");
            db.insert_embedding(id, &unit_vec(w)).expect("embed");
        }

        let query_vec = unit_vec(1.0);
        let expected = vec![
            "src/t1.rs",
            "src/v1.rs",
            "src/t2.rs",
            "src/v2.rs",
            "src/t3.rs",
            "src/v3.rs",
        ];

        // Repeated in-process: each call builds a fresh HashMap with a fresh
        // hash seed, which is exactly what used to reshuffle the tied pairs.
        for i in 0..40 {
            let hits = db
                .search_hybrid("rrftieterm", &query_vec, 10)
                .expect("hybrid ok");
            let paths: Vec<&str> = hits.iter().map(|h| h.file_path.as_str()).collect();
            assert_eq!(
                paths, expected,
                "hybrid order must be identical on every call (call {i})"
            );
        }
    }

    fn queue_ids(db: &Database) -> Vec<i64> {
        db.chunks_missing_embeddings()
            .expect("queue")
            .into_iter()
            .map(|(id, ..)| id)
            .collect()
    }

    fn embedded_ids(db: &Database) -> Vec<i64> {
        let mut stmt = db
            .conn
            .prepare("SELECT chunk_id FROM embeddings ORDER BY chunk_id")
            .expect("prepare");
        stmt.query_map([], |r| r.get::<_, i64>(0))
            .expect("query")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect")
    }

    // Issue #47's remaining acceptance criterion: the property must hold on a
    // PARTIALLY EMBEDDED index — the normal state in the first hours after
    // indexing, and where the instability is largest.
    //
    // The fixture reproduces that state the way a real drain leaves it rather
    // than by merely seeding few rows. `embed_phase` walks the
    // `chunks_missing_embeddings()` queue with a cursor and commits one batch
    // per transaction, so an interrupted drain leaves vectors on an exact
    // PREFIX of that queue order — never-embedded band first, then graph_rank
    // DESC, mtime DESC, id — with the tail bare. One drained chunk is then
    // flagged `embed_pending = 1`, the re-embed band that co-exists with the
    // warmup tail. Both properties are asserted, so the fixture cannot quietly
    // decay into "a small index".
    //
    // This is the disjoint-list case at its sharpest: the vector list holds
    // only the drained prefix, chosen by centrality and recency rather than by
    // relevance to this query, while the text list holds every match. Chunks
    // reachable through exactly one list collide on identical RRF scores.
    #[test]
    fn search_hybrid_is_byte_identical_on_a_partially_embedded_index() {
        let db = open_db();

        // Alternating files: even ones carry the query term, odd ones do not,
        // so the drained prefix straddles both and the two lists genuinely
        // diverge. mtime descends with the file index to give the queue a
        // meaningful order.
        for f in 0..6_i64 {
            let file_id = db
                .upsert_file(
                    &format!("src/f{f}.rs"),
                    Some("rust"),
                    &format!("hash{f}"),
                    600 - f * 100,
                )
                .expect("upsert file");
            let content = if f % 2 == 0 {
                "fn handler() { warmupterm(); }"
            } else {
                "fn helper() { alpha beta gamma; }"
            };
            for c in 0..2_usize {
                db.insert_chunk(
                    file_id,
                    "function",
                    Some(&format!("fn_{f}_{c}")),
                    1 + c * 10,
                    5 + c * 10,
                    content,
                    None,
                    4,
                )
                .expect("insert chunk");
            }
        }

        let queue = queue_ids(&db);
        assert_eq!(queue.len(), 12, "every chunk starts unembedded");

        // Drain a prefix through the same API the real embed phase commits
        // with, which also clears `embed_pending` in the same transaction.
        const DRAINED: usize = 5;
        let batch: Vec<(i64, Vec<f32>)> = queue[..DRAINED]
            .iter()
            .enumerate()
            .map(|(i, id)| (*id, unit_vec(1.0 - i as f32 * 0.02)))
            .collect();
        db.insert_embeddings(&batch).expect("commit batch");

        // A pending re-embed alongside the warmup tail.
        db.conn
            .execute(
                "UPDATE chunks SET embed_pending = 1 WHERE id = ?1",
                rusqlite::params![queue[0]],
            )
            .expect("flag re-embed");

        let mut expected_embedded = queue[..DRAINED].to_vec();
        expected_embedded.sort_unstable();
        assert_eq!(
            embedded_ids(&db),
            expected_embedded,
            "the drained set must be an exact prefix of the real queue order"
        );
        let still_missing = queue_ids(&db);
        assert_eq!(
            still_missing.len(),
            12 - DRAINED + 1,
            "the unembedded tail plus the one pending re-embed are still queued"
        );
        assert_eq!(
            still_missing[still_missing.len() - 1],
            queue[0],
            "a pending re-embed sorts behind the never-embedded band"
        );

        let query_vec = unit_vec(1.0);
        // `search_hybrid`'s own candidate budget for limit 10.
        let candidates = 30;
        let vec_list = db.search_similar(&query_vec, candidates).expect("knn ok");
        let text_list = db.search_text("warmupterm", candidates).expect("fts ok");

        // Guard against a decorative test: recompute the RRF scores the way
        // `search_hybrid` does and require an exact collision. Without one,
        // every score is distinct and the tie-break is never consulted, so the
        // test would pass with or without the fix.
        let mut rrf: std::collections::HashMap<i64, f64> = std::collections::HashMap::new();
        for list in [&vec_list, &text_list] {
            for (rank, r) in list.iter().enumerate() {
                *rrf.entry(r.chunk_id).or_insert(0.0) +=
                    1.0 / (crate::search::RRF_K + (rank + 1) as f64);
            }
        }
        let mut score_bits: Vec<u64> = rrf.values().map(|v| v.to_bits()).collect();
        let distinct_before = score_bits.len();
        score_bits.sort_unstable();
        score_bits.dedup();
        assert!(
            score_bits.len() < distinct_before,
            "fixture must produce at least one exact RRF tie, else the tie-break is untested"
        );

        // 64 calls. Each builds a fresh HashMap, and `RandomState` reseeds per
        // instance, so an unbroken tie is an independent coin flip per call:
        // surviving 64 of them has probability 2^-64. The pre-fix code in
        // practice diverges on the first or second call; 64 is headroom, not a
        // tight bound.
        let baseline = serde_json::to_string(
            &db.search_hybrid("warmupterm", &query_vec, 10)
                .expect("hybrid ok"),
        )
        .expect("serialise");
        for call in 1..64 {
            let got = serde_json::to_string(
                &db.search_hybrid("warmupterm", &query_vec, 10)
                    .expect("hybrid ok"),
            )
            .expect("serialise");
            assert_eq!(
                got, baseline,
                "partially-embedded hybrid results must be byte-identical (call {call})"
            );
        }

        // The answer must actually span both bands, or the fixture is only
        // exercising a fully-drained index by another name.
        let hits = db
            .search_hybrid("warmupterm", &query_vec, 10)
            .expect("hybrid ok");
        let drained: std::collections::HashSet<i64> = queue[..DRAINED].iter().copied().collect();
        assert!(
            hits.iter().any(|h| drained.contains(&h.chunk_id)),
            "results must include an embedded chunk"
        );
        assert!(
            hits.iter().any(|h| !drained.contains(&h.chunk_id)),
            "results must include a not-yet-embedded chunk"
        );
    }

    /// A search term containing FTS5-special punctuation (unbalanced `"`,
    /// a bare `:`, and boolean-looking keywords) must never surface a raw
    /// FTS5 parse error — it should be treated as a literal string and either
    /// return matches or an empty result, but always `Ok`.
    #[test]
    fn search_text_with_punctuation_never_errors() {
        let db = open_db();
        seed_chunk(&db, "fn parse_config() { /* handles foo:bar */ }");

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
            "content:secret", // attempted FTS5 column-filter injection
            "\"\"\"\"\"",     // consecutive quotes
            "^prefix",
            "((()))",
        ];
        for q in queries {
            let result = db.search_text(q, 10);
            assert!(
                result.is_ok(),
                "query {q:?} must not surface a raw FTS5 parse error, got: {:?}",
                result.err()
            );
        }
    }

    /// A query term containing an embedded NUL byte must not surface a raw
    /// FTS5 "unterminated string" parse error. `fts5_quote_literal` strips
    /// embedded `\0` before quoting, since FTS5's own query-string parser
    /// treats `\0` as an early string terminator (distinct from SQLite's
    /// NUL-safe text binding), which would otherwise hide the closing `"` we
    /// append.
    #[test]
    fn search_text_embedded_nul_byte_still_leaks_raw_parse_error() {
        let db = open_db();
        seed_chunk(&db, "fn parse_config() { /* handles foo:bar */ }");

        let result = db.search_text("\0embedded nul", 10);
        assert!(
            result.is_ok(),
            "query with embedded NUL must not surface a raw FTS5 parse error, got: {:?}",
            result.err()
        );
    }

    /// A literal-quoted term still matches real content containing that
    /// literal substring, so quoting doesn't silently break search relevance.
    #[test]
    fn search_text_quoted_colon_term_still_matches() {
        let db = open_db();
        seed_chunk(&db, "fn parse_config() { /* handles foo:bar */ }");

        let results = db.search_text("parse_config", 10).expect("search ok");
        assert!(
            !results.is_empty(),
            "expected the seeded chunk to match a plain-word query"
        );
    }

    /// A term shaped like an FTS5 column filter (`content:...`) must be
    /// treated as a literal string to search for, not interpreted as
    /// targeting the `content` column. Quoted, it should behave like any
    /// other safe literal search (no match on an unrelated nonsense term,
    /// no error).
    #[test]
    fn search_text_column_filter_syntax_is_literal_not_a_filter() {
        let db = open_db();
        seed_chunk(&db, "fn parse_config() { /* handles foo:bar */ }");

        let result = db.search_text("content:nonexistent_term_xyz", 10);
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    // The documented `--mode text` contract is BM25 over independent terms: a
    // multi-word query ranks chunks that contain the terms regardless of their
    // order or adjacency. Before the fix the whole query was matched as a single
    // quoted FTS5 phrase, so word order alone decided whether there was a hit.
    #[test]
    fn search_text_scores_terms_independent_of_order() {
        let db = open_db();
        seed_chunk(
            &db,
            "We chose a token bucket over a leaky bucket because bursts are expected.",
        );

        // The natural order and every reordering must hit the one chunk that
        // contains all the terms — order and adjacency must not decide the hit.
        for q in [
            "leaky bucket",
            "bucket leaky",
            "bursts are expected",
            "expected bursts",
            "token bucket bursts",
        ] {
            let hits = db.search_text(q, 10).expect("search ok");
            assert!(
                !hits.is_empty(),
                "query {q:?} must match the chunk containing all its terms, \
                 regardless of word order"
            );
        }
    }

    // Partial-overlap ranking: a chunk containing more of the query terms ranks
    // above one containing fewer, and the fewer-term chunk still appears (OR /
    // bag-of-words semantics, ranked by BM25) rather than being dropped — which
    // is what phrase matching did.
    #[test]
    fn search_text_ranks_more_term_overlap_higher() {
        let db = open_db();

        let all_terms = db
            .upsert_file("src/all.rs", Some("rust"), "aaaa1111", 0)
            .expect("upsert file");
        db.insert_chunk(
            all_terms,
            "function",
            Some("all"),
            1,
            5,
            "token bucket bursts all three present here",
            None,
            8,
        )
        .expect("insert chunk");

        let one_term = db
            .upsert_file("src/one.rs", Some("rust"), "bbbb2222", 0)
            .expect("upsert file");
        db.insert_chunk(
            one_term,
            "function",
            Some("one"),
            1,
            5,
            "a single bucket and lots of unrelated padding padding padding",
            None,
            8,
        )
        .expect("insert chunk");

        let hits = db
            .search_text("token bucket bursts", 10)
            .expect("search ok");

        // Both chunks share the term "bucket", so both are candidates under
        // bag-of-words scoring.
        assert!(
            hits.len() >= 2,
            "the chunk sharing only one term must still be a candidate: {:?}",
            hits.iter().map(|h| &h.content).collect::<Vec<_>>()
        );
        assert!(
            hits[0].content.contains("all three present"),
            "the chunk containing all three query terms must rank first, got: {:?}",
            hits.iter().map(|h| &h.content).collect::<Vec<_>>()
        );
        assert!(
            hits.iter().any(|h| h.content.contains("single bucket")),
            "the single-term chunk must still appear (ranked lower), not be excluded"
        );
    }

    // Stemming decision, locked in: `--mode text` uses the table's default FTS5
    // tokenizer (`unicode61`), which case-folds but does NOT stem. So `bursts`
    // matches the literal token `bursts`; a query for `burst` does NOT match a
    // chunk that only contains `bursts`; and matching is case-insensitive.
    // If the tokenizer ever gains stemming, this test flags it.
    #[test]
    fn search_text_does_not_stem_and_is_case_insensitive() {
        let db = open_db();
        seed_chunk(&db, "the queue absorbs bursts of traffic");

        assert!(
            !db.search_text("bursts", 10).expect("ok").is_empty(),
            "the exact token must match"
        );
        assert!(
            db.search_text("burst", 10).expect("ok").is_empty(),
            "unstemmed: a query for `burst` must NOT match a chunk containing only `bursts`"
        );
        assert!(
            !db.search_text("BURSTS", 10).expect("ok").is_empty(),
            "matching is case-insensitive (unicode61 case-folding)"
        );
    }
}
