use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::Serialize;
use std::path::{Path, PathBuf};

mod anchors;
mod dedupe;
mod edges;
pub mod events;
mod file_links;
mod import;
mod import_state;
mod migrate;
mod note_id;
mod notes;
mod search;
mod sync;
mod tags;
mod uuid_v7;

pub use anchors::PendingAnchor;
pub use dedupe::DedupeSummary;
pub use events::{EventFields, EventRow, record_event_at};
pub use file_links::{FileState, ResolvedFileLink, normalize_relative_path, resolve_file_link};
pub use import::CarriedEdgeImport;
pub use import_state::NotesImportMarker;
pub use note_id::{NoteId, unresolvable_id_message};
pub use sync::{SyncEdge, SyncRow};
pub use tags::normalize_tag;
pub use uuid_v7::uuid_v7_at;

#[cfg(test)]
mod schema_tests;
#[cfg(test)]
mod tests;

/// Version stamped into `PRAGMA user_version` by [`MemoryStore::open`].
///
/// A store stamped above [`LAST_LEGACY_SCHEMA_VERSION`] and below this is
/// migrated forward in place by the ladder in `migrate.rs`, one version at a
/// time, up to this constant. A fresh store takes the same road: it is created
/// from the frozen `memory_001_initial.sql` at [`INITIAL_SCHEMA_VERSION`] and
/// climbs the ladder from there, so there is exactly one way to reach the
/// current shape.
///
/// It continues the old ladder's numbering rather than restarting at 1, and
/// that is the whole point of [`LAST_LEGACY_SCHEMA_VERSION`]: `user_version`
/// is one i32 per file, shared with every stamp that ladder ever wrote, so a
/// fresh numbering would make an old product's store read as a *newer* one.
pub(super) const MEMORY_SCHEMA_VERSION: i32 = 14;

/// The highest `user_version` the pre-rename migration ladder ever stamped,
/// across every released binary (0.9.6 stamped 9; 0.9.7 and 0.9.8 stamped
/// 10).
///
/// A store carrying any stamp at or below this was written by an older
/// product and must be told to export and import — not migrated, since the
/// pre-11 ladder was removed at the spelunk-to-inkentry rename and nothing
/// migrates it forward. Nothing may reclaim this range: `MEMORY_SCHEMA_VERSION`
/// only ever moves up from here.
pub(super) const LAST_LEGACY_SCHEMA_VERSION: i32 = 10;

/// The version `memory_001_initial.sql` creates, and the one it is frozen at.
pub(super) const INITIAL_SCHEMA_VERSION: i32 = LAST_LEGACY_SCHEMA_VERSION + 1;

const _: () = assert!(
    MEMORY_SCHEMA_VERSION > LAST_LEGACY_SCHEMA_VERSION,
    "the memory schema version must stay above every stamp the old ladder wrote, or a store \
     from an older product is misread as one from a newer build"
);

pub struct MemoryStore {
    pub(super) conn: Connection,
    /// The directory linked-file paths (ADR-101 D3) are resolved against:
    /// the grandparent of `memory.db` (its parent is `.inkentry/`), so a
    /// path stored as `src/lib.rs` means `<project_root>/src/lib.rs`. Falls
    /// back to the process's current directory for a store with no real
    /// on-disk location (`:memory:`, used by tests) — the same fallback D3
    /// specifies for a project that is not a git repository, since neither
    /// case can be checked against anything more authoritative.
    project_root: PathBuf,
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
    /// The entry's portable identity: `sha256` over its kind, title and body
    /// (ADR-068), the same on every machine that holds the entry. `id` is this
    /// store's own token for it and is minted per machine (ADR-093 D1).
    pub entity_id: String,
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
    /// Who or what produced this entry (ADR-098 D6). `None` means no caller
    /// declared an actor when the entry was written — read as `unknown`,
    /// never as "known to be human". Not part of `entity_id`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<super::origin::Origin>,
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
        let project_root = path
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let store = Self { conn, project_root };
        store.create_schema()?;
        // WAL for the same reason `index.db` uses it (see `storage/db.rs`): in
        // the default rollback-journal mode every autocommit write is a journal
        // file create + fsync + delete, and the sync paths commit once per row
        // (`add_note`, `apply_remote_note`, `set_remote_id`). On NTFS that
        // per-row barrier measured ~0.26s, making sync cost scale with row
        // count rather than request count.
        //
        // Set *after* `create_schema`, never before: journal mode is persisted
        // in the file header, so setting it up front would convert a store this
        // build refuses — and refusing a store that does not fit the schema
        // without half-converting it is the whole point of `create_schema`.
        store
            .conn
            .execute_batch("PRAGMA journal_mode = WAL")
            .context("setting journal mode")?;
        Ok(store)
    }

    /// Create the memory schema on a new file, migrate one already stamped
    /// between [`LAST_LEGACY_SCHEMA_VERSION`] and [`MEMORY_SCHEMA_VERSION`], or
    /// accept one already at the current version.
    ///
    /// A fresh store is created from the frozen `memory_001_initial.sql` at
    /// [`INITIAL_SCHEMA_VERSION`] and then migrated like any other.
    /// Anything else is refused rather than half-covered with a shape its rows
    /// do not fit, unless it falls in the migratable range — and *which*
    /// refusal matters, because the two say opposite things. A store from an
    /// older product must be told to export and import; only a store from a
    /// genuinely newer build can be told to upgrade. The old ladder's stamps
    /// are what separate them, which is why this build's stamp continues that
    /// numbering instead of restarting. The ladder itself is
    /// `storage::migration_ladder`.
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
        if version > LAST_LEGACY_SCHEMA_VERSION {
            // This build's own history, not the old product's: migrate
            // forward rather than refuse.
            return super::migration_ladder::apply_ladder(
                &self.conn,
                version,
                MEMORY_SCHEMA_VERSION,
                migrate::MEMORY_MIGRATIONS,
                "memory.db",
            );
        }
        // At or below the old ladder's highest stamp: a store the old ladder
        // stamped, or one predating the stamp entirely and recognisable only
        // by holding tables.
        if version > 0 || !self.is_empty_file()? {
            let stamp = if version > 0 {
                format!(" (schema version {version})")
            } else {
                String::new()
            };
            anyhow::bail!(
                "this memory store was written by an older product{stamp} and cannot be \
                 opened in place. Export it with `spelunk-export`, then run \
                 `inkentry import` on the dump to bring it across.\n\
                 `spelunk-export` is a separate per-platform download from \
                 https://github.com/spelunk-cloud/spelunk/releases — it does not ship \
                 with inkentry."
            );
        }

        // Creation and its stamp commit together. Split across two
        // transactions, a crash between them would leave a fully-formed store
        // carrying no stamp — which the check above, correctly, refuses.
        // `user_version` is a header i32 and is transactional; the value here
        // is a code-controlled constant.
        self.conn
            .execute_batch(&format!(
                "BEGIN;\n{}\nPRAGMA user_version = {INITIAL_SCHEMA_VERSION};\nCOMMIT;",
                include_str!("../../../migrations/memory_001_initial.sql")
            ))
            .context("creating memory schema")?;
        super::migration_ladder::apply_ladder_quietly(
            &self.conn,
            INITIAL_SCHEMA_VERSION,
            MEMORY_SCHEMA_VERSION,
            migrate::MEMORY_MIGRATIONS,
            "memory.db",
        )
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
