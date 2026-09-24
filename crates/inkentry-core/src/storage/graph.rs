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
    /// The file defining the callee, when the edge resolved to one.
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
        let mut stmt = self.conn.prepare_cached(
            "INSERT INTO graph_edges (source_file, source_name, target_name, kind, line, target_file)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for e in edges {
            stmt.execute(rusqlite::params![
                e.source_file,
                e.source_name,
                e.target_name,
                e.kind.to_string(),
                e.line as i64,
                e.target_file,
            ])?;
        }
        Ok(())
    }

    /// All edges where `name` appears as source_name OR target_name.
    pub fn edges_for_symbol(&self, name: &str) -> Result<Vec<GraphEdge>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT source_file, source_name, target_name, kind, line, target_file
             FROM graph_edges
             WHERE source_name = ?1 OR target_name = ?1
             ORDER BY kind, target_name, target_file",
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
             ORDER BY kind, target_name, target_file",
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
            let sql = format!(
                "SELECT DISTINCT c.id
                 FROM chunks c
                 WHERE c.name IN (
                     SELECT target_name FROM graph_edges
                     WHERE source_name IN ({ph}) AND kind = 'calls'
                     UNION
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
    /// Excludes the `mentions` rows an index built before they were retired
    /// may still hold: they were never structural, and would skew PageRank.
    /// One call site can resolve to several `target_file` rows; it still
    /// counts once.
    pub fn graph_edges_all(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT source_name, target_name FROM graph_edges \
             WHERE source_name IS NOT NULL AND kind != 'mentions' \
             GROUP BY source_file, source_name, target_name, kind",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::super::Database;
    use crate::indexer::graph::{Edge, EdgeKind};
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

    // chunks: caller, callee; edges: caller --calls--> callee.
    // Returns (file_id, caller_id, callee_id).
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
        (file_id, caller_id, callee_id)
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
    }

    // ── target_file ─────────────────────────────────────────────────────────

    fn call(source_file: &str, source: &str, target: &str, target_file: Option<&str>) -> Edge {
        Edge {
            source_file: source_file.to_owned(),
            source_name: Some(source.to_owned()),
            target_name: target.to_owned(),
            kind: EdgeKind::Calls,
            line: 1,
            target_file: target_file.map(str::to_owned),
        }
    }

    fn define(db: &Database, path: &str, name: &str) {
        let file_id = match db.file_id_for_path(path).unwrap() {
            Some(id) => id,
            None => db.upsert_file(path, Some("rust"), "h", 0).unwrap(),
        };
        insert_named_chunk(db, file_id, name);
    }

    #[test]
    fn edges_differing_only_in_target_file_are_stored_as_distinct_rows() {
        let db = open_db();
        db.replace_edges(
            "src/a.rs",
            &[
                call("src/a.rs", "go", "helper", Some("src/b.rs")),
                call("src/a.rs", "go", "helper", Some("src/c.rs")),
            ],
        )
        .unwrap();
        let mut targets: Vec<_> = db
            .edges_for_file("src/a.rs")
            .unwrap()
            .into_iter()
            .map(|e| (e.target_name, e.target_file))
            .collect();
        targets.sort();
        assert_eq!(
            targets,
            vec![
                ("helper".to_owned(), Some("src/b.rs".to_owned())),
                ("helper".to_owned(), Some("src/c.rs".to_owned())),
            ]
        );
    }

    #[test]
    fn an_unresolved_edge_still_joins_every_same_named_definition() {
        let db = open_db();
        define(&db, "src/a.rs", "go");
        define(&db, "src/b.rs", "helper");
        define(&db, "src/c.rs", "helper");
        db.replace_edges("src/a.rs", &[call("src/a.rs", "go", "helper", None)])
            .unwrap();

        let neighbours = db.graph_neighbor_chunks(&["go"]).unwrap();
        assert_eq!(
            neighbours.len(),
            2,
            "a NULL target_file edge reaches both `helper` chunks, as before"
        );
    }

    #[test]
    fn a_pair_counts_once_for_pagerank_however_many_target_files_it_has() {
        let db = open_db();
        db.replace_edges(
            "src/a.ts",
            &[
                call("src/a.ts", "run", "helper", None),
                call("src/a.ts", "run", "helper", Some("src/a.ts")),
            ],
        )
        .unwrap();
        // A same-named caller in another file is its own edge, as it always was.
        db.replace_edges("src/b.ts", &[call("src/b.ts", "run", "helper", None)])
            .unwrap();

        let mut pairs = db.graph_edges_all().unwrap();
        pairs.sort();
        let pair = ("run".to_owned(), "helper".to_owned());
        assert_eq!(pairs, vec![pair.clone(), pair]);
    }

    #[test]
    fn a_callee_is_listed_once_for_its_caller_however_many_target_files_it_has() {
        let db = open_db();
        db.replace_edges(
            "src/a.ts",
            &[
                call("src/a.ts", "run", "helper", None),
                call("src/a.ts", "run", "helper", Some("src/a.ts")),
            ],
        )
        .unwrap();

        assert_eq!(db.callees_for_symbol("run").unwrap(), vec!["helper"]);
    }
}
