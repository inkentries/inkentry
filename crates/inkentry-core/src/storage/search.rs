use anyhow::Result;

use super::Database;

// BM25 weights for `chunks_fts`'s columns, in declaration order: name, path,
// docstring, summary, content. They mostly lift the first result rather than
// recall at ten.
const FTS_COLUMN_WEIGHTS: (f64, f64, f64, f64, f64) = (6.0, 2.0, 2.0, 2.0, 1.0);

// Candidates each list contributes to hybrid fusion, whatever the caller's
// limit, so the top results do not depend on how many were asked for.
const HYBRID_POOL: usize = 250;

// Weight of the vector score in hybrid fusion; full-text gets the rest. A query
// naming an identifier leans on full-text, where the exact symbol matches.
const DENSE_WEIGHT: f64 = 0.6;
const DENSE_WEIGHT_FOR_IDENTIFIER_QUERY: f64 = 0.4;

// Stand-in vector score for a chunk the embed queue has not reached yet. With
// none it would be capped at the full-text share and rank below every vector
// hit while an index warms up. Text-only chunks (ADR-104) never get a vector
// and keep that cap: lifting them as well lowered MRR.
const AWAITING_VECTOR_DENSE_SCORE: f64 = 0.5;

const GRAPH_RANK_ALPHA: f32 = 0.15;

impl Database {
    /// K-nearest-neighbour search using sqlite-vec. The raw float query vector
    /// is int8-quantised to match the `embeddings` table's `int8[896]` column.
    /// Returns results ordered by ascending vector distance (closest first);
    /// the reported `distance` blends in the chunk's `graph_rank`.
    pub fn search_similar(
        &self,
        query: &[f32],
        limit: usize,
    ) -> Result<Vec<crate::search::SearchResult>> {
        self.knn(query, limit, GRAPH_RANK_ALPHA)
    }

    fn knn(
        &self,
        query: &[f32],
        limit: usize,
        graph_rank_alpha: f32,
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

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params![&query_blob], |row| {
            // int8 L2 distance is ~127× the f32 distance; rescale to keep the
            // graph-rank blend (and downstream `1 - distance`) on the old scale.
            let raw_distance: f32 = row.get::<_, f32>(1)? / crate::embeddings::INT8_SCALE;
            let graph_rank: f32 = row.get(10)?;
            let blended = raw_distance * (1.0 - graph_rank_alpha) - graph_rank * graph_rank_alpha;
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
    /// The query's words (and the parts of any compound identifier) are
    /// scored as **independent terms**, so a chunk containing more of them
    /// ranks above one containing fewer, regardless of order or adjacency.
    /// The index is stemmed (`porter unicode61`) and BM25 weighs a hit in the
    /// symbol name above one in the path, docstring or summary, and those
    /// above one in the body.
    ///
    /// `distance` is the raw BM25 score, which FTS5 makes more negative for a
    /// better match, so lower is better here as it is for vector search.
    pub fn search_text(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<crate::search::SearchResult>> {
        let limit = limit.min(1_000);
        let (name, path, doc, summary, content) = FTS_COLUMN_WEIGHTS;
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT c.id, c.node_type, c.name,
                    CAST(c.start_line AS INTEGER), CAST(c.end_line AS INTEGER),
                    c.content, f.path, f.language,
                    bm25(chunks_fts, {name}, {path}, {doc}, {summary}, {content}) AS score
             FROM chunks_fts
             JOIN chunks c ON chunks_fts.rowid = c.id
             JOIN files  f ON c.file_id = f.id
             WHERE chunks_fts MATCH ?1
             ORDER BY score, f.path, c.start_line, c.end_line, c.id
             LIMIT ?2",
        ))?;
        let fts_query = crate::search::lexical::code_fts_query(query);
        let rows = stmt.query_map(rusqlite::params![fts_query, limit as i64], |row| {
            Ok(crate::search::SearchResult {
                chunk_id: row.get(0)?,
                node_type: row.get(1)?,
                name: row.get(2)?,
                start_line: row.get::<_, i64>(3)? as usize,
                end_line: row.get::<_, i64>(4)? as usize,
                content: row.get(5)?,
                file_path: row.get(6)?,
                language: row.get(7)?,
                distance: row.get::<_, f64>(8)? as f32,
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

    /// Hybrid search: fuses FTS5 BM25 with vector KNN by a weighted sum of
    /// normalised scores.
    ///
    /// Each list contributes its top [`HYBRID_POOL`] candidates. BM25 is divided
    /// by the best BM25 in its list; cosine similarity is min-max scaled over
    /// its list. A chunk is scored `w · vector + (1 - w) · text`, with 0 for a
    /// list it is absent from, except that a chunk still waiting for its vector
    /// takes a middling vector score instead; `w` is lower for a query that names an
    /// identifier ([`crate::search::lexical::names_identifier`]). Unlike rank
    /// fusion this keeps how far apart two candidates are, which is what lets
    /// a deeper pool help instead of diluting the top. `distance` is
    /// `1 - score`, so lower is better.
    pub fn search_hybrid(
        &self,
        query: &str,
        embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<crate::search::SearchResult>> {
        use std::collections::HashMap;

        let pool = HYBRID_POOL.max(limit);
        let vec_results = self.knn(embedding, pool, 0.0)?;
        let text_results = self.search_text(query, pool).unwrap_or_default();

        let dense_weight = if crate::search::lexical::names_identifier(query) {
            DENSE_WEIGHT_FOR_IDENTIFIER_QUERY
        } else {
            DENSE_WEIGHT
        };

        // Unit vectors: cosine = 1 - L2² / 2.
        let cosines: Vec<f64> = vec_results
            .iter()
            .map(|r| 1.0 - f64::from(r.distance).powi(2) / 2.0)
            .collect();
        let dense_norm = min_max(&cosines);
        // FTS5's bm25() is negative, more negative for a better match.
        let best_bm25 = text_results
            .iter()
            .map(|r| -f64::from(r.distance))
            .fold(0.0_f64, f64::max);

        let mut scores: HashMap<i64, f64> = HashMap::new();
        let mut by_id: HashMap<i64, crate::search::SearchResult> = HashMap::new();

        for (result, cosine) in vec_results.into_iter().zip(cosines) {
            *scores.entry(result.chunk_id).or_insert(0.0) += dense_weight * dense_norm(cosine);
            by_id.entry(result.chunk_id).or_insert(result);
        }

        let unscored: Vec<i64> = text_results
            .iter()
            .map(|r| r.chunk_id)
            .filter(|id| !scores.contains_key(id))
            .collect();
        let awaiting_vector = self.awaiting_vector(&unscored)?;

        for result in text_results {
            if awaiting_vector.contains(&result.chunk_id) {
                *scores.entry(result.chunk_id).or_insert(0.0) +=
                    dense_weight * AWAITING_VECTOR_DENSE_SCORE;
            }
            let text = if best_bm25 > 0.0 {
                -f64::from(result.distance) / best_bm25
            } else {
                0.0
            };
            *scores.entry(result.chunk_id).or_insert(0.0) += (1.0 - dense_weight) * text;
            by_id.entry(result.chunk_id).or_insert(result);
        }

        // `scores` is a HashMap, whose iteration order is reseeded per instance,
        // and exact score ties are common (identical chunks, or chunks that
        // reach only one list at its best score), so a sort on score alone
        // would leave order to that reseeding. Tie-break on corpus position,
        // not rowid, so two machines indexing the same tree agree.
        let mut ranked: Vec<(i64, f64)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| {
            b.1.total_cmp(&a.1)
                .then_with(|| tie_break_key(&by_id, a.0).cmp(&tie_break_key(&by_id, b.0)))
        });
        ranked.truncate(limit);

        let results = ranked
            .into_iter()
            .filter_map(|(id, score)| {
                by_id.remove(&id).map(|mut r| {
                    r.distance = (1.0 - score) as f32;
                    r
                })
            })
            .collect();

        Ok(results)
    }

    // Of `ids`, the chunks the embed queue will give a vector but has not yet.
    fn awaiting_vector(&self, ids: &[i64]) -> Result<std::collections::HashSet<i64>> {
        if ids.is_empty() {
            return Ok(std::collections::HashSet::new());
        }
        let placeholders = vec!["?"; ids.len()].join(",");
        let mut stmt = self.conn.prepare(&format!(
            "SELECT c.id FROM chunks c
             LEFT JOIN embeddings e ON e.chunk_id = c.id
             WHERE c.id IN ({placeholders}) AND c.text_only = 0 AND e.chunk_id IS NULL"
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(ids), |row| row.get(0))?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }
}

// Scales `values` onto [0, 1]. When they are all equal, each is the best of
// its list and scales to 1.
fn min_max(values: &[f64]) -> impl Fn(f64) -> f64 + use<> {
    let lo = values.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    move |v| if hi > lo { (v - lo) / (hi - lo) } else { 1.0 }
}

// Not the bare chunk_id: that rowid is assigned by indexing order and can
// differ between machines. end_line matters too, since a nested node
// starting on its parent's line shares (path, start_line) with the parent;
// the id settles the residual tie of two chunks over one identical span.
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

    // Register the sqlite-vec extension exactly once per test process.
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

    // A nested node beginning on its parent's line shares (path, start_line)
    // with the parent, differing only in end_line. Both fixtures insert the
    // wider span first, so rowid order is the opposite of span order.
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
    fn search_hybrid_breaks_score_ties_on_end_line_before_rowid() {
        let db = open_db();
        let outer = seed_span(&db, "src/h.rs", 1, 20, "fn hybridspanterm() {}");
        let inner = seed_span(&db, "src/h.rs", 1, 5, "fn hybridspanterm() {}");

        // Identical content and no vectors: identical scores, so the
        // tie-break alone orders them.
        let hits = db
            .search_hybrid("hybridspanterm", &unit_vec(1.0), 10)
            .expect("hybrid ok");
        assert_eq!(hits[0].distance, hits[1].distance);
        let ids: Vec<i64> = hits.iter().map(|h| h.chunk_id).collect();
        assert_eq!(
            ids,
            vec![inner, outer],
            "a score tie over one start line must order by end_line, not by rowid"
        );
    }

    // Chunk `semantic` is the best vector match and absent from full text;
    // `lexical` is the best full-text match and has no vector. The query's
    // shape decides which list leads.
    #[test]
    fn search_hybrid_leans_on_full_text_for_an_identifier_query() {
        let db = open_db();
        let semantic = seed_chunk_in(&db, "src/semantic.rs", "fn other() { alpha beta; }");
        db.insert_embedding(semantic, &unit_vec(1.0))
            .expect("embed");
        let lexical = seed_chunk_in(&db, "src/lexical.rs", "fn zebra_term() {}");
        db.insert_embedding(lexical, &unit_vec(0.0)).expect("embed");

        let first =
            |q: &str| db.search_hybrid(q, &unit_vec(1.0), 10).expect("hybrid ok")[0].chunk_id;
        assert_eq!(first("zebra term"), semantic);
        assert_eq!(first("zebra_term"), lexical);
    }

    // `both` is the weakest match in each list, ranked behind 30 chunks that
    // reach only one list. A candidate pool sized from the limit leaves it out
    // at a small limit and lets it in at a large one, where rank fusion would
    // put it first for appearing twice.
    #[test]
    fn search_hybrid_top_results_do_not_depend_on_the_limit() {
        let db = open_db();
        for i in 0..30_u16 {
            seed_chunk_in(
                &db,
                &format!("src/t{i:02}.rs"),
                "fn f() { limitterm(); limitterm(); }",
            );
            let id = seed_chunk_in(&db, &format!("src/v{i:02}.rs"), "fn f() { alpha; }");
            db.insert_embedding(id, &unit_vec(1.0 - f32::from(i) * 0.002))
                .expect("embed");
        }
        let both = seed_chunk_in(
            &db,
            "src/both.rs",
            "fn f() { limitterm(); pad pad pad pad pad pad pad pad pad pad }",
        );
        db.insert_embedding(both, &unit_vec(0.8)).expect("embed");

        let ids = |limit| -> Vec<i64> {
            db.search_hybrid("limitterm", &unit_vec(1.0), limit)
                .expect("hybrid ok")
                .iter()
                .map(|h| h.chunk_id)
                .collect()
        };
        let wide = ids(40);
        assert_eq!(ids(5), wide[..5]);
        assert_eq!(ids(10), wide[..10]);
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

    // Byte-identical text-only chunks score identically, the pervasive tie in
    // hybrid search.
    #[test]
    fn search_hybrid_breaks_score_ties_by_source_position() {
        let db = open_db();

        // Text-only (ADR-104): matches the FTS query, never embedded.
        for path in ["tests/t3.rs", "tests/t1.rs", "tests/t2.rs"] {
            seed_chunk_in(&db, path, "fn f() { scoretieterm(); }");
        }
        // Vector-only: embedded at increasing distance, no query term in content.
        for (path, w) in [("src/v1.rs", 1.0), ("src/v2.rs", 0.98), ("src/v3.rs", 0.95)] {
            let id = seed_chunk_in(&db, path, "fn f() { alpha beta gamma; }");
            db.insert_embedding(id, &unit_vec(w)).expect("embed");
        }

        let query_vec = unit_vec(1.0);
        let expected = vec![
            "src/v1.rs",
            "tests/t1.rs",
            "tests/t2.rs",
            "tests/t3.rs",
            "src/v2.rs",
            "src/v3.rs",
        ];

        // Repeated in-process: each call builds a fresh HashMap with a fresh
        // hash seed, so a lingering seed-dependent order shows up as flakiness.
        for i in 0..40 {
            let hits = db
                .search_hybrid("scoretieterm", &query_vec, 10)
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

    #[test]
    fn search_hybrid_keeps_a_chunk_awaiting_its_vector_above_the_text_share() {
        let db = open_db();
        let close = seed_span(&db, "src/close.rs", 1, 5, "fn close() { unrelated words }");
        let far = seed_span(&db, "src/far.rs", 1, 5, "fn far() { pending lookup }");
        let awaiting = seed_span(
            &db,
            "src/awaiting.rs",
            1,
            5,
            "fn awaiting() { pending lookup pending lookup }",
        );
        let text_only = seed_span(
            &db,
            "tests/text_only.rs",
            1,
            5,
            "fn text_only() { pending lookup pending lookup }",
        );
        db.insert_embeddings(&[(close, unit_vec(1.0)), (far, unit_vec(0.0))])
            .expect("embed");

        let ids: Vec<i64> = db
            .search_hybrid("pending lookup", &unit_vec(1.0), 10)
            .expect("hybrid ok")
            .iter()
            .map(|h| h.chunk_id)
            .collect();

        let at = |id: i64| ids.iter().position(|&x| x == id).expect("ranked");
        assert!(
            at(awaiting) < at(close),
            "an unembedded code chunk with the best text match outranks the nearest vector: {ids:?}"
        );
        assert!(
            at(close) < at(text_only),
            "a text-only chunk stays capped at the full-text share: {ids:?}"
        );
    }

    // Models a partially embedded index: an interrupted drain leaves vectors on
    // an exact prefix of the real embed queue order (never-embedded band
    // first, then graph_rank DESC, mtime DESC, id), with one drained chunk
    // flagged embed_pending = 1 to add the re-embed band. The vector list then
    // holds only that prefix while the text list holds every match, so the
    // identical not-yet-embedded chunks collide on identical scores.
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

        // Guard against a decorative test: require an exact score collision.
        // Without one the tie-break is never consulted, so the test would pass
        // with or without it.
        let fused = db
            .search_hybrid("warmupterm", &query_vec, 10)
            .expect("hybrid ok");
        let mut score_bits: Vec<u32> = fused.iter().map(|h| h.distance.to_bits()).collect();
        let distinct_before = score_bits.len();
        score_bits.sort_unstable();
        score_bits.dedup();
        assert!(
            score_bits.len() < distinct_before,
            "fixture must produce at least one exact score tie, else the tie-break is untested"
        );

        // 64 calls: `RandomState` reseeds per instance, so an unbroken tie is
        // an independent coin flip per call, giving a false pass probability
        // of 2^-64.
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

    // FTS5 reads `\0` as end of string, which would hide the closing quote.
    #[test]
    fn search_text_embedded_nul_byte_does_not_surface_parse_error() {
        let db = open_db();
        seed_chunk(&db, "fn parse_config() { /* handles foo:bar */ }");

        let result = db.search_text("\0embedded nul", 10);
        assert!(
            result.is_ok(),
            "query with embedded NUL must not surface a raw FTS5 parse error, got: {:?}",
            result.err()
        );
    }

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

    // `content:...` must not be interpreted as an FTS5 column filter.
    #[test]
    fn search_text_column_filter_syntax_is_literal_not_a_filter() {
        let db = open_db();
        seed_chunk(&db, "fn parse_config() { /* handles foo:bar */ }");

        let result = db.search_text("content:nonexistent_term_xyz", 10);
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    // `--mode text` scores independent terms: a multi-word query must match
    // regardless of word order or adjacency.
    #[test]
    fn search_text_scores_terms_independent_of_order() {
        let db = open_db();
        seed_chunk(
            &db,
            "We chose a token bucket over a leaky bucket because bursts are expected.",
        );

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

    // A chunk containing more query terms ranks above one containing fewer;
    // the fewer-term chunk still appears (bag-of-words, not phrase matching).
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

    // `chunks_fts` stems (`porter unicode61`) and case-folds, so a query for
    // `burst` reaches a chunk that only says `bursts`.
    #[test]
    fn search_text_stems_and_is_case_insensitive() {
        let db = open_db();
        seed_chunk(&db, "the queue absorbs bursts of traffic");

        for q in ["bursts", "burst", "BURSTS", "bursting"] {
            assert!(
                !db.search_text(q, 10).expect("ok").is_empty(),
                "{q:?} must reach the chunk containing `bursts`"
            );
        }
    }

    #[test]
    fn search_text_reaches_the_parts_of_a_camel_case_identifier() {
        let db = open_db();
        seed_chunk(&db, "fn run() { LinearRagSearch::new().go() }");

        assert!(
            !db.search_text("linear rag", 10).expect("ok").is_empty(),
            "the sub-words of `LinearRagSearch` must be searchable"
        );
    }

    #[test]
    fn search_text_indexes_the_file_path_docstring_and_summary() {
        let db = open_db();
        let file_id = db
            .upsert_file("src/billing/invoices.rs", Some("rust"), "h", 0)
            .expect("upsert file");
        let id = db
            .insert_chunk(
                file_id,
                "function",
                Some("f"),
                1,
                5,
                "fn f() {}",
                Some(r#"{"docstring":"/// Prorates the charge","parent_scope":null}"#),
                4,
            )
            .expect("insert chunk");
        db.update_chunk_summary(id, "applies a discount")
            .expect("summary");

        for q in ["invoices", "prorates", "discount"] {
            let hits = db.search_text(q, 10).expect("ok");
            assert_eq!(hits.len(), 1, "{q:?} must reach the chunk");
        }
    }

    #[test]
    fn search_text_ranks_a_name_match_above_a_body_match() {
        let db = open_db();
        let file_id = db
            .upsert_file("src/lib.rs", Some("rust"), "h", 0)
            .expect("upsert file");
        db.insert_chunk(
            file_id,
            "function",
            Some("other"),
            1,
            2,
            "fn other() { reconcile(); }",
            None,
            4,
        )
        .expect("insert chunk");
        db.insert_chunk(
            file_id,
            "function",
            Some("reconcile"),
            3,
            4,
            "fn reconcile() {}",
            None,
            4,
        )
        .expect("insert chunk");

        let hits = db.search_text("reconcile", 10).expect("ok");
        assert_eq!(hits[0].name.as_deref(), Some("reconcile"));
        assert!(
            hits[0].distance < hits[1].distance,
            "distance is lower-is-better, as for vector search"
        );
    }

    #[test]
    fn deleting_a_file_removes_its_chunks_from_the_full_text_index() {
        let db = open_db();
        let id = seed_chunk_in(&db, "src/gone.rs", "fn vanishingterm() {}");
        assert_eq!(db.search_text("vanishingterm", 10).expect("ok").len(), 1);

        db.conn
            .execute("DELETE FROM chunks WHERE id = ?1", [id])
            .expect("delete");
        assert!(db.search_text("vanishingterm", 10).expect("ok").is_empty());
    }

    // Every index run rewrites `graph_rank` on every chunk, so re-tokenising
    // for it would be pure cost. `total_changes` counts the rows a trigger
    // writes too, so a fired trigger would show up as more than one change.
    #[test]
    fn an_unrelated_column_update_does_not_rewrite_the_full_text_row() {
        let db = open_db();
        let id = seed_chunk_in(&db, "src/a.rs", "fn steadyterm() {}");

        let before = db.conn.total_changes();
        db.conn
            .execute(
                "UPDATE chunks SET graph_rank = 0.5, embed_pending = 1 WHERE id = ?1",
                [id],
            )
            .expect("update");
        assert_eq!(db.conn.total_changes() - before, 1);

        let before = db.conn.total_changes();
        db.update_chunk_summary(id, "a summary").expect("summary");
        assert!(
            db.conn.total_changes() - before > 1,
            "a summary change must re-index the row"
        );
        assert_eq!(db.search_text("steadyterm", 10).expect("ok").len(), 1);
    }
}
