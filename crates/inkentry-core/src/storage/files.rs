use anyhow::Result;

use super::Database;

/// A row from the `files` table, returned by [`Database::file_records_under`].
pub struct FileRecord {
    pub id: i64,
    pub path: String,
    pub language: Option<String>,
    pub hash: String,
    pub indexed_at: i64,
}

impl Database {
    /// Inserts or updates a file record. `mtime` is the file's modification
    /// time in unix seconds (0 when unavailable), which the embed queue orders
    /// by. It is refreshed only when the file is re-parsed.
    pub fn upsert_file(
        &self,
        path: &str,
        language: Option<&str>,
        hash: &str,
        mtime: i64,
    ) -> Result<i64> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let path_words = crate::search::lexical::identifier_subwords(path);
        self.conn.execute(
            "INSERT INTO files (path, language, hash, indexed_at, mtime, path_words)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(path) DO UPDATE SET
                language   = excluded.language,
                hash       = excluded.hash,
                indexed_at = excluded.indexed_at,
                mtime      = excluded.mtime,
                path_words = excluded.path_words",
            rusqlite::params![path, language, hash, now, mtime, path_words],
        )?;

        // ON CONFLICT UPDATE doesn't reset last_insert_rowid; fetch it explicitly.
        let id: i64 = self.conn.query_row(
            "SELECT id FROM files WHERE path = ?1",
            rusqlite::params![path],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    /// Returns the stored hash for a file path, or None if not indexed.
    pub fn file_hash(&self, path: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT hash FROM files WHERE path = ?1")?;
        let mut rows = stmt.query(rusqlite::params![path])?;
        Ok(rows.next()?.map(|r| r.get(0)).transpose()?)
    }

    /// Returns the stored modification time (unix seconds) for a file path, or
    /// None if not indexed.
    pub fn file_mtime(&self, path: &str) -> Result<Option<i64>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT mtime FROM files WHERE path = ?1")?;
        let mut rows = stmt.query(rusqlite::params![path])?;
        Ok(rows.next()?.map(|r| r.get(0)).transpose()?)
    }

    /// Whether a file has at least one stored chunk. A file whose hash is
    /// current but has none was interrupted between storing its hash and its
    /// first chunk, which the hash check alone cannot detect.
    pub fn file_has_chunks(&self, path: &str) -> Result<bool> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM chunks c JOIN files f ON f.id = c.file_id \
             WHERE f.path = ?1)",
        )?;
        stmt.query_row(rusqlite::params![path], |r| r.get::<_, bool>(0))
            .map_err(Into::into)
    }

    /// [`crate::indexer::chunker::chunked_by_tree`] for a file's stored chunks.
    pub fn file_chunked_by_tree(&self, path: &str) -> Result<bool> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM chunks c JOIN files f ON f.id = c.file_id \
             WHERE f.path = ?1 AND (c.node_type <> 'verbatim' OR c.name IS NOT NULL))",
        )?;
        stmt.query_row(rusqlite::params![path], |r| r.get::<_, bool>(0))
            .map_err(Into::into)
    }

    /// Look up the file id for a given path, or None if not indexed.
    pub fn file_id_for_path(&self, path: &str) -> Result<Option<i64>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT id FROM files WHERE path = ?1")?;
        let mut rows = stmt.query(rusqlite::params![path])?;
        Ok(rows.next()?.map(|r| r.get(0)).transpose()?)
    }

    /// Return all chunk IDs and their content for a given file id.
    pub fn chunks_content_for_file_id(&self, file_id: i64) -> Result<Vec<(i64, String)>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT id, content FROM chunks WHERE file_id = ?1 ORDER BY id")?;
        let rows = stmt.query_map(rusqlite::params![file_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// List all indexed file paths under the given root prefix.
    pub fn file_paths_under(&self, root: &str) -> Result<Vec<(i64, String)>> {
        let prefix = format!("{}%", super::escape_like(root));
        let mut stmt = self
            .conn
            .prepare_cached("SELECT id, path FROM files WHERE path LIKE ?1 ESCAPE '\\'")?;
        let rows = stmt.query_map(rusqlite::params![prefix], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// List all indexed files under the given root prefix, including hash and indexed_at.
    pub fn file_records_under(&self, root: &str) -> Result<Vec<FileRecord>> {
        let prefix = format!("{}%", super::escape_like(root));
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, path, language, hash, indexed_at FROM files WHERE path LIKE ?1 ESCAPE '\\'",
        )?;
        let rows = stmt.query_map(rusqlite::params![prefix], |r| {
            Ok(FileRecord {
                id: r.get(0)?,
                path: r.get(1)?,
                language: r.get(2)?,
                hash: r.get(3)?,
                indexed_at: r.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Delete a file record and all its chunks, embeddings, and graph edges.
    pub fn delete_file(&self, file_id: i64, file_path: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM embeddings WHERE chunk_id IN (SELECT id FROM chunks WHERE file_id = ?1)",
            rusqlite::params![file_id],
        )?;
        self.conn.execute(
            "DELETE FROM chunks WHERE file_id = ?1",
            rusqlite::params![file_id],
        )?;
        self.conn.execute(
            "DELETE FROM graph_edges WHERE source_file = ?1",
            rusqlite::params![file_path],
        )?;
        self.conn.execute(
            "DELETE FROM files WHERE id = ?1",
            rusqlite::params![file_id],
        )?;
        Ok(())
    }
}
