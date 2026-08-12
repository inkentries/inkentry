use anyhow::Result;
use rusqlite::OptionalExtension;

use super::Database;

impl Database {
    #[allow(clippy::too_many_arguments)]
    pub fn insert_chunk(
        &self,
        file_id: i64,
        node_type: &str,
        name: Option<&str>,
        start_line: usize,
        end_line: usize,
        content: &str,
        metadata: Option<&str>,
        token_count: usize,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO chunks (file_id, node_type, name, start_line, end_line, content, metadata, token_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                file_id,
                node_type,
                name,
                start_line as i64,
                end_line as i64,
                content,
                metadata,
                token_count as i64,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Backfill token_count for all chunks where it is still 0 (existing indexes).
    /// Returns the number of rows updated.
    pub fn backfill_token_counts(&self) -> Result<usize> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT id, content FROM chunks WHERE token_count = 0")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        let pairs: Vec<(i64, String)> = rows.collect::<rusqlite::Result<Vec<_>>>()?;

        let count = pairs.len();
        for (id, content) in &pairs {
            let tc = crate::search::tokens::estimate_tokens(content) as i64;
            if tc > 0 {
                self.conn.execute(
                    "UPDATE chunks SET token_count = ?1 WHERE id = ?2",
                    rusqlite::params![tc, id],
                )?;
            }
        }
        Ok(count)
    }

    /// Return chunks the embed queue must (re-)process: those with **no** vector
    /// yet (never embedded) and those flagged `embed_pending = 1` (a stored
    /// vector that no longer reflects the chunk's current `embedding_text()` and
    /// must be re-embedded in place). Returns the raw fields needed to
    /// reconstruct the exact `Chunk::embedding_text()` document format plus the
    /// stored token estimate: `(chunk_id, name, metadata_json, summary, content,
    /// token_count)`. `token_count` may be 0 on a pre-backfill index.
    ///
    /// A plain re-`index` skips unchanged files by file-hash, so those chunks
    /// never reach the embed phase on their own; the index command unions the
    /// rows returned here into the embed batch so unchanged-but-unembedded
    /// chunks still get embedded without reparsing.
    #[allow(clippy::type_complexity)]
    pub fn chunks_missing_embeddings(
        &self,
    ) -> Result<
        Vec<(
            i64,
            Option<String>,
            Option<String>,
            Option<String>,
            String,
            usize,
        )>,
    > {
        // Priority bands within the one queue, data-driven, no cold/warm
        // branching:
        //   - (e.chunk_id IS NOT NULL) ASC leads, so never-embedded chunks (no
        //     vector, key 0) always sort ahead of pending re-embeds (have a
        //     vector, key 1): coverage is bought before refinement.
        //   - graph_rank DESC then puts PageRank-central code first. PageRank now
        //     runs before the embed phase, so this is honest on a cold first
        //     index too; a repo with no edges leaves every rank at the 0.0
        //     default and this key is inert.
        //   - f.mtime DESC orders by file recency; legacy/pre-migration rows
        //     carry mtime 0 and deterministically sort last.
        //   - c.id is the final deterministic tiebreak.
        let mut stmt = self.conn.prepare_cached(
            "SELECT c.id, c.name, c.metadata, c.summary, c.content, c.token_count
             FROM chunks c
             LEFT JOIN embeddings e ON e.chunk_id = c.id
             JOIN files f ON f.id = c.file_id
             WHERE e.chunk_id IS NULL OR c.embed_pending = 1
             ORDER BY (e.chunk_id IS NOT NULL) ASC, c.graph_rank DESC, f.mtime DESC, c.id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)? as usize,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn delete_chunks_for_file(&self, file_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM chunks WHERE file_id = ?1",
            rusqlite::params![file_id],
        )?;
        Ok(())
    }

    /// Fetch full `SearchResult` rows for a list of chunk IDs (used for graph
    /// neighbour enrichment in `ask`).
    pub fn chunks_by_ids(&self, ids: &[i64]) -> Result<Vec<crate::search::SearchResult>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let mut out = Vec::new();
        for chunk in ids.chunks(super::sql::SQLITE_MAX_BIND) {
            let ph = super::sql::placeholders(chunk.len());
            let sql = format!(
                "SELECT c.id, 0.0, c.node_type, c.name,
                        CAST(c.start_line AS INTEGER), CAST(c.end_line AS INTEGER),
                        c.content, f.path, f.language, c.token_count
                 FROM chunks c
                 JOIN files f ON f.id = c.file_id
                 WHERE c.id IN ({ph})
                 ORDER BY f.path, c.start_line, c.end_line, c.id"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::ToSql> =
                chunk.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
            debug_assert_eq!(params.len(), chunk.len());
            let rows = stmt.query_map(params.as_slice(), |row| {
                Ok(crate::search::SearchResult {
                    chunk_id: row.get(0)?,
                    distance: row.get(1)?,
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
            for row in rows {
                out.push(row?);
            }
        }
        Ok(out)
    }

    /// Return all chunks for a file path (exact match or LIKE suffix).
    /// Used by the `chunks` subcommand and `cat-chunks` plumbing command.
    pub fn chunks_for_file(&self, path: &str) -> Result<Vec<crate::search::SearchResult>> {
        // Escape LIKE metacharacters in the user-supplied path so that '%' and '_'
        // in real file names are treated as literals. ESCAPE '\\' activates the
        // backslash escape character in the SQLite LIKE expression.
        let escaped = super::escape_like(path);
        let suffix_pattern = format!("%{escaped}");
        let mut stmt = self.conn.prepare(
            "SELECT c.id, c.node_type, c.name,
                    CAST(c.start_line AS INTEGER), CAST(c.end_line AS INTEGER),
                    c.content, f.path, f.language, c.token_count
             FROM chunks c
             JOIN files f ON f.id = c.file_id
             WHERE f.path = ?1 OR f.path LIKE ?2 ESCAPE '\\'
             ORDER BY c.start_line",
        )?;
        let rows = stmt.query_map(rusqlite::params![path, suffix_pattern], |row| {
            Ok(crate::search::SearchResult {
                chunk_id: row.get(0)?,
                distance: 0.0,
                node_type: row.get(1)?,
                name: row.get(2)?,
                start_line: row.get::<_, i64>(3)? as usize,
                end_line: row.get::<_, i64>(4)? as usize,
                content: row.get(5)?,
                file_path: row.get(6)?,
                language: row.get(7)?,
                from_graph: false,
                governing_specs: vec![],
                token_count: row.get::<_, i64>(8)? as usize,
                project_name: None,
                project_path: None,
                summary: None,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Named chunks that have not yet had a structural summary composed
    /// (`summary IS NULL`). Returns `(id, name, metadata_json, content)`, ordered
    /// by id for a deterministic pass. A stored `""` (composed but suppressed for
    /// a secret hit, or genuinely empty) is not `NULL`, so it is never
    /// recomputed on a plain re-index — matching the existing refill guard.
    /// Title-less chunks are excluded here; their slot is built by tier-3 MMR
    /// selection, not structural composition.
    #[allow(clippy::type_complexity)]
    pub fn named_chunks_needing_summary(
        &self,
    ) -> Result<Vec<(i64, String, Option<String>, String)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, name, metadata, content
             FROM chunks
             WHERE name IS NOT NULL AND summary IS NULL
             ORDER BY id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Title-less chunks (`name IS NULL`) that already have a primary vector but
    /// no summary yet — the tier-3 MMR selection candidates. The primary vector
    /// is required because it is reused as the MMR centroid (no whole-chunk
    /// re-embed). Returns `(id, node_type, content)`, ordered by id.
    pub fn titleless_chunks_needing_selection(&self) -> Result<Vec<(i64, String, String)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT c.id, c.node_type, c.content
             FROM chunks c
             JOIN embeddings e ON e.chunk_id = c.id
             WHERE c.name IS NULL AND c.summary IS NULL
             ORDER BY c.id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// The stored primary embedding for a chunk, dequantised back to f32, or
    /// `None` if the chunk has no vector. Used as the MMR centroid in tier 3.
    pub fn embedding_for_chunk(&self, chunk_id: i64) -> Result<Option<Vec<f32>>> {
        let blob: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT embedding FROM embeddings WHERE chunk_id = ?1",
                rusqlite::params![chunk_id],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        Ok(blob.map(|b| crate::embeddings::int8_blob_to_vec(&b)))
    }

    /// Callee target names for a symbol, in the graph's deterministic SQL order
    /// (`ORDER BY target_name`). Used as the split-callees ingredient of a
    /// structural summary; the fixed order keeps the composed summary
    /// byte-identical across runs regardless of edge insertion order.
    pub fn callees_for_symbol(&self, name: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT target_name FROM graph_edges
             WHERE source_name = ?1 AND kind = 'calls'
             ORDER BY target_name",
        )?;
        let rows = stmt.query_map(rusqlite::params![name], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Set the composed summary for a single chunk. `""` marks a slot that was
    /// composed but suppressed (a secret hit) or genuinely empty, so it is not
    /// recomputed on a plain re-index.
    pub fn update_chunk_summary(&self, chunk_id: i64, summary: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE chunks SET summary = ?1 WHERE id = ?2",
            rusqlite::params![summary, chunk_id],
        )?;
        Ok(())
    }

    /// Write a tier-3 chunk's MMR-selected summary slot and flag it for in-place
    /// re-embed, atomically. The flag is set only after the summary is durably
    /// written, so a worker killed between the two never leaves a chunk that
    /// would re-embed without its selected slot; a kill before both recomputes
    /// the identical selection on resume (determinism).
    pub fn set_summary_and_mark_pending(&self, chunk_id: i64, summary: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE chunks SET summary = ?1, embed_pending = 1 WHERE id = ?2",
            rusqlite::params![summary, chunk_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Return all (chunk_id, name) pairs for chunks that have a name.
    /// Used to map PageRank scores back to chunk IDs.
    pub fn chunks_with_names(&self) -> Result<Vec<(i64, Option<String>)>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT id, name FROM chunks WHERE name IS NOT NULL")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Update the graph_rank score for a single chunk.
    pub fn update_graph_rank(&self, chunk_id: i64, score: f32) -> Result<()> {
        self.conn.execute(
            "UPDATE chunks SET graph_rank = ?1 WHERE id = ?2",
            rusqlite::params![score, chunk_id],
        )?;
        Ok(())
    }

    /// Batch-update graph_rank scores inside a transaction for performance.
    pub fn update_graph_ranks(&self, scores: &[(i64, f32)]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for (chunk_id, score) in scores {
            tx.execute(
                "UPDATE chunks SET graph_rank = ?1 WHERE id = ?2",
                rusqlite::params![score, chunk_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Database;
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

    /// Insert two chunks and return their ids.
    fn seed_two_chunks(db: &Database) -> (i64, i64) {
        let file_id = db
            .upsert_file("src/lib.rs", Some("rust"), "deadbeef", 0)
            .expect("upsert file");
        let a = db
            .insert_chunk(file_id, "function", Some("a"), 1, 5, "fn a() {}", None, 4)
            .expect("insert a");
        let b = db
            .insert_chunk(file_id, "function", Some("b"), 6, 9, "fn b() {}", None, 4)
            .expect("insert b");
        (a, b)
    }

    /// Upsert one file at `mtime` (unix secs) and insert a named chunk per entry
    /// in `names`, returning the chunk ids in insertion order.
    fn seed_file_chunks(db: &Database, path: &str, mtime: i64, names: &[&str]) -> Vec<i64> {
        let file_id = db
            .upsert_file(path, Some("rust"), "h", mtime)
            .expect("upsert file");
        names
            .iter()
            .map(|n| {
                db.insert_chunk(file_id, "function", Some(n), 1, 2, "fn x() {}", None, 4)
                    .expect("insert chunk")
            })
            .collect()
    }

    fn missing_ids(db: &Database) -> Vec<i64> {
        db.chunks_missing_embeddings()
            .expect("query missing")
            .into_iter()
            .map(|(id, ..)| id)
            .collect()
    }

    /// Cold index: every `graph_rank` is the 0.0 default, so the embed queue is
    /// ordered by `files.mtime DESC` (most-recently-modified file first) with
    /// `chunks.id` breaking ties within a file — and this is deterministic
    /// across repeated calls on the same data.
    #[test]
    fn chunks_missing_embeddings_cold_orders_by_mtime_desc_then_id() {
        let db = open_db();
        // Older file seeded first (smaller ids); newer file second.
        let old = seed_file_chunks(&db, "old.rs", 100, &["a1", "a2"]);
        let new = seed_file_chunks(&db, "new.rs", 200, &["b1", "b2"]);

        let expected = vec![new[0], new[1], old[0], old[1]];
        assert_eq!(
            missing_ids(&db),
            expected,
            "recent file's chunks first (mtime DESC), id tiebreak within a file"
        );
        // Deterministic across a second identical call.
        assert_eq!(
            missing_ids(&db),
            expected,
            "queue order is deterministic across runs on the same fixture"
        );
    }

    /// Warm re-index: the prior run's `graph_rank DESC` leads (hot code first);
    /// chunks with no rank yet (`graph_rank = 0`, e.g. newly added) sort after,
    /// ordered by `mtime DESC`.
    #[test]
    fn chunks_missing_embeddings_warm_orders_by_graph_rank_then_mtime() {
        let db = open_db();
        let a = seed_file_chunks(&db, "a.rs", 100, &["a1", "a2"]); // older file
        let b = seed_file_chunks(&db, "b.rs", 200, &["b1", "b2"]); // newer file

        // Prior run populated ranks for a2 and b1 only; a1 and b2 stay at 0.
        db.update_graph_rank(a[1], 0.9).unwrap();
        db.update_graph_rank(b[0], 0.5).unwrap();

        // graph_rank DESC: a2(0.9), b1(0.5); then unranked by mtime DESC:
        // b2 (mtime 200) before a1 (mtime 100).
        let expected = vec![a[1], b[0], b[1], a[0]];
        assert_eq!(
            missing_ids(&db),
            expected,
            "graph_rank leads; unranked (rank 0) chunks follow by mtime DESC"
        );
    }

    /// Legacy/pre-migration rows carry `mtime = 0` (or `modified()` was
    /// unavailable). They must not error and must sort deterministically after
    /// positive mtimes, `chunks.id` breaking ties.
    #[test]
    fn chunks_missing_embeddings_legacy_mtime_zero_sorts_last() {
        let db = open_db();
        let legacy = seed_file_chunks(&db, "legacy.rs", 0, &["l1", "l2"]);
        let fresh = seed_file_chunks(&db, "fresh.rs", 200, &["f1", "f2"]);

        let expected = vec![fresh[0], fresh[1], legacy[0], legacy[1]];
        assert_eq!(
            missing_ids(&db),
            expected,
            "mtime=0 rows sort after positive mtimes, id tiebreak, no error"
        );
    }

    /// A bulk-copied file tree (e.g. `cp -r` or a fresh checkout) commonly
    /// leaves every file with an *identical* mtime, and a cold index leaves
    /// every chunk's `graph_rank` at the shared `0.0` default. With both
    /// leading keys tied across many rows, the ordering must not fall through
    /// to SQLite's unspecified tie-break: `c.id` must fully determine the
    /// order, identically across repeated calls.
    #[test]
    fn chunks_missing_embeddings_many_ties_are_fully_determined_by_id() {
        let db = open_db();
        let mut expected = Vec::new();
        // 6 files, identical mtime, 3 chunks each: 18 rows all tied on
        // (graph_rank=0.0, mtime=500).
        for i in 0..6 {
            let names = ["x", "y", "z"];
            let ids = seed_file_chunks(&db, &format!("f{i}.rs"), 500, &names);
            expected.extend(ids);
        }
        // Ascending c.id is the only remaining discriminator once graph_rank
        // and mtime are constant across every row.
        let mut sorted_expected = expected.clone();
        sorted_expected.sort();
        assert_eq!(
            expected, sorted_expected,
            "sanity: ids were inserted in ascending order"
        );

        assert_eq!(
            missing_ids(&db),
            expected,
            "fully-tied rows (graph_rank, mtime) must resolve to ascending c.id"
        );
        // Repeat: no run-to-run flake from an underspecified ORDER BY.
        assert_eq!(
            missing_ids(&db),
            expected,
            "tie-break must be stable across repeated calls, not left to SQLite's whim"
        );
    }

    /// Many chunks can share the same *non-zero* `graph_rank` too (e.g. several
    /// leaf functions PageRank scores to the same value). Ties within a shared
    /// rank must fall through to `mtime DESC`, then `c.id`, not collapse to an
    /// arbitrary order.
    #[test]
    fn chunks_missing_embeddings_tied_nonzero_rank_falls_back_to_mtime_then_id() {
        let db = open_db();
        let a = seed_file_chunks(&db, "a.rs", 100, &["a1", "a2"]);
        let b = seed_file_chunks(&db, "b.rs", 300, &["b1", "b2"]);
        let c = seed_file_chunks(&db, "c.rs", 300, &["c1", "c2"]);

        // All six chunks share the same non-zero rank.
        for id in a.iter().chain(&b).chain(&c) {
            db.update_graph_rank(*id, 0.42).unwrap();
        }

        // Tied on rank: mtime DESC groups b/c (300) ahead of a (100); within
        // the b/c tie (same rank AND same mtime), c.id is the final tiebreak.
        let expected = vec![b[0], b[1], c[0], c[1], a[0], a[1]];
        assert_eq!(
            missing_ids(&db),
            expected,
            "a shared non-zero graph_rank must fall back to mtime DESC, then id"
        );
    }

    /// A file with a modification time far in the future (clock skew, or a
    /// deliberately touched file) must sort ahead of every normal-mtime row,
    /// and the query must not error or panic on a large positive `i64`.
    #[test]
    fn chunks_missing_embeddings_future_mtime_sorts_first_no_panic() {
        let db = open_db();
        let normal = seed_file_chunks(&db, "normal.rs", 1_000, &["n1"]);
        // Comfortably in the future (year ~2107) without approaching i64::MAX,
        // matching what a skewed system clock could plausibly report.
        let skewed = seed_file_chunks(&db, "skewed.rs", 4_300_000_000, &["s1"]);

        let expected = vec![skewed[0], normal[0]];
        assert_eq!(
            missing_ids(&db),
            expected,
            "a future/skewed mtime must sort first, not error"
        );
    }

    /// A negative mtime (not producible by `stat_mtime`'s own fallback, which
    /// always yields 0 on failure, but defensive against any other write path)
    /// must not error the ORDER BY and must sort after both positive and
    /// zero/legacy mtimes.
    #[test]
    fn chunks_missing_embeddings_negative_mtime_sorts_last_no_error() {
        let db = open_db();
        let negative = seed_file_chunks(&db, "negative.rs", -100, &["neg"]);
        let legacy = seed_file_chunks(&db, "legacy.rs", 0, &["leg"]);
        let fresh = seed_file_chunks(&db, "fresh.rs", 200, &["fr"]);

        let expected = vec![fresh[0], legacy[0], negative[0]];
        assert_eq!(
            missing_ids(&db),
            expected,
            "DESC ordering must place negative mtime after 0 and positive mtimes, without erroring"
        );
    }

    /// A chunk with no matching `embeddings` row must surface via
    /// `chunks_missing_embeddings` (the parse phase unions these into the embed
    /// batch so a parse-only index doesn't leave chunks permanently
    /// unembedded). Once an embedding is inserted, the chunk
    /// drops out of the result.
    #[test]
    fn chunks_missing_embeddings_finds_unembedded_then_clears() {
        let db = open_db();
        let (a, b) = seed_two_chunks(&db);

        // Both chunks parsed, neither embedded yet.
        let mut missing: Vec<i64> = db
            .chunks_missing_embeddings()
            .expect("query missing")
            .into_iter()
            .map(|(id, ..)| id)
            .collect();
        missing.sort();
        assert_eq!(missing, vec![a, b], "both unembedded chunks are missing");

        // Embed one; only the other remains missing.
        db.insert_embedding(a, &[0.1f32; 896])
            .expect("insert embedding");
        let still_missing: Vec<i64> = db
            .chunks_missing_embeddings()
            .expect("query missing")
            .into_iter()
            .map(|(id, ..)| id)
            .collect();
        assert_eq!(still_missing, vec![b], "embedded chunk drops out");
    }

    /// The tiered queue puts never-embedded chunks (no vector) ahead of pending
    /// re-embeds (a vector present, `embed_pending = 1`) via the leading
    /// `(e.chunk_id IS NOT NULL) ASC` key, and excludes fully-current chunks.
    #[test]
    fn chunks_missing_embeddings_never_embedded_sort_before_pending_reembeds() {
        let db = open_db();
        let ids = seed_file_chunks(&db, "a.rs", 100, &["a", "b", "c"]);
        // a: embedded then flagged for re-embed; b: embedded and current; c: never embedded.
        db.insert_embedding(ids[0], &[0.1f32; 896]).unwrap();
        db.insert_embedding(ids[1], &[0.1f32; 896]).unwrap();
        db.conn
            .execute(
                "UPDATE chunks SET embed_pending = 1 WHERE id = ?1",
                rusqlite::params![ids[0]],
            )
            .unwrap();

        assert_eq!(
            missing_ids(&db),
            vec![ids[2], ids[0]],
            "never-embedded chunk first, then the pending re-embed; the current chunk is excluded"
        );
    }

    /// `named_chunks_needing_summary` returns only named chunks whose summary is
    /// still NULL — a title-less chunk (tier-3's domain) and an already-composed
    /// (or suppressed `\"\"`) summary are both excluded, so a plain re-index never
    /// recomputes them.
    #[test]
    fn named_chunks_needing_summary_excludes_titleless_and_already_composed() {
        let db = open_db();
        let file_id = db.upsert_file("a.rs", Some("rust"), "h", 0).unwrap();
        let named_todo = db
            .insert_chunk(
                file_id,
                "function",
                Some("todo"),
                1,
                2,
                "fn todo(){}",
                None,
                4,
            )
            .unwrap();
        let named_done = db
            .insert_chunk(
                file_id,
                "function",
                Some("done"),
                3,
                4,
                "fn done(){}",
                None,
                4,
            )
            .unwrap();
        let titleless = db
            .insert_chunk(file_id, "verbatim", None, 5, 9, "some prose", None, 4)
            .unwrap();
        db.update_chunk_summary(named_done, "already composed")
            .unwrap();

        let got: Vec<i64> = db
            .named_chunks_needing_summary()
            .unwrap()
            .into_iter()
            .map(|(id, ..)| id)
            .collect();
        assert_eq!(got, vec![named_todo]);
        assert!(!got.contains(&named_done));
        assert!(!got.contains(&titleless));
    }

    /// Callees are returned in the graph's deterministic `ORDER BY target_name`,
    /// regardless of edge insertion order — the invariant that keeps a composed
    /// summary byte-identical across runs.
    #[test]
    fn callees_for_symbol_are_ordered_deterministically() {
        use crate::indexer::graph::{Edge, EdgeKind};
        let db = open_db();
        db.upsert_file("a.rs", Some("rust"), "h", 0).unwrap();
        // Insert in a deliberately unsorted order.
        let edge = |target: &str| Edge {
            source_file: "a.rs".to_string(),
            source_name: Some("caller".to_string()),
            target_name: target.to_string(),
            kind: EdgeKind::Calls,
            line: 1,
        };
        db.replace_edges("a.rs", &[edge("zeta"), edge("alpha"), edge("mu")])
            .unwrap();
        assert_eq!(
            db.callees_for_symbol("caller").unwrap(),
            vec!["alpha", "mu", "zeta"]
        );
    }

    /// The tier-3 writer sets the summary slot and the re-embed flag together;
    /// the selection candidate query returns only title-less chunks that already
    /// have a primary vector and no summary yet.
    #[test]
    fn titleless_selection_candidates_and_pending_writer() {
        let db = open_db();
        let file_id = db.upsert_file("a.rs", Some("rust"), "h", 0).unwrap();
        let titleless = db
            .insert_chunk(file_id, "verbatim", None, 1, 4, "prose here", None, 4)
            .unwrap();
        let titleless_no_vec = db
            .insert_chunk(file_id, "verbatim", None, 5, 8, "more prose", None, 4)
            .unwrap();
        let named = db
            .insert_chunk(file_id, "function", Some("f"), 9, 10, "fn f(){}", None, 4)
            .unwrap();
        db.insert_embedding(titleless, &[0.2f32; 896]).unwrap();
        db.insert_embedding(named, &[0.2f32; 896]).unwrap();

        let candidates: Vec<i64> = db
            .titleless_chunks_needing_selection()
            .unwrap()
            .into_iter()
            .map(|(id, ..)| id)
            .collect();
        assert_eq!(
            candidates,
            vec![titleless],
            "only a title-less chunk with a primary vector and no summary is a candidate"
        );
        assert!(!candidates.contains(&titleless_no_vec));
        assert!(!candidates.contains(&named));

        // The stored primary vector is retrievable as the MMR centroid.
        assert!(db.embedding_for_chunk(titleless).unwrap().is_some());
        assert!(db.embedding_for_chunk(titleless_no_vec).unwrap().is_none());

        db.set_summary_and_mark_pending(titleless, "selected units")
            .unwrap();
        let (summary, pending): (Option<String>, i64) = db
            .conn
            .query_row(
                "SELECT summary, embed_pending FROM chunks WHERE id = ?1",
                rusqlite::params![titleless],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(summary.as_deref(), Some("selected units"));
        assert_eq!(
            pending, 1,
            "the writer flags the chunk for in-place re-embed"
        );
    }

    #[test]
    fn chunks_by_ids_empty_input_early_return() {
        let db = open_db();
        seed_two_chunks(&db);
        assert!(
            db.chunks_by_ids(&[]).expect("empty ok").is_empty(),
            "chunks_by_ids must early-return [] on empty input"
        );
    }

    /// Chunking across SQLITE_MAX_BIND (issue #405 §3): an input list longer
    /// than the bind budget must run multiple statements without a prepare/bind
    /// error and concatenate the result vecs, matching the single-statement
    /// result for the real ids.
    #[test]
    fn chunks_by_ids_chunks_and_concatenates() {
        use super::super::sql::SQLITE_MAX_BIND;

        let db = open_db();
        let (a, b) = seed_two_chunks(&db);

        // Baseline: both real ids in one statement.
        let single = db.chunks_by_ids(&[a, b]).expect("single-chunk query");
        let mut single_ids: Vec<i64> = single.iter().map(|r| r.chunk_id).collect();
        single_ids.sort();
        assert_eq!(single_ids, vec![a, b], "baseline returns both chunks");

        // Drive with > SQLITE_MAX_BIND ids: the two real ids plus filler that
        // matches no row. This straddles the chunk boundary.
        let mut ids: Vec<i64> = vec![a, b];
        ids.extend((0..(SQLITE_MAX_BIND as i64 + 5)).map(|n| 1_000_000 + n));
        assert!(ids.len() > SQLITE_MAX_BIND, "must exceed the bind budget");

        let merged = db
            .chunks_by_ids(&ids)
            .expect("multi-chunk query must not hit a prepare/bind limit");

        let mut merged_ids: Vec<i64> = merged.iter().map(|r| r.chunk_id).collect();
        merged_ids.sort();
        assert_eq!(
            merged_ids, single_ids,
            "concatenated chunked result must equal the single-statement result"
        );
    }
}
