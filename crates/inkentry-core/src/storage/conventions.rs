//! Storage methods for the `conventions` table.

// Must not import `crate::conventions`, which imports `Database` from here.

use anyhow::Result;

use super::Database;

/// A chunk in the compact form convention extraction reads.
#[derive(Debug, Clone)]
pub struct RawChunkRow {
    pub language: String,
    pub node_type: String,
    pub name: Option<String>,
    pub content: String,
    pub file_path: String,
}

/// A row of the `conventions` table.
#[derive(Debug, Clone)]
pub struct ConventionRow {
    pub language: String,
    pub category: String,
    pub description: String,
    pub confidence: f32,
    pub evidence_count: u32,
    pub extracted_at: i64,
}

impl Database {
    /// Returns every chunk in the compact form convention extraction reads,
    /// recording whether its content starts with a doc-comment prefix.
    pub fn all_chunks_for_conventions(&self) -> Result<Vec<RawChunkRow>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT c.node_type,
                    c.name,
                    c.content,
                    f.path,
                    COALESCE(f.language, 'unknown')
             FROM chunks c
             JOIN files f ON f.id = c.file_id
             ORDER BY c.id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(RawChunkRow {
                node_type: row.get(0)?,
                name: row.get(1)?,
                content: row.get(2)?,
                file_path: row.get(3)?,
                language: row.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Replaces all convention records with `records` in one transaction.
    pub fn replace_conventions(&self, records: &[ConventionRow]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM conventions", [])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO conventions
                 (language, category, description, confidence, evidence_count, extracted_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for r in records {
                stmt.execute(rusqlite::params![
                    r.language,
                    r.category,
                    r.description,
                    r.confidence,
                    r.evidence_count as i64,
                    r.extracted_at,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Fetch all stored convention rows, optionally filtered by language.
    pub fn list_conventions(&self, language: Option<&str>) -> Result<Vec<ConventionRow>> {
        let rows: Vec<ConventionRow> = if let Some(lang) = language {
            let mut stmt = self.conn.prepare_cached(
                "SELECT language, category, description, confidence, evidence_count, extracted_at
                 FROM conventions
                 WHERE language = ?1
                 ORDER BY confidence DESC",
            )?;
            stmt.query_map(rusqlite::params![lang], map_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            let mut stmt = self.conn.prepare_cached(
                "SELECT language, category, description, confidence, evidence_count, extracted_at
                 FROM conventions
                 ORDER BY language, confidence DESC",
            )?;
            stmt.query_map([], map_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(rows)
    }
}

fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ConventionRow> {
    Ok(ConventionRow {
        language: row.get(0)?,
        category: row.get(1)?,
        description: row.get(2)?,
        confidence: row.get(3)?,
        evidence_count: row.get::<_, i64>(4)? as u32,
        extracted_at: row.get(5)?,
    })
}

/// Returns `true` if `content` starts with any common doc-comment prefix.
pub fn has_doc_prefix(content: &str) -> bool {
    let trimmed = content.trim_start();
    trimmed.starts_with("///")
        || trimmed.starts_with("//!")
        || trimmed.starts_with("/**")
        || trimmed.starts_with("/*!")
        || trimmed.starts_with("\"\"\"")
        || trimmed.starts_with("'''")
        || trimmed.starts_with("* ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_doc_prefix_rust_triple_slash() {
        assert!(has_doc_prefix("/// Does the thing"));
        assert!(has_doc_prefix("//! Crate-level doc"));
    }

    #[test]
    fn has_doc_prefix_jsdoc() {
        assert!(has_doc_prefix("/** JSDoc block */"));
        assert!(has_doc_prefix("/*!  JSDoc bang */"));
    }

    #[test]
    fn has_doc_prefix_python_docstring() {
        assert!(has_doc_prefix(r#""""Return the answer.""""#));
    }

    #[test]
    fn has_doc_prefix_negative() {
        assert!(!has_doc_prefix("// plain comment"));
        assert!(!has_doc_prefix("fn foo() {}"));
    }
}
