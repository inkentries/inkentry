use anyhow::Result;

use super::Database;

/// A graph edge as returned by query methods.
#[derive(Debug, Clone, serde::Serialize)]
pub struct GraphEdge {
    pub source_file: String,
    pub source_name: Option<String>,
    pub target_name: String,
    pub kind: String,
    pub line: usize,
    /// Repo-relative path of the file that defines `target_name`, when the
    /// resolution tier bound this edge to a specific definition (ADR-097).
    /// `None` means unresolved: every consumer's bare-name fallback still
    /// applies. Omitted from JSONL entirely rather than emitted `null`, so
    /// the field is additive to `plumbing graph-edges`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_file: Option<String>,
}

fn row_to_edge(row: &rusqlite::Row<'_>) -> rusqlite::Result<GraphEdge> {
    // Round-trip the stored `kind` through `EdgeKind::parse` so a row written
    // by an older/foreign schema variant with an unrecognised kind string
    // normalises to a known value instead of propagating an arbitrary string.
    let kind: String = row.get(3)?;
    Ok(GraphEdge {
        source_file: row.get(0)?,
        source_name: row.get(1)?,
        target_name: row.get(2)?,
        kind: crate::indexer::graph::EdgeKind::parse(&kind).to_string(),
        line: row.get::<_, i64>(4)? as usize,
        target_file: row.get(5)?,
    })
}

impl Database {
    /// Insert a batch of edges for one file. Existing rows for that file are
    /// removed first (called during re-index).
    pub fn replace_edges(
        &self,
        file_path: &str,
        edges: &[crate::indexer::graph::Edge],
    ) -> Result<()> {
        self.conn.execute(
            "DELETE FROM graph_edges WHERE source_file = ?1",
            rusqlite::params![file_path],
        )?;
        for e in edges {
            self.conn.execute(
                "INSERT INTO graph_edges (source_file, source_name, target_name, kind, line, target_file)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    e.source_file,
                    e.source_name,
                    e.target_name,
                    e.kind.to_string(),
                    e.line as i64,
                    e.target_file,
                ],
            )?;
        }
        Ok(())
    }

    /// All edges where `name` appears as source_name OR target_name.
    pub fn edges_for_symbol(&self, name: &str) -> Result<Vec<GraphEdge>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT source_file, source_name, target_name, kind, line, target_file
             FROM graph_edges
             WHERE source_name = ?1 OR target_name = ?1
             ORDER BY kind, target_name",
        )?;
        let rows = stmt.query_map(rusqlite::params![name], row_to_edge)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Whether the index holds any graph edge at all (any kind). Cheap probe
    /// that short-circuits; distinguishes "graph never populated" from "this
    /// symbol is absent from a populated graph".
    pub fn has_any_graph_edges(&self) -> Result<bool> {
        let exists: bool =
            self.conn
                .query_row("SELECT EXISTS(SELECT 1 FROM graph_edges)", [], |r| r.get(0))?;
        Ok(exists)
    }

    /// All edges originating from `file_path`.
    pub fn edges_for_file(&self, file_path: &str) -> Result<Vec<GraphEdge>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT source_file, source_name, target_name, kind, line, target_file
             FROM graph_edges
             WHERE source_file = ?1
             ORDER BY kind, target_name",
        )?;
        let rows = stmt.query_map(rusqlite::params![file_path], row_to_edge)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Return chunk IDs of symbols that are called-by or call the given chunk names.
    /// Used by `inkentry ask` to enrich context with graph neighbours.
    pub fn graph_neighbor_chunks(&self, names: &[&str]) -> Result<Vec<i64>> {
        if names.is_empty() {
            return Ok(vec![]);
        }
        // The names slice is bound twice per statement (once per IN clause), so
        // the effective per-chunk budget is half the bind limit.
        let chunk_size = super::sql::SQLITE_MAX_BIND / 2;
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for chunk in names.chunks(chunk_size) {
            let ph = super::sql::placeholders(chunk.len());
            // The callee side (first branch) disambiguates a resolved edge to
            // the chunk in its own file, per ADR-097; an unresolved
            // (target_file IS NULL) edge keeps today's every-same-name-chunk
            // join. The caller side (second branch) has no such column to
            // disambiguate on — source_name's file is always source_file, but
            // that identity isn't threaded through this name-only match — so
            // it is unchanged.
            let sql = format!(
                "SELECT DISTINCT c.id
                 FROM graph_edges ge
                 JOIN chunks c ON c.name = ge.target_name
                 LEFT JOIN files f ON f.id = c.file_id
                 WHERE ge.source_name IN ({ph}) AND ge.kind = 'calls'
                   AND (ge.target_file IS NULL OR ge.target_file = f.path)
                 UNION
                 SELECT DISTINCT c.id
                 FROM chunks c
                 WHERE c.name IN (
                     SELECT source_name FROM graph_edges
                     WHERE target_name IN ({ph}) AND kind = 'calls'
                 )"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            // Bind the names slice twice: once for each IN clause.
            let params: Vec<&dyn rusqlite::ToSql> = chunk
                .iter()
                .chain(chunk.iter())
                .map(|n| n as &dyn rusqlite::ToSql)
                .collect();
            debug_assert_eq!(params.len(), chunk.len() * 2);
            let rows = stmt.query_map(params.as_slice(), |r| r.get::<_, i64>(0))?;
            for row in rows {
                let id = row?;
                // Preserve the single-statement DISTINCT semantics across chunks.
                if seen.insert(id) {
                    out.push(id);
                }
            }
        }
        Ok(out)
    }

    /// Return all (source_name, target_name) pairs from graph_edges where
    /// source_name is non-NULL. Used by PageRank computation after indexing.
    /// Excludes 'mentions' edges — those are for LinearRAG, not structural PageRank.
    pub fn graph_edges_all(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT source_name, target_name FROM graph_edges \
             WHERE source_name IS NOT NULL AND kind != 'mentions'",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Append mention edges for a file's chunks (without deleting — caller must have
    /// already called `replace_edges` which clears all edge kinds including 'mentions').
    pub fn append_mention_edges(
        &self,
        file_path: &str,
        edges: &[(Option<&str>, &str)],
    ) -> Result<()> {
        for (source_name, target_name) in edges {
            self.conn.execute(
                "INSERT INTO graph_edges (source_file, source_name, target_name, kind, line) \
                 VALUES (?1, ?2, ?3, 'mentions', 0)",
                rusqlite::params![file_path, source_name, target_name],
            )?;
        }
        Ok(())
    }

    /// For each chunk in `chunk_ids`, return the symbols it mentions.
    /// Joins via source_name + source_file — only works for named chunks.
    pub fn mention_edges_for_chunks(
        &self,
        chunk_ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, Vec<String>>> {
        if chunk_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let mut map: std::collections::HashMap<i64, Vec<String>> = std::collections::HashMap::new();
        for chunk in chunk_ids.chunks(super::sql::SQLITE_MAX_BIND) {
            let ph = super::sql::placeholders(chunk.len());
            // CTE + INDEXED BY forces SQLite to start from chunk IDs rather than
            // scanning all 'mentions' edges — critical with 25k+ mention edges.
            let sql = format!(
                "WITH chunk_info AS MATERIALIZED (
                     SELECT c.id, c.name, f.path
                     FROM chunks c JOIN files f ON f.id = c.file_id
                     WHERE c.id IN ({ph})
                 )
                 SELECT ci.id, ge.target_name
                 FROM chunk_info ci
                 JOIN graph_edges ge INDEXED BY graph_edges_source_name_kind
                      ON ge.source_name = ci.name AND ge.source_file = ci.path
                      AND ge.kind IN ('mentions', 'calls')"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::ToSql> =
                chunk.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
            debug_assert_eq!(params.len(), chunk.len());
            let rows = stmt.query_map(params.as_slice(), |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (chunk_id, symbol) = row?;
                map.entry(chunk_id).or_default().push(symbol);
            }
        }
        Ok(map)
    }

    /// For each symbol in `symbols`, return the chunk IDs that mention it.
    pub fn chunks_mentioning_symbols(
        &self,
        symbols: &[&str],
    ) -> Result<std::collections::HashMap<String, Vec<i64>>> {
        if symbols.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let mut map: std::collections::HashMap<String, Vec<i64>> = std::collections::HashMap::new();
        for chunk in symbols.chunks(super::sql::SQLITE_MAX_BIND) {
            let ph = super::sql::placeholders(chunk.len());
            // Symbol values are user-file-derived (AST-extracted). They flow
            // strictly through bind parameters — the only thing interpolated
            // into the SQL text is the placeholder string `ph`, which contains
            // no caller data.
            let sql = format!(
                "SELECT ge.target_name, c.id
                 FROM graph_edges ge INDEXED BY graph_edges_target_name_kind
                 JOIN chunks c ON c.name = ge.source_name
                 JOIN files f ON f.id = c.file_id AND f.path = ge.source_file
                 WHERE ge.target_name IN ({ph})
                   AND ge.kind IN ('mentions', 'calls')"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::ToSql> =
                chunk.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
            debug_assert_eq!(params.len(), chunk.len());
            let rows = stmt.query_map(params.as_slice(), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?;
            for row in rows {
                let (symbol, chunk_id) = row?;
                map.entry(symbol).or_default().push(chunk_id);
            }
        }
        Ok(map)
    }
}

#[cfg(test)]
mod tests {
    use super::super::Database;
    use super::GraphEdge;
    use std::sync::OnceLock;

    /// Register the sqlite-vec extension exactly once per test process.
    /// `Database::open` creates a `vec0` virtual table, which requires the
    /// extension to be loaded before any connection is opened.
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

    /// Insert one named chunk in `src/lib.rs` and return its chunk id.
    fn insert_named_chunk(db: &Database, file_id: i64, name: &str) -> i64 {
        db.insert_chunk(
            file_id,
            "function",
            Some(name),
            1,
            5,
            &format!("fn {name}() {{}}"),
            None,
            4,
        )
        .expect("insert chunk")
    }

    /// Build a small, fully-known graph dataset:
    ///   chunks: caller (fn), callee (fn)
    ///   edges:  caller --calls--> callee   (structural)
    ///           caller --mentions--> mentioned_sym
    ///           caller --calls--> real_target
    /// Returns (file_id, caller_id, callee_id).
    fn seed_graph(db: &Database) -> (i64, i64, i64) {
        let file_id = db
            .upsert_file("src/lib.rs", Some("rust"), "deadbeef", 0)
            .expect("upsert file");
        let caller_id = insert_named_chunk(db, file_id, "caller");
        let callee_id = insert_named_chunk(db, file_id, "callee");
        db.replace_edges(
            "src/lib.rs",
            &[crate::indexer::graph::Edge {
                source_file: "src/lib.rs".to_string(),
                source_name: Some("caller".to_string()),
                target_name: "callee".to_string(),
                kind: crate::indexer::graph::EdgeKind::Calls,
                line: 2,
                target_file: None,
            }],
        )
        .expect("replace edges");
        db.append_mention_edges(
            "src/lib.rs",
            &[
                (Some("caller"), "mentioned_sym"),
                (Some("caller"), "callee"),
            ],
        )
        .expect("append mention edges");
        (file_id, caller_id, callee_id)
    }

    /// A symbol containing SQL metacharacters must be treated as a literal bind
    /// value: the query runs without error and returns no rows for it (there is
    /// no edge with that target_name), proving the bytes never reach the SQL
    /// text as code.
    #[test]
    fn chunks_mentioning_symbols_treats_metacharacters_as_literal() {
        let db = open_db();

        // Seed a real edge so the table is non-empty and a successful query
        // could in principle return rows — the injection string still must not.
        let file_id = db
            .upsert_file("src/lib.rs", Some("rust"), "deadbeef", 0)
            .expect("upsert file");
        db.insert_chunk(
            file_id,
            "function",
            Some("caller"),
            1,
            5,
            "fn caller() {}",
            None,
            4,
        )
        .expect("insert chunk");
        db.append_mention_edges("src/lib.rs", &[(Some("caller"), "real_target")])
            .expect("append edges");

        let malicious = "') OR 1=1 --";
        let map = db
            .chunks_mentioning_symbols(&[malicious, "real_target"])
            .expect("query must not error on a SQL-metacharacter symbol");

        // The injection attempt is a literal value with no matching edge.
        assert!(
            !map.contains_key(malicious),
            "metacharacter symbol must not match any edge (was treated as SQL?)"
        );
        // The legitimate symbol still resolves, proving the query is intact and
        // the malicious value did not widen or break the result set.
        assert_eq!(
            map.get("real_target").map(|v| v.len()),
            Some(1),
            "legitimate symbol must still resolve to its chunk"
        );
    }

    /// Strengthened literal-binding check: a battery of SQL-metacharacter
    /// payloads must each be treated as an opaque value (issue #405 priority
    /// variant). None may error, none may match an edge, and a structurally
    /// dangerous payload (`UNION SELECT`, comment terminators, stacked
    /// statements) must not leak extra rows or drop the legitimate result.
    #[test]
    fn chunks_mentioning_symbols_binds_injection_payloads_as_literals() {
        let db = open_db();
        seed_graph(&db);

        let payloads = [
            "') OR 1=1 --",
            "'; DROP TABLE chunks; --",
            "real_target') UNION SELECT id, id FROM chunks --",
            "\" OR \"\"=\"",
            "real_target' OR '1'='1",
            "%_\\",           // LIKE metacharacters (irrelevant here, must stay literal)
            "real_target\0x", // embedded NUL byte
        ];

        let mut query: Vec<&str> = payloads.to_vec();
        query.push("mentioned_sym"); // a genuinely-present target

        let map = db
            .chunks_mentioning_symbols(&query)
            .expect("injection payloads must bind as literals, never error");

        // The one real symbol resolves to exactly the caller chunk.
        assert_eq!(
            map.get("mentioned_sym").map(|v| v.len()),
            Some(1),
            "legitimate symbol must resolve despite injection neighbours"
        );
        // No payload is interpreted as SQL: none matches an edge, so none keys
        // the map. (A successful injection would either error above or surface
        // unexpected keys / extra chunk ids.)
        for p in payloads {
            assert!(
                !map.contains_key(p),
                "payload {p:?} must be treated as a literal value, not SQL"
            );
        }
        // The whole result set is exactly the single legitimate mapping.
        assert_eq!(map.len(), 1, "no payload may widen the result set");
    }

    // -------------------------------------------------------------------------
    // chunking across SQLITE_MAX_BIND (issue #405 §3)
    //
    // Chunking is keyed purely off input-slice length vs `SQLITE_MAX_BIND`, so
    // driving each function with an input list longer than the boundary forces
    // the multi-statement path. Only a few elements correspond to real DB rows;
    // the rest are non-matching filler. We assert (a) no prepare/bind error,
    // (b) the result equals the known single-statement result, and (c) that we
    // genuinely crossed >1 chunk.
    // -------------------------------------------------------------------------

    use super::super::sql::SQLITE_MAX_BIND;

    // ADR-097: a resolved edge (`target_file` set) must hit only the chunk
    // in the file it names, not every same-named chunk repo-wide; an
    // unresolved edge (`target_file` NULL) keeps today's bare-name fan-out.
    #[test]
    fn graph_neighbor_chunks_disambiguates_a_resolved_edge_by_target_file() {
        let db = open_db();
        let file_a = db
            .upsert_file("a.rs", Some("rust"), "ha", 0)
            .expect("upsert a.rs");
        let file_b = db
            .upsert_file("b.rs", Some("rust"), "hb", 0)
            .expect("upsert b.rs");
        let chunk_a = insert_named_chunk(&db, file_a, "dup");
        let chunk_b = insert_named_chunk(&db, file_b, "dup");
        db.upsert_file("caller.rs", Some("rust"), "hc", 0)
            .expect("upsert caller.rs");
        insert_named_chunk(&db, file_a, "caller_in_a");

        // Resolved: this edge names a.rs as the definer, so only chunk_a
        // must come back, even though a same-named chunk exists in b.rs.
        db.replace_edges(
            "a.rs",
            &[crate::indexer::graph::Edge {
                source_file: "a.rs".to_string(),
                source_name: Some("caller_in_a".to_string()),
                target_name: "dup".to_string(),
                kind: crate::indexer::graph::EdgeKind::Calls,
                line: 1,
                target_file: Some("a.rs".to_string()),
            }],
        )
        .expect("replace edges");

        let resolved = db
            .graph_neighbor_chunks(&["caller_in_a"])
            .expect("resolved lookup");
        assert_eq!(
            resolved,
            vec![chunk_a],
            "a resolved edge must hit only the chunk in its own file"
        );
        assert!(
            !resolved.contains(&chunk_b),
            "the same-named chunk in the other file must not appear"
        );

        // Unresolved: no target_file, so both same-named chunks still match —
        // today's behaviour, unchanged.
        db.replace_edges(
            "caller.rs",
            &[crate::indexer::graph::Edge {
                source_file: "caller.rs".to_string(),
                source_name: Some("caller_elsewhere".to_string()),
                target_name: "dup".to_string(),
                kind: crate::indexer::graph::EdgeKind::Calls,
                line: 1,
                target_file: None,
            }],
        )
        .expect("replace edges");

        let mut unresolved = db
            .graph_neighbor_chunks(&["caller_elsewhere"])
            .expect("unresolved lookup");
        unresolved.sort_unstable();
        let mut expected = vec![chunk_a, chunk_b];
        expected.sort_unstable();
        assert_eq!(
            unresolved, expected,
            "an unresolved edge must still fan out to every same-named chunk"
        );
    }

    #[test]
    fn graph_neighbor_chunks_chunks_and_merges_distinct() {
        let db = open_db();
        let (_file_id, _caller_id, callee_id) = seed_graph(&db);

        // graph_neighbor_chunks binds its slice twice, so the per-chunk budget
        // is SQLITE_MAX_BIND / 2. To force >1 chunk we need an input longer than
        // that half-budget. "caller" is the real query name (caller --calls-->
        // callee, so the neighbour chunk is `callee`).
        let chunk_budget = SQLITE_MAX_BIND / 2;
        let mut names: Vec<&str> = vec!["caller"]; // matches; pulls in `callee`
        names.resize(chunk_budget + 5, "no_such_symbol_xyz"); // filler past the boundary
        assert!(
            names.len() > chunk_budget,
            "input must exceed the halved per-chunk budget to exercise chunking"
        );

        let neighbours = db
            .graph_neighbor_chunks(&names)
            .expect("multi-chunk query must not hit a prepare/bind limit");

        // Compare against the single-statement result for the same logical query.
        let single = db
            .graph_neighbor_chunks(&["caller"])
            .expect("single-chunk query");
        assert_eq!(
            single,
            vec![callee_id],
            "single-statement baseline: caller's calls-neighbour is callee"
        );
        assert_eq!(
            neighbours, single,
            "chunked result must equal the single-statement result"
        );

        // DISTINCT across chunks: callee must appear exactly once even though the
        // matching name `caller` could in principle recur across chunk boundaries.
        assert_eq!(
            neighbours.iter().filter(|&&id| id == callee_id).count(),
            1,
            "DISTINCT semantics must be preserved across chunk merges"
        );
    }

    #[test]
    fn mention_edges_for_chunks_chunks_and_merges_per_key() {
        let db = open_db();
        let (_file_id, caller_id, _callee_id) = seed_graph(&db);

        // caller mentions: "mentioned_sym" and "callee" (mentions edges) plus the
        // structural calls edge to "callee" — kind IN ('mentions','calls').
        let single = db
            .mention_edges_for_chunks(&[caller_id])
            .expect("single-chunk query");
        let mut expected: Vec<String> = single.get(&caller_id).cloned().unwrap_or_default();
        expected.sort();
        assert!(
            !expected.is_empty(),
            "baseline: caller must mention at least one symbol"
        );

        // Drive with > SQLITE_MAX_BIND ids: the real id plus non-existent filler.
        let mut ids: Vec<i64> = vec![caller_id];
        // Use clearly-non-existent ids well outside the real rowid range.
        ids.extend((0..(SQLITE_MAX_BIND as i64 + 5)).map(|n| 1_000_000 + n));
        assert!(ids.len() > SQLITE_MAX_BIND, "must exceed bind budget");

        let merged = db
            .mention_edges_for_chunks(&ids)
            .expect("multi-chunk query must not hit a prepare/bind limit");

        let mut got: Vec<String> = merged.get(&caller_id).cloned().unwrap_or_default();
        got.sort();
        assert_eq!(
            got, expected,
            "per-key Vec must match the single-statement result after merge"
        );
        // No phantom keys from the filler ids.
        assert_eq!(
            merged.keys().copied().collect::<Vec<_>>(),
            vec![caller_id],
            "filler ids must not introduce spurious map keys"
        );
    }

    #[test]
    fn chunks_mentioning_symbols_chunks_and_merges_per_key() {
        let db = open_db();
        let (_file_id, caller_id, _callee_id) = seed_graph(&db);

        // Baseline single-statement result for the real symbol.
        let single = db
            .chunks_mentioning_symbols(&["mentioned_sym"])
            .expect("single-chunk query");
        assert_eq!(
            single.get("mentioned_sym"),
            Some(&vec![caller_id]),
            "baseline: mentioned_sym is mentioned by the caller chunk"
        );

        // Drive with > SQLITE_MAX_BIND symbols: the real one plus filler.
        let mut syms: Vec<&str> = vec!["mentioned_sym"];
        syms.resize(SQLITE_MAX_BIND + 5, "no_such_symbol_xyz");
        assert!(syms.len() > SQLITE_MAX_BIND, "must exceed bind budget");

        let merged = db
            .chunks_mentioning_symbols(&syms)
            .expect("multi-chunk query must not hit a prepare/bind limit");

        assert_eq!(
            merged.get("mentioned_sym"),
            Some(&vec![caller_id]),
            "per-key Vec must match the single-statement result after merge"
        );
        assert_eq!(
            merged.len(),
            1,
            "filler symbols must not introduce spurious map keys"
        );
    }

    #[test]
    fn has_any_graph_edges_reflects_population() {
        let db = open_db();
        assert!(
            !db.has_any_graph_edges().expect("probe ok"),
            "a fresh index holds no graph edges"
        );
        seed_graph(&db);
        assert!(
            db.has_any_graph_edges().expect("probe ok"),
            "after seeding, the probe reports edges present"
        );
    }

    // -------------------------------------------------------------------------
    // empty-input early-return (issue #405 §2.2 step 1)
    // -------------------------------------------------------------------------

    #[test]
    fn graph_functions_empty_input_early_return() {
        let db = open_db();
        seed_graph(&db);

        assert!(
            db.graph_neighbor_chunks(&[]).expect("empty ok").is_empty(),
            "graph_neighbor_chunks must early-return [] on empty input"
        );
        assert!(
            db.mention_edges_for_chunks(&[])
                .expect("empty ok")
                .is_empty(),
            "mention_edges_for_chunks must early-return empty map"
        );
        assert!(
            db.chunks_mentioning_symbols(&[])
                .expect("empty ok")
                .is_empty(),
            "chunks_mentioning_symbols must early-return empty map"
        );
    }

    // -------------------------------------------------------------------------
    // target_file round-trip and JSONL shape (ADR-097)
    // -------------------------------------------------------------------------

    // A resolved edge round-trips its `target_file` through `replace_edges`
    // and `edges_for_symbol`; an unresolved one stays `None`, not `Some("")`
    // or a stray empty string.
    #[test]
    fn edges_for_symbol_round_trips_target_file() {
        let db = open_db();
        db.upsert_file("a.rs", Some("rust"), "ha", 0)
            .expect("upsert file");
        db.replace_edges(
            "a.rs",
            &[
                crate::indexer::graph::Edge {
                    source_file: "a.rs".to_string(),
                    source_name: Some("resolved_caller".to_string()),
                    target_name: "shared".to_string(),
                    kind: crate::indexer::graph::EdgeKind::Calls,
                    line: 1,
                    target_file: Some("a.rs".to_string()),
                },
                crate::indexer::graph::Edge {
                    source_file: "a.rs".to_string(),
                    source_name: Some("unresolved_caller".to_string()),
                    target_name: "elsewhere".to_string(),
                    kind: crate::indexer::graph::EdgeKind::Calls,
                    line: 2,
                    target_file: None,
                },
            ],
        )
        .expect("replace edges");

        let resolved = db.edges_for_symbol("shared").expect("query");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].target_file.as_deref(), Some("a.rs"));

        let unresolved = db.edges_for_symbol("elsewhere").expect("query");
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].target_file, None);
    }

    // `target_file` carries `skip_serializing_if`, so `plumbing graph-edges`
    // JSONL omits the key entirely when unresolved (additive-field contract,
    // docs/stability.md) rather than emitting a `null`.
    #[test]
    fn graph_edge_json_omits_target_file_when_unresolved_and_includes_it_when_resolved() {
        let unresolved = GraphEdge {
            source_file: "a.rs".to_string(),
            source_name: None,
            target_name: "dup".to_string(),
            kind: "calls".to_string(),
            line: 1,
            target_file: None,
        };
        let resolved = GraphEdge {
            target_file: Some("a.rs".to_string()),
            ..unresolved.clone()
        };

        let unresolved_json = serde_json::to_value(&unresolved).unwrap();
        assert!(
            unresolved_json.get("target_file").is_none(),
            "unresolved must omit target_file, not emit null: {unresolved_json}"
        );

        let resolved_json = serde_json::to_value(&resolved).unwrap();
        assert_eq!(
            resolved_json.get("target_file"),
            Some(&serde_json::Value::String("a.rs".to_string()))
        );
    }
}
