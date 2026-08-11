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
    /// Returns `(id, created)`. `created` is `false` when the entry was already
    /// in this store under the same convergence key — a second import of the
    /// same dump, or of an overlapping one — in which case its tags and linked
    /// files are merged add-wins into the row already present and that row's id
    /// is returned. The caller reports the two cases apart, because "imported"
    /// and "was already here" are different answers to the question a user asks
    /// after a one-way move.
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
    ) -> Result<(NoteId, bool)> {
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

        // A UNIQUE violation here is not necessarily the convergence key: this
        // entry also carries a `uuid` and possibly a `remote_id`, and either
        // can collide with a row already in the store. Those are not the same
        // entry arriving twice — they are two entries claiming one identity —
        // so they are named rather than handed to the collision recovery,
        // which would look for a convergence key that is not there and let
        // SQLite's own "Query returned no rows" reach the user instead.
        if let Err(rusqlite::Error::SqliteFailure(err, _)) = &insert
            && err.code == rusqlite::ErrorCode::ConstraintViolation
            && err.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
            && self.note_id_for_entity_id(&entity_id)?.is_none()
        {
            return Err(self.identity_already_taken(uuid, remote_id));
        }

        self.recover_from_entity_id_collision(
            insert,
            &entity_id,
            &tags.iter().map(String::as_str).collect::<Vec<_>>(),
            &linked_files.iter().map(String::as_str).collect::<Vec<_>>(),
        )
    }

    /// Which identity this store already holds, said in the dump's terms.
    fn identity_already_taken(&self, uuid: &str, remote_id: Option<&str>) -> anyhow::Error {
        let taken = |column: &str, value: &str| -> bool {
            self.conn
                .query_row(
                    &format!("SELECT 1 FROM notes WHERE {column} = ?1"),
                    rusqlite::params![value],
                    |_| Ok(()),
                )
                .is_ok()
        };
        if taken("uuid", uuid) {
            return anyhow::anyhow!(
                "this store already holds a different entry under the identity {uuid:?}. \
                 Refusing to import any of it."
            );
        }
        if let Some(remote) = remote_id
            && taken("remote_id", remote)
        {
            return anyhow::anyhow!(
                "this store already holds a different entry under the remote id {remote:?}. \
                 Refusing to import any of it."
            );
        }
        anyhow::anyhow!(
            "the entry collides with one already in this store on an identity the dump \
             declares. Refusing to import any of it."
        )
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
