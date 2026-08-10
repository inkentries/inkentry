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
    /// Insert a note in a transaction that also sets `invalid_at` on the
    /// superseded entry.
    ///
    /// Returns `(id, created)`, see `MemoryStore::add_note` for what
    /// `created` means. ADR-068's fifth amendment (E1): this INSERT
    /// populates `entity_id` just like `add_note`/`add_note_with_created_at`,
    /// so it is subject to the same UNIQUE constraint and reuses the same
    /// `recover_from_entity_id_collision`. On a collision, the *existing*
    /// row's id is what the archive-`OLD` step below targets, not a fresh one.
    ///
    /// A collision can resolve to `supersedes_id` itself (a caller
    /// superseding `old_id` with content byte-identical to `old_id`'s own
    /// `{kind,title,body}`). Then there is nothing to archive and no edge to
    /// add, since the "new" entry already is that row, so both steps are
    /// skipped and the row is returned unchanged (`created = false`,
    /// tags/linked_files already merged by the recovery above): a self-loop
    /// of exactly the shape `dedupe.rs`'s own self-edge guard exists to
    /// prevent, reached via this path instead.
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
                 (uuid, kind, title, body, tags, linked_files, valid_at, created_at, entity_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    super::uuid_v7_at(created_at),
                    kind,
                    title,
                    body,
                    tags.join(","),
                    linked_files.join(","),
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
                // Self-collision guard: nothing to archive and no edge to add
                // when the collision resolved to the supersede target itself.
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
                // OLD is absent or already archived (e.g. a prior --supersedes
                // call already claimed it). Mirrors supersede()'s existing
                // reject-on-stale-OLD contract (ADR-068 E4): bail so the outer
                // match rolls the whole transaction back, so the just-inserted
                // new note is never committed and no carrier write happens.
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

    /// Archive `old_id` and link it to `new_id` as its replacement.
    /// Sets `invalid_at` to now if not already set.
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

    /// Insert a directed edge between two notes.
    /// `kind` must be one of: supersedes, relates_to, contradicts.
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

    /// Return all outgoing and incoming edges for a note.
    /// Returns `(outgoing, incoming)`.
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
