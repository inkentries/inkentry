//! Writing entries and edges that arrived from a portable dump.
//!
//! Separate from `notes.rs` because the rules differ: an authored entry is
//! minted here and now, and everything about it is this store's to decide. An
//! imported entry arrives with its own identity, creation time, provenance and
//! convergence key, and every one of those is carried verbatim — recomputing
//! any of them would silently fork the entry from the copy it came from.

use anyhow::{Context, Result};
use std::str::FromStr;

use super::{MemoryStore, NoteId};

impl MemoryStore {
    /// Insert an entry exactly as the dump described it, and return its id.
    ///
    /// The caller supplies `uuid`: carried verbatim when the dump had one,
    /// seeded from `created_at` when it did not. Deciding that here would put
    /// identity policy in the wrong place — it belongs to whatever read the
    /// dump, which is the only thing that knows whether one arrived.
    ///
    /// An entry whose `entity_id` collides with one already imported is the
    /// same entry twice: its tags and linked files are merged add-wins into
    /// the row already present, and that row's id is returned.
    #[allow(clippy::too_many_arguments)]
    pub fn import_entry(
        &self,
        uuid: &str,
        kind: &str,
        title: &str,
        body: &str,
        tags: &[String],
        linked_files: &[String],
        created_at: i64,
        status: &str,
        source_ref: Option<&str>,
        valid_at: Option<i64>,
        invalid_at: Option<i64>,
        entity_id: Option<&str>,
        remote_id: Option<&str>,
    ) -> Result<NoteId> {
        // Carried verbatim when present. Recomputing a key the writer already
        // holds would fork the entry from every other copy of it the moment
        // the two sides hash anything differently.
        let entity_id = entity_id
            .map(str::to_string)
            .unwrap_or_else(|| crate::storage::entity_id::entity_id(kind, title, body));

        let insert = self.conn.execute(
            "INSERT INTO notes \
             (uuid, kind, title, body, tags, linked_files, created_at, status, source_ref, \
              valid_at, invalid_at, entity_id, remote_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            rusqlite::params![
                uuid,
                kind,
                title,
                body,
                tags.join(","),
                linked_files.join(","),
                created_at,
                status,
                source_ref,
                valid_at,
                invalid_at,
                entity_id,
                remote_id,
            ],
        );

        let (id, _created) = self.recover_from_entity_id_collision(
            insert,
            &entity_id,
            &tags.iter().map(String::as_str).collect::<Vec<_>>(),
            &linked_files.iter().map(String::as_str).collect::<Vec<_>>(),
        )?;
        Ok(id)
    }

    /// Insert an edge from a dump, preserving its recorded timestamp.
    ///
    /// `INSERT OR IGNORE` because a dump may legitimately describe the same
    /// relationship twice — the reader deduplicates on `(type, from, to)`, and
    /// this is the second line of defence rather than the first.
    pub fn import_edge(
        &self,
        from_id: &NoteId,
        to_id: &NoteId,
        kind: &str,
        created_at: Option<i64>,
    ) -> Result<()> {
        match created_at {
            Some(at) => self.conn.execute(
                "INSERT OR IGNORE INTO memory_edges (from_id, to_id, kind, created_at) \
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![from_id.as_str(), to_id.as_str(), kind, at],
            ),
            None => self.conn.execute(
                "INSERT OR IGNORE INTO memory_edges (from_id, to_id, kind) VALUES (?1, ?2, ?3)",
                rusqlite::params![from_id.as_str(), to_id.as_str(), kind],
            ),
        }
        .with_context(|| format!("importing a {kind} relationship"))?;
        Ok(())
    }

    /// The id of the entry already holding `entity_id`, if any. Lets an import
    /// recognise an entry it has already seen without depending on the uuid,
    /// which a second dump of the same store may not have carried.
    pub fn note_id_for_entity_id(&self, entity_id: &str) -> Result<Option<NoteId>> {
        use rusqlite::OptionalExtension;
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT uuid FROM notes WHERE entity_id = ?1",
                rusqlite::params![entity_id],
                |r| r.get(0),
            )
            .optional()?;
        raw.map(|r| NoteId::from_str(&r).map_err(|e| anyhow::anyhow!(e)))
            .transpose()
    }
}
