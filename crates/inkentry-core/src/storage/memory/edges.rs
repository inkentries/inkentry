use anyhow::Result;
use std::str::FromStr;

use super::{MemoryEdge, MemoryStore, NoteId};

pub(super) fn row_to_edge(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryEdge> {
    let endpoint = |idx: usize| -> rusqlite::Result<NoteId> {
        let raw: String = row.get(idx)?;
        NoteId::from_str(&raw).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, e.into())
        })
    };
    Ok(MemoryEdge {
        from_id: endpoint(0)?,
        to_id: endpoint(1)?,
        kind: row.get(2)?,
        created_at: row.get(3)?,
    })
}

impl MemoryStore {
    /// Inserts a note that supersedes `supersedes_id`, archiving the old entry
    /// and linking the two, in one transaction.
    ///
    /// Returns `(id, created)`; see [`MemoryStore::add_note`] for what `created`
    /// means. When the content already exists, that entry is reused rather than
    /// inserted. If it is `supersedes_id` itself, nothing is archived or linked,
    /// so the entry is never made to supersede itself.
    ///
    /// Errors, without inserting anything, when `supersedes_id` is not an
    /// active entry.
    #[allow(clippy::too_many_arguments)]
    pub fn add_note_superseding(
        &self,
        kind: &str,
        title: &str,
        body: &str,
        tags: &[&str],
        linked_files: &[&str],
        valid_at: Option<i64>,
        supersedes_id: &NoteId,
    ) -> Result<(NoteId, bool)> {
        self.conn.execute_batch("BEGIN")?;
        let result = (|| -> Result<(NoteId, bool)> {
            let created_at = crate::storage::note_record::now_secs();
            let entity_id = crate::storage::entity_id::entity_id(kind, title, body);
            let insert_result = self.conn.execute(
                "INSERT INTO notes \
                 (uuid, kind, title, body, valid_at, created_at, entity_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    super::uuid_v7_at(created_at),
                    kind,
                    title,
                    body,
                    valid_at,
                    created_at,
                    entity_id,
                ],
            );
            let (id, created) = self.recover_from_entity_id_collision(
                insert_result,
                &entity_id,
                tags,
                linked_files,
            )?;
            if &id == supersedes_id {
                return Ok((id, created));
            }
            let changed = self.conn.execute(
                "UPDATE notes
                 SET    status = 'archived',
                        superseded_by = ?2,
                        invalid_at = CASE WHEN invalid_at IS NULL THEN unixepoch() ELSE invalid_at END
                 WHERE  uuid = ?1 AND status = 'active'",
                rusqlite::params![supersedes_id.as_str(), id.as_str()],
            )?;
            if changed == 0 {
                // Bailing rolls back the insert, so a stale target leaves no new note.
                anyhow::bail!("No active memory entry with id {supersedes_id} (old).");
            }
            self.conn.execute(
                "INSERT OR IGNORE INTO memory_edges (from_id, to_id, kind) VALUES (?1, ?2, 'supersedes')",
                rusqlite::params![id.as_str(), supersedes_id.as_str()],
            )?;
            Ok((id, created))
        })();
        match result {
            Ok(v) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(v)
            }
            Err(e) => {
                self.conn.execute_batch("ROLLBACK").ok();
                Err(e)
            }
        }
    }

    /// Archives `old_id` and links it to `new_id` as its replacement, setting
    /// `invalid_at` if it is unset.
    pub fn supersede(&self, old_id: &NoteId, new_id: &NoteId) -> Result<bool> {
        self.conn.execute_batch("BEGIN")?;
        let result = (|| -> Result<bool> {
            let changed = self.conn.execute(
                "UPDATE notes
                 SET    status = 'archived',
                        superseded_by = ?2,
                        invalid_at = CASE WHEN invalid_at IS NULL THEN unixepoch() ELSE invalid_at END
                 WHERE  uuid = ?1 AND status = 'active'",
                rusqlite::params![old_id.as_str(), new_id.as_str()],
            )?;
            if changed > 0 {
                self.conn.execute(
                    "INSERT OR IGNORE INTO memory_edges (from_id, to_id, kind) VALUES (?1, ?2, 'supersedes')",
                    rusqlite::params![new_id.as_str(), old_id.as_str()],
                )?;
            }
            Ok(changed > 0)
        })();
        match result {
            Ok(v) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Inserts a directed edge between two notes. `kind` must be `supersedes`,
    /// `relates_to` or `contradicts`.
    pub fn add_edge(&self, from_id: &NoteId, to_id: &NoteId, kind: &str) -> Result<()> {
        const VALID_KINDS: &[&str] = &["supersedes", "relates_to", "contradicts"];
        if !VALID_KINDS.contains(&kind) {
            anyhow::bail!(
                "invalid edge kind '{kind}'; must be one of: supersedes, relates_to, contradicts"
            );
        }
        self.conn.execute(
            "INSERT OR IGNORE INTO memory_edges (from_id, to_id, kind) VALUES (?1, ?2, ?3)",
            rusqlite::params![from_id.as_str(), to_id.as_str(), kind],
        )?;
        Ok(())
    }

    /// Every edge of `kind`, regardless of when it was created. Callers that
    /// need a time window filter on `created_at` themselves.
    pub fn edges_of_kind(&self, kind: &str) -> Result<Vec<MemoryEdge>> {
        let mut stmt = self.conn.prepare(
            "SELECT from_id, to_id, kind, created_at FROM memory_edges WHERE kind = ?1 ORDER BY created_at",
        )?;
        let edges = stmt
            .query_map(rusqlite::params![kind], row_to_edge)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(edges)
    }

    /// Returns `(outgoing, incoming)` edges for a note.
    pub fn get_edges(&self, id: &NoteId) -> Result<(Vec<MemoryEdge>, Vec<MemoryEdge>)> {
        let mut stmt = self.conn.prepare(
            "SELECT from_id, to_id, kind, created_at FROM memory_edges WHERE from_id = ?1 ORDER BY created_at",
        )?;
        let outgoing = stmt
            .query_map(rusqlite::params![id.as_str()], row_to_edge)?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut stmt2 = self.conn.prepare(
            "SELECT from_id, to_id, kind, created_at FROM memory_edges WHERE to_id = ?1 ORDER BY created_at",
        )?;
        let incoming = stmt2
            .query_map(rusqlite::params![id.as_str()], row_to_edge)?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok((outgoing, incoming))
    }
}
