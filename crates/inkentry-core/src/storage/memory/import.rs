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
use crate::storage::note_record::CarriedEdge;

/// What applying a batch of carried edges did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CarriedEdgeImport {
    /// Edges that became a new `memory_edges` row.
    pub applied: usize,
    /// Edges already present, so nothing was added for them.
    pub already_present: usize,
    /// Edges whose source or target has no row in this store. Skipped, not
    /// fatal: a partial fetch, an entry kept off the carrier, or an entry
    /// deleted on the writing machine all produce this legitimately, and a
    /// later import after a fuller fetch resolves them.
    pub unresolved: usize,
}

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

    /// Insert an edge from a dump, preserving its recorded timestamp. Returns
    /// whether a row was actually added, which is how a caller tells an edge it
    /// imported from one that was already here.
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
    ) -> Result<bool> {
        let inserted = match created_at {
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
        Ok(inserted > 0)
    }

    /// Apply edges that arrived as `(source entity_id, edge)` pairs, the shape
    /// the git-notes carrier records them in. Both endpoints resolve through
    /// `entity_id`, so the same edge lands on the same row on every machine.
    ///
    /// Must run after every entry it could reference is in the store: the
    /// foreign keys refuse an edge whose endpoints are absent, so an edge is
    /// only ever applied once both rows exist and is otherwise counted as
    /// unresolved. Idempotent through the primary key, so re-applying the
    /// whole carrier after a fetch adds nothing that is already here.
    ///
    /// Only the two kinds the carrier defines are applied. Any other kind is a
    /// record from a build this one does not know and is passed over, the way
    /// an unknown key on the record itself is.
    pub fn import_carried_edges(
        &self,
        edges: &[(String, CarriedEdge)],
    ) -> Result<CarriedEdgeImport> {
        let mut outcome = CarriedEdgeImport::default();
        for (from_entity_id, edge) in edges {
            if !matches!(edge.kind.as_str(), "relates_to" | "contradicts") {
                continue;
            }
            let (Some(from), Some(to)) = (
                self.note_id_for_entity_id(from_entity_id)?,
                self.note_id_for_entity_id(&edge.to_entity_id)?,
            ) else {
                outcome.unresolved += 1;
                continue;
            };
            if self.import_edge(&from, &to, &edge.kind, None)? {
                outcome.applied += 1;
            } else {
                outcome.already_present += 1;
            }
        }
        Ok(outcome)
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

    /// Every entry whose `entity_id` starts with `prefix`, so a caller can tell
    /// a miss from a hit from an ambiguous handle (ADR-093 D2).
    ///
    /// A range over the unique index rather than a `LIKE`, which SQLite only
    /// turns into a range under the right collation and pragma. `'g'` is the
    /// upper bound because the column is lowercase hex, so it sorts after every
    /// value the prefix can extend to and before the next prefix.
    pub fn note_ids_for_entity_id_prefix(&self, prefix: &str) -> Result<Vec<NoteId>> {
        let mut stmt = self.conn.prepare(
            "SELECT uuid FROM notes WHERE entity_id >= ?1 AND entity_id < ?1 || 'g' \
             ORDER BY entity_id",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![prefix], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|r| NoteId::from_str(&r).map_err(|e| anyhow::anyhow!(e)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

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

    fn add(store: &MemoryStore, title: &str) -> (NoteId, String) {
        let (id, _) = store
            .add_note("decision", title, "body", &[], &[], None, None)
            .expect("add");
        (
            id,
            crate::storage::entity_id::entity_id("decision", title, "body"),
        )
    }

    fn edge_rows(store: &MemoryStore) -> Vec<(String, String, String)> {
        let mut stmt = store
            .conn
            .prepare("SELECT from_id, to_id, kind FROM memory_edges ORDER BY from_id, to_id, kind")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// The whole contract in one pass: a resolvable edge of either kind lands,
    /// one with a missing endpoint is counted rather than failing the batch,
    /// and applying the same batch again changes nothing.
    #[test]
    fn import_carried_edges_applies_resolvable_edges_counts_dangling_and_is_idempotent() {
        register_sqlite_vec();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let store = MemoryStore::open(tmp.path()).expect("open");
        let (a, ea) = add(&store, "a");
        let (b, eb) = add(&store, "b");

        let edges = vec![
            (ea.clone(), CarriedEdge::new("relates_to", eb.clone())),
            (eb.clone(), CarriedEdge::new("contradicts", ea.clone())),
            (
                ea.clone(),
                CarriedEdge::new("relates_to", "not-here".to_string()),
            ),
            (
                "not-here".to_string(),
                CarriedEdge::new("relates_to", ea.clone()),
            ),
        ];
        let first = store.import_carried_edges(&edges).expect("apply");
        assert_eq!(
            first,
            CarriedEdgeImport {
                applied: 2,
                already_present: 0,
                unresolved: 2,
            }
        );
        assert_eq!(edge_rows(&store), {
            let mut rows = vec![
                (a.to_string(), b.to_string(), "relates_to".to_string()),
                (b.to_string(), a.to_string(), "contradicts".to_string()),
            ];
            rows.sort();
            rows
        });

        let again = store.import_carried_edges(&edges).expect("re-apply");
        assert_eq!(
            again,
            CarriedEdgeImport {
                applied: 0,
                already_present: 2,
                unresolved: 2,
            }
        );
        assert_eq!(edge_rows(&store).len(), 2, "re-applying adds no rows");

        // The dangling target arrives; the next pass resolves it.
        let (_c, ec) = add(&store, "not-here-yet");
        let late = vec![(ea, CarriedEdge::new("relates_to", ec))];
        assert_eq!(store.import_carried_edges(&late).expect("late").applied, 1);
    }

    /// The prefix read is what tells a quoted handle from an ambiguous one, so
    /// it is pinned against crafted values: two that share the queried prefix,
    /// one that shares only part of it, and one that shares none. Crafted
    /// rather than hashed because entries sharing eight hex characters is a
    /// 32-bit coincidence no fixture can produce from content.
    #[test]
    fn the_prefix_read_returns_every_candidate_and_stops_at_the_range_bound() {
        register_sqlite_vec();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let store = MemoryStore::open(tmp.path()).expect("open");
        let (first, _) = add(&store, "first");
        let (second, _) = add(&store, "second");
        let (neighbour, _) = add(&store, "neighbour");
        let (elsewhere, _) = add(&store, "elsewhere");
        for (id, entity_id) in [
            (&first, "abcd1234aaaa"),
            (&second, "abcd1234bbbb"),
            (&neighbour, "abcd1235cccc"),
            (&elsewhere, "00000000dddd"),
        ] {
            store
                .conn
                .execute(
                    "UPDATE notes SET entity_id = ?1 WHERE uuid = ?2",
                    rusqlite::params![entity_id, id.as_str()],
                )
                .expect("craft entity_id");
        }

        assert_eq!(
            store.note_ids_for_entity_id_prefix("abcd1234").unwrap(),
            vec![first.clone(), second],
            "both entries under the prefix, and nothing from the next one"
        );
        assert_eq!(
            store.note_ids_for_entity_id_prefix("abcd1234aaaa").unwrap(),
            vec![first],
            "a longer prefix separates them"
        );
        assert_eq!(
            store
                .note_ids_for_entity_id_prefix("abcd123")
                .unwrap()
                .len(),
            3,
            "a shorter prefix reaches the neighbouring value too"
        );
        assert!(
            store
                .note_ids_for_entity_id_prefix("ffffffff")
                .unwrap()
                .is_empty()
        );
    }

    /// `supersedes` has its own carrier field; a record carrying it in the edge
    /// list is not something this build writes, so it is passed over rather
    /// than turned into a second, unreconciled path to the same row.
    #[test]
    fn import_carried_edges_passes_over_kinds_it_does_not_define() {
        register_sqlite_vec();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let store = MemoryStore::open(tmp.path()).expect("open");
        let (_a, ea) = add(&store, "a");
        let (_b, eb) = add(&store, "b");

        let edges = vec![
            (ea.clone(), CarriedEdge::new("supersedes", eb.clone())),
            (ea, CarriedEdge::new("depends_on", eb)),
        ];
        let outcome = store.import_carried_edges(&edges).expect("apply");
        assert_eq!(outcome, CarriedEdgeImport::default());
        assert!(edge_rows(&store).is_empty());
    }
}
