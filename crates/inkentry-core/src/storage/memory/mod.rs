use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::Serialize;
use std::path::Path;

mod dedupe;
mod edges;
mod import;
mod import_state;
mod note_id;
mod notes;
mod search;
mod sync;
mod uuid_v7;

pub use dedupe::DedupeSummary;
pub use import_state::NotesImportMarker;
pub use note_id::{NoteId, unresolvable_id_message};
pub use sync::{SyncEdge, SyncRow};
pub use uuid_v7::uuid_v7_at;

#[cfg(test)]
mod schema_tests;
#[cfg(test)]
mod tests;

/// Version stamped into `PRAGMA user_version` by [`MemoryStore::open`].
///
/// There is no migration ladder. Every store this binary opens was created by
/// this binary at the shape `memory_001_initial.sql` declares; data from an
/// earlier product crosses via `inkentry import`, which imports into a store
/// created the same way (ADR-078). The constant survives so a store written by
/// a future build is refused rather than silently misread.
///
/// It continues the ladder's numbering rather than restarting at 1, and that is
/// the whole point of [`LAST_LEGACY_SCHEMA_VERSION`]: `user_version` is one i32
/// per file, shared with every stamp the ladder ever wrote, so a fresh
/// numbering would make an old product's store read as a *newer* one.
pub(super) const MEMORY_SCHEMA_VERSION: i32 = 11;

/// The highest `user_version` the migration ladder ever stamped, across every
/// released binary (0.9.6 stamped 9; 0.9.7 and 0.9.8 stamped 10).
///
/// A store carrying any stamp at or below this was written by an older product
/// and must be told to export and import — not to upgrade, which is advice that
/// can never work. Nothing may reclaim this range: `MEMORY_SCHEMA_VERSION` only
/// ever moves up from here.
pub(super) const LAST_LEGACY_SCHEMA_VERSION: i32 = 10;

const _: () = assert!(
    MEMORY_SCHEMA_VERSION > LAST_LEGACY_SCHEMA_VERSION,
    "the memory schema version must stay above every stamp the old ladder wrote, or a store \
     from an older product is misread as one from a newer build"
);

pub struct MemoryStore {
    pub(super) conn: Connection,
}

#[derive(Debug, Serialize)]
pub struct MemoryEdge {
    pub from_id: NoteId,
    pub to_id: NoteId,
    pub kind: String,
    pub created_at: i64,
}

#[derive(Debug, Serialize)]
pub struct Note {
    pub id: NoteId,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    pub linked_files: Vec<String>,
    pub created_at: i64,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<NoteId>,
    /// Git commit SHA for harvested entries; NULL for manually created entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    /// When this entry became valid (unix epoch). None = treat as created_at.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_at: Option<i64>,
    /// When this entry was invalidated/superseded (unix epoch). None = still valid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invalid_at: Option<i64>,
    /// Semantic distance — only populated by search(), None otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance: Option<f64>,
    /// Fused relevance score — only populated by hybrid search, None otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    /// Set only for notes returned via cross-project dep pass. None for local notes.
    /// Contains the dep project's display name (final path component of root_path).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_project: Option<String>,
    /// Set alongside source_project: the dep project's root path, for disambiguation
    /// when two linked projects share a display name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_project_path: Option<String>,
    /// Canonical cross-machine id (uuid) when synced to a remote; None for
    /// never-synced local rows. Carried from the remote wire (ADR-059 D2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_id: Option<String>,
}

impl MemoryStore {
    /// Execute a raw SQL batch statement on the connection.
    ///
    /// Exposed for transaction management in callers that need BEGIN/COMMIT/ROLLBACK
    /// without access to the private `conn` field (e.g. `memory reconcile`).
    pub fn execute_batch(&self, sql: &str) -> rusqlite::Result<()> {
        self.conn.execute_batch(sql)
    }

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating directory {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening memory DB at {}", path.display()))?;
        // Enforcement is declared here rather than inherited from whichever
        // SQLite the workspace links against: the bundled build happens to
        // compile foreign keys on by default, and a data-integrity guarantee
        // resting on a vendored dependency's compile flag disappears silently
        // the day someone builds against a system SQLite. `PRAGMA foreign_keys`
        // is per-connection and cannot live in the schema file, so it runs on
        // every open.
        conn.execute_batch("PRAGMA foreign_keys = ON")
            .context("enabling foreign-key enforcement")?;
        super::apply_test_page_cap(&conn)?;
        let store = Self { conn };
        store.create_schema()?;
        Ok(store)
    }

    /// Create the memory schema on a new file, or accept one already stamped
    /// with it.
    ///
    /// There is no ladder: `memory_001_initial.sql` declares the final shape.
    /// Anything else is refused rather than half-covered with a shape its rows
    /// do not fit — and *which* refusal matters, because the two say opposite
    /// things. A store from an older product must be told to export and import;
    /// only a store from a genuinely newer build can be told to upgrade. The
    /// old ladder's stamps are what separate them, which is why this build's
    /// stamp continues that numbering instead of restarting.
    fn create_schema(&self) -> Result<()> {
        let version: i32 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .context("reading user_version")?;

        if version == MEMORY_SCHEMA_VERSION {
            return Ok(());
        }
        if version > MEMORY_SCHEMA_VERSION {
            anyhow::bail!(
                "memory.db schema version {version} is newer than this build of inkentry \
                 supports (max {MEMORY_SCHEMA_VERSION}); upgrade inkentry to open this store."
            );
        }
        // Below this build's stamp: a store the old ladder stamped, or one
        // predating the stamp entirely and recognisable only by holding tables.
        if version > 0 || !self.is_empty_file()? {
            let stamp = if version > 0 {
                format!(" (schema version {version})")
            } else {
                String::new()
            };
            anyhow::bail!(
                "this memory store was written by an older product{stamp} and cannot be \
                 opened in place. Export it with the migration tool, then run \
                 `inkentry import` to bring it across."
            );
        }

        // Creation and its stamp commit together. Split across two
        // transactions, a crash between them would leave a fully-formed store
        // carrying no stamp — which the check above, correctly, refuses.
        // `user_version` is a header i32 and is transactional; the value here
        // is a code-controlled constant.
        self.conn
            .execute_batch(&format!(
                "BEGIN;\n{}\nPRAGMA user_version = {MEMORY_SCHEMA_VERSION};\nCOMMIT;",
                include_str!("../../../migrations/memory_001_initial.sql")
            ))
            .context("creating memory schema")?;
        Ok(())
    }

    /// True when the file has no user tables.
    fn is_empty_file(&self) -> Result<bool> {
        let n: i64 = self
            .conn
            .query_row(
                "SELECT count(*) FROM sqlite_master \
                 WHERE type='table' AND name NOT LIKE 'sqlite_%'",
                [],
                |r| r.get(0),
            )
            .context("counting user tables")?;
        Ok(n == 0)
    }

    /// The storage surrogate for an exported identity, or `None` when no such
    /// entry exists. Private: the integer never leaves this module.
    pub(super) fn rowid_for(&self, id: &NoteId) -> Result<Option<i64>> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM notes WHERE uuid = ?1",
                rusqlite::params![id.as_str()],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// The exported identity for a storage surrogate.
    pub(super) fn uuid_for_rowid(&self, rowid: i64) -> Result<Option<NoteId>> {
        use rusqlite::OptionalExtension;
        use std::str::FromStr;
        let uuid: Option<String> = self
            .conn
            .query_row(
                "SELECT uuid FROM notes WHERE id = ?1",
                rusqlite::params![rowid],
                |r| r.get(0),
            )
            .optional()?;
        uuid.map(|u| NoteId::from_str(&u).map_err(|e| anyhow::anyhow!(e)))
            .transpose()
    }
}
