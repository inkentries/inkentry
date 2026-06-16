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
}

fn row_to_edge(row: &rusqlite::Row<'_>) -> rusqlite::Result<GraphEdge> {
    Ok(GraphEdge {
        source_file: row.get(0)?,
        source_name: row.get(1)?,
        target_name: row.get(2)?,
        kind: row.get(3)?,
        line: row.get::<_, i64>(4)? as usize,
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
                "INSERT INTO graph_edges (source_file, source_name, target_name, kind, line)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    e.source_file,
                    e.source_name,
                    e.target_name,
                    e.kind.to_string(),
                    e.line as i64
                ],
            )?;
        }
        Ok(())
    }

    /// All edges where `name` appears as source_name OR target_name.
    pub fn edges_for_symbol(&self, name: &str) -> Result<Vec<GraphEdge>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT source_file, source_name, target_name, kind, line
             FROM graph_edges
             WHERE source_name = ?1 OR target_name = ?1
             ORDER BY kind, target_name",
        )?;
        let rows = stmt.query_map(rusqlite::params![name], row_to_edge)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// All edges originating from `file_path`.
    pub fn edges_for_file(&self, file_path: &str) -> Result<Vec<GraphEdge>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT source_file, source_name, target_name, kind, line
             FROM graph_edges
             WHERE source_file = ?1
             ORDER BY kind, target_name",
        )?;
        let rows = stmt.query_map(rusqlite::params![file_path], row_to_edge)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Return chunk IDs of symbols that are called-by or call the given chunk names.
    /// Used by `spelunk ask` to enrich context with graph neighbours.
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
            .upsert_file("src/lib.rs", Some("rust"), "deadbeef")
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
}
