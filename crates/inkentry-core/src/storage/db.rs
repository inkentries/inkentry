use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use std::path::{Path, PathBuf};

use super::index_migrate::{INDEX_MIGRATIONS, IndexMigrationKind};
use super::migration_ladder::MigrationStep;

/// Wraps the SQLite connection and provides typed access to the schema.
/// Methods are implemented across sub-modules in the `storage` package.
pub struct Database {
    pub(super) conn: Connection,
    rebuilt_from: Option<i32>,
}

/// Version stamped into `PRAGMA user_version` by [`Database::open`].
///
/// Every index this binary opens reaches this version by the registry in
/// `storage::index_migrate`: a fresh index is created from the frozen
/// `index_001_initial.sql` at [`INITIAL_SCHEMA_VERSION`] and climbs from
/// there, and a store stamped above [`LAST_LEGACY_SCHEMA_VERSION`] and below
/// this migrates forward in place the same way, one version at a time, unless
/// a registered version in that range asks to rebuild instead
/// (`storage::index_migrate::IndexMigrationKind::Rebuild`) — for a change,
/// like a different embedding space, that an in-place step cannot fix.
/// Anything else — at or below [`LAST_LEGACY_SCHEMA_VERSION`], or
/// from a build newer than this one — is discarded and rebuilt or refused,
/// because an index is derived from the user's source tree and reindexing
/// reproduces it exactly.
///
/// It continues the old ladder's numbering rather than restarting at 1, for the
/// reason [`LAST_LEGACY_SCHEMA_VERSION`] records.
pub(super) const CURRENT_SCHEMA_VERSION: i32 = 20;

/// The highest `user_version` the old migration ladder ever stamped.
///
/// `user_version` is one i32 per file, shared with every stamp the ladder wrote,
/// so a fresh numbering starting at 1 would make an index from an older build
/// read as one from a *newer* build — and be refused with advice to upgrade to
/// something that does not exist. Nothing may reclaim this range:
/// `CURRENT_SCHEMA_VERSION` only ever moves up from here.
pub(super) const LAST_LEGACY_SCHEMA_VERSION: i32 = 16;

/// The version `index_001_initial.sql` creates, and the one it is frozen at.
/// The ladder below it was collapsed into that file once, at the 1.0 rename;
/// every later shape is a numbered step, and the file is never edited again.
pub(super) const INITIAL_SCHEMA_VERSION: i32 = LAST_LEGACY_SCHEMA_VERSION + 1;

const _: () = assert!(
    CURRENT_SCHEMA_VERSION > LAST_LEGACY_SCHEMA_VERSION,
    "the index schema version must stay above every stamp the old ladder wrote, or an index \
     from an older build is misread as one from a newer build"
);

/// `index_meta` key recording that the file the caller is holding is one
/// [`Database::rebuild`] emptied, and which version it replaced.
///
/// The in-memory [`Database::rebuilt_from`] only reaches the run that did the
/// rebuild; every run after it opens a file that is merely empty. Without a
/// durable record, "emptied by a rebuild" and "never indexed" are the same
/// store, which is what made a rebuilt index read as an empty repository.
///
/// [`Database::mark_reindexed`] removes it, so the marker means "not
/// repopulated since", not "was rebuilt once".
const REBUILT_FROM_KEY: &str = "rebuilt_from_version";

/// [`Database::pass_owed`] key for "every file's graph edges must be
/// re-extracted", set by a migration step that changes what edge extraction
/// stores. Re-extraction touches `graph_edges` only: no chunking, no
/// embedding.
pub const GRAPH_EDGES_REEXTRACT: &str = "graph_edges_reextract";

impl Database {
    /// Open (or create) the database at `path` and run all migrations.
    ///
    /// Assumes `sqlite3_auto_extension` has already been called in `main` to
    /// load the sqlite-vec extension into every new connection.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating db directory {}", parent.display()))?;
        }

        let conn = Connection::open(path)
            .with_context(|| format!("opening database at {}", path.display()))?;
        Self::apply_connection_pragmas(&conn)?;

        let mut db = Self {
            conn,
            rebuilt_from: None,
        };
        db.create_schema(path)?;
        Ok(db)
    }

    /// The schema version this open discarded, on the one run that discarded
    /// it, or `None` on an open that changed nothing. A caller with a user in
    /// front of it says so here; the `tracing::warn!` [`rebuild`](Self::rebuild)
    /// also emits is below the CLI's default filter and reaches nobody.
    pub fn rebuilt_from(&self) -> Option<i32> {
        self.rebuilt_from
    }

    /// The version a rebuild replaced, while the index it left behind still
    /// holds nothing a reindex would put back. `None` on an index no rebuild
    /// touched, and once [`mark_reindexed`](Self::mark_reindexed) has run.
    ///
    /// This is what separates an emptied index from one that was never built,
    /// on every run after the rebuild rather than only on the rebuild itself.
    pub fn unpopulated_since_rebuild(&self) -> Result<Option<i32>> {
        let recorded: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM index_meta WHERE key = ?1",
                rusqlite::params![REBUILT_FROM_KEY],
                |row| row.get(0),
            )
            .optional()
            .context("reading the index rebuild marker")?;
        Ok(recorded.and_then(|v| v.parse().ok()))
    }

    /// Drop the rebuild marker: an index run has been through this store, so
    /// whatever it holds now is what the source tree produced.
    pub fn mark_reindexed(&self) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM index_meta WHERE key = ?1",
                rusqlite::params![REBUILT_FROM_KEY],
            )
            .context("clearing the index rebuild marker")?;
        Ok(())
    }

    /// Record that some derived-data refresh pass still owes work under
    /// `key`, without saying what: naming the reason and running the pass are
    /// the caller's job. A migration step whose new rows a real indexing pass
    /// must backfill — rather than a rebuild, which would throw away
    /// embeddings a re-extraction doesn't need to touch — sets this instead.
    pub fn mark_pass_owed(&self, key: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO index_meta (key, value) VALUES (?1, '1')",
                rusqlite::params![key],
            )
            .with_context(|| format!("recording that {key} owes a pass"))?;
        Ok(())
    }

    /// Whether [`mark_pass_owed`](Self::mark_pass_owed) was called for `key`
    /// and not yet cleared.
    pub fn pass_owed(&self, key: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM index_meta WHERE key = ?1",
                rusqlite::params![key],
                |_| Ok(()),
            )
            .optional()
            .with_context(|| format!("reading whether {key} owes a pass"))?
            .is_some())
    }

    /// Clear the marker [`mark_pass_owed`](Self::mark_pass_owed) set for
    /// `key`: the pass ran, so nothing is owed until the next step that sets
    /// it again.
    pub fn clear_pass_owed(&self, key: &str) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM index_meta WHERE key = ?1",
                rusqlite::params![key],
            )
            .with_context(|| format!("clearing the {key} pass marker"))?;
        Ok(())
    }

    /// Per-connection settings, applied on every connection this type opens —
    /// including the second one a rebuild makes, which would otherwise come up
    /// without foreign-key enforcement or the WAL.
    fn apply_connection_pragmas(conn: &Connection) -> Result<()> {
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        super::apply_test_page_cap(conn)?;
        Ok(())
    }

    /// Create the index schema on a new file, accept one already at it,
    /// migrate one stamped between [`LAST_LEGACY_SCHEMA_VERSION`] and this
    /// build's own version forward in place, and rebuild anything older.
    ///
    /// Below [`LAST_LEGACY_SCHEMA_VERSION`] there is still no ladder: an index
    /// is derived from the user's source tree, so the answer to a shape this
    /// old is to reindex, not to convert. That is the same reasoning ADR-078
    /// applies to `memory.db`, reaching the opposite action for versions below
    /// its own floor because the two stores hold different things — memory is
    /// authored and refuses rather than rebuild, an index is not.
    ///
    /// `usage` is the exception that stops "purely derived" being true, and it
    /// is carried across every rebuild, whichever version triggered it.
    fn create_schema(&mut self, path: &Path) -> Result<()> {
        let version: i32 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .context("reading user_version")?;

        if version == CURRENT_SCHEMA_VERSION {
            return Ok(());
        }
        if version > CURRENT_SCHEMA_VERSION {
            anyhow::bail!(
                "index.db schema version {version} is newer than this build of inkentry \
                 supports (max {CURRENT_SCHEMA_VERSION}); upgrade inkentry to open this index."
            );
        }
        if version > LAST_LEGACY_SCHEMA_VERSION {
            // This build's own history, not the pre-ladder shapes below:
            // migrate forward rather than rebuild, unless the registry itself
            // says this range must rebuild.
            return self.migrate_or_rebuild(
                path,
                version,
                CURRENT_SCHEMA_VERSION,
                INDEX_MIGRATIONS,
            );
        }
        // At or below the old ladder's highest stamp: an index written by an
        // older ladder, or one predating the stamp entirely and recognisable
        // only by holding tables.
        if version > 0 || !self.is_empty_file()? {
            return self.rebuild(path, version);
        }

        self.create_fresh()
    }

    /// Migrate `found` forward to `target` through `registry`'s `Migrate`
    /// steps, unless a registered version in `(found, target]` is a
    /// [`IndexMigrationKind::Rebuild`] — in which case this rebuilds once,
    /// carrying `usage` across, instead of running any step in that range.
    ///
    /// Split out of [`create_schema`](Self::create_schema) so a test can drive
    /// the resolution logic against a synthetic registry and target without
    /// the production [`CURRENT_SCHEMA_VERSION`] and [`INDEX_MIGRATIONS`]
    /// having to move to exercise it.
    fn migrate_or_rebuild(
        &mut self,
        path: &Path,
        found: i32,
        target: i32,
        registry: &[(i32, IndexMigrationKind)],
    ) -> Result<()> {
        let must_rebuild = registry.iter().any(|&(v, kind)| {
            v > found && v <= target && matches!(kind, IndexMigrationKind::Rebuild)
        });
        if must_rebuild {
            return self.rebuild(path, found);
        }
        let steps: Vec<(i32, MigrationStep)> = registry
            .iter()
            .filter_map(|&(v, kind)| match kind {
                IndexMigrationKind::Migrate(step) => Some((v, step)),
                IndexMigrationKind::Rebuild => None,
            })
            .collect();
        super::migration_ladder::apply_ladder(&self.conn, found, target, &steps, "index.db")
    }

    /// Replace an index this build did not write, carrying `usage` across.
    ///
    /// The file is removed and recreated rather than having its tables
    /// dropped. Two of them are virtual — an FTS5 index and a vec0 table — and
    /// each owns a set of shadow tables that must not be dropped directly and
    /// whose names have changed across the shapes this might encounter. Taking
    /// the file out removes the need to know any of them.
    fn rebuild(&mut self, path: &Path, found: i32) -> Result<()> {
        let carried = self.read_usage().unwrap_or_default();

        tracing::warn!(
            found_version = found,
            carried_usage_rows = carried.len(),
            "index.db was written by an older schema and cannot be read by this build; \
             rebuilding it empty. Run `inkentry index` to repopulate it."
        );

        // Close this connection before the file goes: an open handle to a
        // deleted file keeps writing to an inode nothing can find again.
        self.conn = Connection::open_in_memory().context("parking the connection")?;
        for suffix in ["", "-wal", "-shm"] {
            let victim = PathBuf::from(format!("{}{suffix}", path.display()));
            match std::fs::remove_file(&victim) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(e).with_context(|| format!("removing {}", victim.display()));
                }
            }
        }

        self.conn = Connection::open(path)
            .with_context(|| format!("reopening database at {}", path.display()))?;
        Self::apply_connection_pragmas(&self.conn)?;
        self.create_fresh()?;
        self.write_usage(&carried)?;
        self.conn
            .execute(
                "INSERT OR REPLACE INTO index_meta (key, value) VALUES (?1, ?2)",
                rusqlite::params![REBUILT_FROM_KEY, found.to_string()],
            )
            .context("recording the index rebuild marker")?;
        self.rebuilt_from = Some(found);
        Ok(())
    }

    fn create_fresh(&self) -> Result<()> {
        // Creation and its stamp commit together. Split across two
        // transactions, a crash between them would leave a fully-formed index
        // carrying no stamp — which the check above would then rebuild,
        // discarding a perfectly good index.
        self.conn
            .execute_batch(&format!(
                "BEGIN;\n{}\nPRAGMA user_version = {INITIAL_SCHEMA_VERSION};\nCOMMIT;",
                include_str!("../../migrations/index_001_initial.sql")
            ))
            .context("creating index schema")?;

        // The same road an existing store takes, so there is one way to reach
        // the current shape. A `Rebuild` entry is meaningless for an index
        // that holds nothing yet, so only the in-place steps run.
        let steps: Vec<(i32, MigrationStep)> = INDEX_MIGRATIONS
            .iter()
            .filter_map(|&(v, kind)| match kind {
                IndexMigrationKind::Migrate(step) => Some((v, step)),
                IndexMigrationKind::Rebuild => None,
            })
            .collect();
        super::migration_ladder::apply_ladder_quietly(
            &self.conn,
            INITIAL_SCHEMA_VERSION,
            CURRENT_SCHEMA_VERSION,
            &steps,
            "index.db",
        )?;

        // The scheme an empty index composes its embedding input under. Stamped
        // at creation, not on first index, because `ensure_*` readers treat an
        // absent stamp as "written by something older" and would recompose a
        // store that has nothing in it yet.
        self.conn
            .execute(
                "INSERT INTO index_meta (key, value) VALUES ('summary_scheme', ?1)",
                rusqlite::params![crate::indexer::summariser::SUMMARY_SCHEME],
            )
            .context("stamping the summary scheme")?;
        Ok(())
    }

    /// Best effort by design: an index old enough to predate the `usage` table
    /// has nothing to carry, and failing the whole open over telemetry would
    /// be the wrong trade.
    fn read_usage(&self) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare("SELECT command, called_at FROM usage")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn write_usage(&self, rows: &[(String, i64)]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare("INSERT INTO usage (command, called_at) VALUES (?1, ?2)")?;
            for (command, called_at) in rows {
                stmt.execute(rusqlite::params![command, called_at])?;
            }
        }
        tx.commit().context("carrying usage across the rebuild")?;
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

    /// Read the recorded embedding-input composition scheme
    /// (`summariser::SUMMARY_SCHEME`), or `None` if never stamped.
    pub fn summary_scheme(&self) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT value FROM index_meta WHERE key = 'summary_scheme'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("reading summary_scheme")
    }

    /// Read the recorded embedding model id, or `None` if never stamped (a DB
    /// predating provenance, treated as "matches anything" until first write).
    pub fn embedding_model(&self) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT value FROM index_meta WHERE key = 'embedding_model'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("reading embedding_model")
    }

    /// Assert the current model matches this DB's provenance before writing
    /// embeddings, stamping it on a fresh/legacy DB. A recorded id that differs
    /// is a hard error: mixing two model ids in one KNN space is corruption.
    pub fn ensure_embedding_model(&self, model_id: &str) -> Result<()> {
        match self.embedding_model()? {
            Some(recorded) if recorded == model_id => Ok(()),
            Some(recorded) => anyhow::bail!(
                "index was built with embedding model '{recorded}' but this build uses \
                 '{model_id}'. Vectors from two models must not share one search index. \
                 Re-index from scratch: `inkentry index . --force` (or delete .inkentry/index.db)."
            ),
            None => {
                self.conn
                    .execute(
                        "INSERT OR REPLACE INTO index_meta (key, value) \
                         VALUES ('embedding_model', ?1), ('embedding_dim', ?2)",
                        rusqlite::params![model_id, crate::embeddings::EMBEDDING_DIM.to_string()],
                    )
                    .context("stamping embedding provenance")?;
                Ok(())
            }
        }
    }

    /// Read the recorded chunker config id (`chunker::chunker_config_id`), or
    /// `None` if never stamped (a DB predating this provenance key).
    pub fn chunker_config(&self) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT value FROM index_meta WHERE key = 'chunker_config'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("reading chunker_config")
    }

    /// Compare the running build's chunker config against this DB's
    /// provenance, stamping it on a fresh/legacy DB. Unlike
    /// [`ensure_embedding_model`](Self::ensure_embedding_model), a mismatch
    /// here is same-model/same-dimension drift (e.g. a changed chunk-token
    /// cap), not a hard incompatibility: old and new chunks coexist in the
    /// same vector space at different granularity, so this never errors.
    /// Returns the stale recorded value on a mismatch so the caller can warn
    /// without failing the run; the stamp is left as-is (not overwritten) so
    /// the warning keeps firing until a `--force` run re-chunks everything
    /// under the current config and calls
    /// [`stamp_chunker_config`](Self::stamp_chunker_config) to refresh it.
    pub fn ensure_chunker_config(&self, config: &str) -> Result<Option<String>> {
        match self.chunker_config()? {
            Some(recorded) if recorded == config => Ok(None),
            Some(recorded) => Ok(Some(recorded)),
            None => {
                self.conn
                    .execute(
                        "INSERT OR REPLACE INTO index_meta (key, value) VALUES ('chunker_config', ?1)",
                        rusqlite::params![config],
                    )
                    .context("stamping chunker config provenance")?;
                Ok(None)
            }
        }
    }

    /// Unconditionally record `config` as this DB's chunker-config provenance,
    /// regardless of what (if anything) was previously stamped. A `--force`
    /// re-index re-chunks every file, so once it finishes every stored chunk
    /// was cut under `config`; refreshing the stamp here is what makes
    /// [`ensure_chunker_config`](Self::ensure_chunker_config) stop warning on
    /// the next normal run, until the config next changes.
    pub fn stamp_chunker_config(&self, config: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO index_meta (key, value) VALUES ('chunker_config', ?1)",
                rusqlite::params![config],
            )
            .context("stamping chunker config provenance")?;
        Ok(())
    }

    /// Insert or replace an embedding for a chunk.
    ///
    /// Takes the raw float vector; it is int8-quantised here for the
    /// `embeddings` `int8[896]` column (see `embeddings::vec_to_int8_blob`).
    pub fn insert_embedding(&self, chunk_id: i64, vector: &[f32]) -> Result<()> {
        let blob = crate::embeddings::vec_to_int8_blob(vector);
        // The `embeddings` table is a sqlite-vec `vec0` virtual table, which does
        // not honour `INSERT OR REPLACE`/`ON CONFLICT`: a second insert for an
        // existing `chunk_id` raises a hard UNIQUE-constraint error instead of
        // overwriting. Emulate replace with an explicit delete-then-insert, kept
        // atomic under one transaction so a repeated `chunk_id` is genuine
        // last-write-wins (re-embed-on-change, `index --force`). When a caller
        // already holds a transaction (batch flush) we join it rather than
        // nesting a BEGIN, which vec0/SQLite would reject.
        // sqlite-vec treats a raw BLOB as float32; vec_int8() reinterprets the
        // bytes as the int8 vector the column expects.
        let write = |conn: &Connection| -> rusqlite::Result<()> {
            conn.execute(
                "DELETE FROM embeddings WHERE chunk_id = ?1",
                rusqlite::params![chunk_id],
            )?;
            conn.execute(
                "INSERT INTO embeddings (chunk_id, embedding) VALUES (?1, vec_int8(?2))",
                rusqlite::params![chunk_id, blob],
            )?;
            // The stored vector now reflects the current input, so the chunk is
            // no longer pending re-embed. Clearing it here — in the same
            // transaction as the vector write — means a kill between the two
            // rolls back both, and a re-embed re-queues cleanly.
            conn.execute(
                "UPDATE chunks SET embed_pending = 0 WHERE id = ?1",
                rusqlite::params![chunk_id],
            )?;
            Ok(())
        };
        if self.conn.is_autocommit() {
            let tx = self.conn.unchecked_transaction()?;
            write(&tx)?;
            tx.commit()?;
        } else {
            write(&self.conn)?;
        }
        Ok(())
    }

    /// Insert or replace a whole batch of embeddings in a single transaction.
    ///
    /// Same per-row replace shape as [`insert_embedding`] (the `embeddings`
    /// vec0 table doesn't honour `INSERT OR REPLACE`, so a repeated
    /// `chunk_id` is emulated with delete-then-insert), but one commit for
    /// the whole batch instead of one implicit autocommit per row (mirrors
    /// the `update_graph_ranks` batch pattern). The embed phase already holds
    /// the whole batch's vectors in memory by the time it writes them, so the
    /// commit boundary is the batch: on an untimely kill the transaction is
    /// rolled back atomically and `chunks_missing_embeddings` re-queues the
    /// entire batch, never a partial one.
    pub fn insert_embeddings(&self, rows: &[(i64, Vec<f32>)]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for (chunk_id, vector) in rows {
            let blob = crate::embeddings::vec_to_int8_blob(vector);
            tx.execute(
                "DELETE FROM embeddings WHERE chunk_id = ?1",
                rusqlite::params![chunk_id],
            )?;
            tx.execute(
                "INSERT INTO embeddings (chunk_id, embedding) VALUES (?1, vec_int8(?2))",
                rusqlite::params![chunk_id, blob],
            )?;
            // Same-transaction clear of the re-embed flag: the batch commit that
            // persists the new vector also marks it current, so a kill mid-batch
            // rolls back both and `chunks_missing_embeddings` re-queues the whole
            // batch (including any pending re-embeds) cleanly.
            tx.execute(
                "UPDATE chunks SET embed_pending = 0 WHERE id = ?1",
                rusqlite::params![chunk_id],
            )?;
        }
        // Held before commit (not after): the crash-safety suite needs the
        // write lock genuinely open here to test a concurrent reader/writer
        // against it, and a real SIGKILL landed here exercises the same
        // uncommitted-batch window `insert_embeddings_shaped_batch_leaves_
        // nothing_after_a_hard_process_exit` proves with a simulated exit.
        super::pause_for_crash_test("embed_tx_open");
        tx.commit()?;
        Ok(())
    }

    /// Delete all embeddings associated with chunks of a given file.
    pub fn delete_embeddings_for_file(&self, file_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM embeddings WHERE chunk_id IN (
                 SELECT id FROM chunks WHERE file_id = ?1
             )",
            rusqlite::params![file_id],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CURRENT_SCHEMA_VERSION, Database, INITIAL_SCHEMA_VERSION, IndexMigrationKind, Result,
    };
    use rusqlite::Connection;
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

    fn user_version(conn: &Connection) -> i32 {
        conn.query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    }

    // The authored/derived split `index_001_initial.sql` states next to
    // `usage`, restated here as the two lists
    // `every_table_in_a_fresh_index_is_classified_as_authored_or_derived`
    // checks a fresh index against.
    const DERIVED_TABLES: &[&str] = &[
        "files",
        "chunks",
        "chunks_fts",
        "embeddings",
        "graph_edges",
        "specs",
        "spec_links",
        "conventions",
        "index_meta",
    ];
    const AUTHORED_TABLES: &[&str] = &["usage"];

    // FTS5 and vec0 each own a set of shadow tables (`chunks_fts_data`,
    // `embeddings_info`, and so on) that no `CREATE TABLE` in the schema
    // declares directly; they belong to their virtual table's classification,
    // not one of their own.
    fn is_virtual_table_shadow(name: &str) -> bool {
        ["chunks_fts", "embeddings"]
            .iter()
            .any(|vt| name.starts_with(&format!("{vt}_")))
    }

    // A table added to `index_001_initial.sql` without a decision about
    // whether `Database::rebuild` needs to carry it across must fail a test,
    // not go unnoticed until a user reports lost data. This enumerates every
    // real table a fresh index actually has and checks each one against the
    // classification above; a table in neither list fails with a message
    // naming the decision to make, not a bare assertion diff. The reverse
    // check (a listed name that no longer exists) catches the classification
    // going stale the other way.
    #[test]
    fn every_table_in_a_fresh_index_is_classified_as_authored_or_derived() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open fresh");
        let mut stmt = db
            .conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            )
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .filter(|n| !is_virtual_table_shadow(n))
            .collect();

        for name in &names {
            assert!(
                DERIVED_TABLES.contains(&name.as_str()) || AUTHORED_TABLES.contains(&name.as_str()),
                "table {name:?} exists in a fresh index.db but is classified as neither \
                 derived nor authored here. Decide whether a rebuild reproduces it correctly \
                 by discarding it (add it to DERIVED_TABLES) or whether it must be carried \
                 across like `usage` (add it to AUTHORED_TABLES and teach Database::rebuild to \
                 carry it), then update index_001_initial.sql's comment to match."
            );
        }
        for name in DERIVED_TABLES.iter().chain(AUTHORED_TABLES) {
            assert!(
                names.iter().any(|n| n == name),
                "the classification names {name:?} but a fresh index.db has no such table; \
                 the list is stale"
            );
        }
    }

    /// A freshly created DB runs every migration and ends stamped at the latest
    /// version.
    #[test]
    fn fresh_db_stamps_current_version() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open fresh");
        assert_eq!(user_version(&db.conn), CURRENT_SCHEMA_VERSION);
    }

    /// Opening an already-migrated DB a second time is a clean no-op that keeps
    /// the version.
    #[test]
    fn reopen_is_idempotent() {
        register_sqlite_vec();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::open(tmp.path()).expect("first open");
        drop(db);
        let db = Database::open(tmp.path()).expect("second open");
        assert_eq!(user_version(&db.conn), CURRENT_SCHEMA_VERSION);
    }

    // ── forward migration (17 and above) ─────────────────────────────────────

    #[test]
    fn migrate_or_rebuild_applies_registered_steps_in_place_and_preserves_existing_rows() {
        register_sqlite_vec();
        let mut db = Database::open(std::path::Path::new(":memory:")).expect("open fresh");
        let file_id = db.upsert_file("kept.rs", Some("rust"), "h", 0).unwrap();

        fn add_marker_18(conn: &Connection) -> Result<()> {
            conn.execute_batch("ALTER TABLE files ADD COLUMN marker_18 TEXT")?;
            Ok(())
        }
        fn add_marker_19(conn: &Connection) -> Result<()> {
            conn.execute_batch("ALTER TABLE files ADD COLUMN marker_19 TEXT")?;
            Ok(())
        }
        let registry = [
            (
                CURRENT_SCHEMA_VERSION + 1,
                IndexMigrationKind::Migrate(add_marker_18),
            ),
            (
                CURRENT_SCHEMA_VERSION + 2,
                IndexMigrationKind::Migrate(add_marker_19),
            ),
        ];

        db.migrate_or_rebuild(
            std::path::Path::new(":memory:"),
            CURRENT_SCHEMA_VERSION,
            CURRENT_SCHEMA_VERSION + 2,
            &registry,
        )
        .expect("migrating forward through registered steps");

        assert_eq!(user_version(&db.conn), CURRENT_SCHEMA_VERSION + 2);
        let path: String = db
            .conn
            .query_row(
                "SELECT path FROM files WHERE id = ?1",
                rusqlite::params![file_id],
                |r| r.get(0),
            )
            .expect("migrating in place must not lose the row that was there before it ran");
        assert_eq!(path, "kept.rs");
    }

    #[test]
    fn migrate_or_rebuild_rebuilds_once_when_the_registry_says_to_and_carries_usage() {
        register_sqlite_vec();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.db");
        let mut db = Database::open(&path).expect("open fresh");
        db.upsert_file("gone.rs", Some("rust"), "h", 0).unwrap();
        db.record_usage("search");

        fn must_not_run(_: &Connection) -> Result<()> {
            panic!("a migrate step registered alongside a later Rebuild entry must never run");
        }
        let registry = [
            (
                CURRENT_SCHEMA_VERSION + 1,
                IndexMigrationKind::Migrate(must_not_run),
            ),
            (CURRENT_SCHEMA_VERSION + 2, IndexMigrationKind::Rebuild),
        ];

        db.migrate_or_rebuild(
            &path,
            CURRENT_SCHEMA_VERSION,
            CURRENT_SCHEMA_VERSION + 2,
            &registry,
        )
        .expect("a rebuild entry must resolve, not error");

        assert_eq!(
            user_version(&db.conn),
            CURRENT_SCHEMA_VERSION,
            "a rebuild always lands the store back at this build's real current schema, \
             whatever synthetic target the caller was migrating toward"
        );
        assert_eq!(
            db.stats().unwrap().file_count,
            0,
            "the rebuild must discard the derived row, exactly like the legacy rebuild path"
        );
        let mut stmt = db.conn.prepare("SELECT command FROM usage").unwrap();
        let commands: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            commands,
            vec!["search".to_string()],
            "usage must survive this rebuild the same way it survives the legacy one"
        );
    }

    #[test]
    fn migrate_or_rebuild_leaves_the_index_at_the_last_completed_version_when_a_step_fails() {
        register_sqlite_vec();
        let mut db = Database::open(std::path::Path::new(":memory:")).expect("open fresh");

        fn add_marker_18(conn: &Connection) -> Result<()> {
            conn.execute_batch("ALTER TABLE files ADD COLUMN marker_18 TEXT")?;
            Ok(())
        }
        fn fail_19(_: &Connection) -> Result<()> {
            anyhow::bail!("synthetic failure for the rollback test")
        }
        let registry = [
            (
                CURRENT_SCHEMA_VERSION + 1,
                IndexMigrationKind::Migrate(add_marker_18),
            ),
            (
                CURRENT_SCHEMA_VERSION + 2,
                IndexMigrationKind::Migrate(fail_19),
            ),
        ];

        let err = db
            .migrate_or_rebuild(
                std::path::Path::new(":memory:"),
                CURRENT_SCHEMA_VERSION,
                CURRENT_SCHEMA_VERSION + 2,
                &registry,
            )
            .expect_err("a failing step must surface as an error");
        assert!(format!("{err:#}").contains("index.db"), "{err}");

        assert_eq!(
            user_version(&db.conn),
            CURRENT_SCHEMA_VERSION + 1,
            "the store must stay at the last version whose step actually completed"
        );
    }

    // ── migration parity ──────────────────────────────────────────────────────

    // Every non-internal `sqlite_master` row (tables, indexes, triggers, and
    // each virtual table's own shadow tables), normalised so incidental
    // whitespace differences in a `CREATE` statement's text don't register as
    // a schema difference. The same comparison `memory.db`'s own parity test
    // uses (`storage::memory::schema_tests::sqlite_master_signature`); no
    // extra handling is needed for `chunks_fts`'s and `embeddings`'s shadow
    // tables here, because both sides below execute byte-identical DDL, and
    // FTS5/vec0 each derive a shadow table's `CREATE` statement
    // deterministically from its virtual table's declaration — two
    // independently created instances of the same declaration produce
    // identical shadow rows, not merely same-shaped ones.
    fn sqlite_master_signature(conn: &Connection) -> Vec<(String, String, String)> {
        let mut stmt = conn
            .prepare(
                "SELECT type, name, COALESCE(sql, '') FROM sqlite_master \
                 WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
            )
            .expect("preparing sqlite_master query");
        stmt.query_map([], |row| {
            let kind: String = row.get(0)?;
            let name: String = row.get(1)?;
            let sql: String = row.get(2)?;
            Ok((
                kind,
                name,
                sql.split_whitespace().collect::<Vec<_>>().join(" "),
            ))
        })
        .expect("querying sqlite_master")
        .collect::<rusqlite::Result<_>>()
        .expect("collecting sqlite_master rows")
    }

    // An index as 1.1 left it: created from the frozen `index_001_initial.sql`
    // and stamped 17, holding one indexed file with a chunk, its embedding and
    // a call edge.
    fn schema_17_store_at(path: &std::path::Path) {
        let conn = Connection::open(path).unwrap();
        let zero_vector = format!("[{}]", vec!["0"; 896].join(","));
        conn.execute_batch(&format!(
            "BEGIN;\n{}\n\
             INSERT INTO files (path, language, hash, indexed_at) VALUES ('src/a.rs', 'rust', 'h', 1);\n\
             INSERT INTO chunks (file_id, node_type, name, start_line, end_line, content) \
                 VALUES (1, 'function', 'go', 1, 3, 'fn go() {{ helper(); }}');\n\
             INSERT INTO embeddings (chunk_id, embedding) VALUES (1, vec_int8('{zero_vector}'));\n\
             INSERT INTO graph_edges (source_file, source_name, target_name, kind, line) \
                 VALUES ('src/a.rs', 'go', 'helper', 'calls', 1);\n\
             PRAGMA user_version = {INITIAL_SCHEMA_VERSION};\nCOMMIT;",
            include_str!("../../migrations/index_001_initial.sql")
        ))
        .unwrap();
    }

    #[test]
    fn a_schema_17_store_migrates_in_place_keeping_its_chunks_and_embeddings() {
        register_sqlite_vec();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        schema_17_store_at(&path);

        let db = Database::open(&path).expect("a schema 17 store must open");

        assert_eq!(user_version(&db.conn), CURRENT_SCHEMA_VERSION);
        assert_eq!(db.rebuilt_from(), None, "migrating must not rebuild");
        let stats = db.stats().unwrap();
        assert_eq!(stats.chunk_count, 1, "the chunk row must survive");
        assert_eq!(stats.embedding_count, 1, "the embedding must survive");
        let target_file: Option<String> = db
            .conn
            .query_row(
                "SELECT target_file FROM graph_edges WHERE target_name = 'helper'",
                [],
                |r| r.get(0),
            )
            .expect("the existing edge gains a target_file column");
        assert_eq!(target_file, None, "an existing edge starts unresolved");
        assert!(
            db.pass_owed(super::GRAPH_EDGES_REEXTRACT).unwrap(),
            "the step must leave the edge re-extraction owed"
        );
    }

    #[test]
    fn a_fresh_store_owes_no_edge_re_extraction() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open fresh");
        assert!(!db.pass_owed(super::GRAPH_EDGES_REEXTRACT).unwrap());
    }

    // A store migrated forward from version 17 must end up indistinguishable
    // from one created fresh. `index_001_initial.sql` is frozen at version 17,
    // and a fresh index climbs the same ladder a 1.1 index does, so this holds
    // by construction; it is what fails first if creation ever stops
    // climbing, or a step's effect depends on the rows it finds.
    #[test]
    fn a_store_migrated_from_schema_version_17_matches_a_fresh_store() {
        register_sqlite_vec();
        let legacy_dir = tempfile::tempdir().unwrap();
        let legacy_path = legacy_dir.path().join("index.db");
        schema_17_store_at(&legacy_path);

        let migrated = Database::open(&legacy_path).expect("open must migrate, not rebuild");
        assert_eq!(user_version(&migrated.conn), CURRENT_SCHEMA_VERSION);
        let fresh = Database::open(std::path::Path::new(":memory:")).expect("open fresh");

        assert_eq!(
            sqlite_master_signature(&migrated.conn),
            sqlite_master_signature(&fresh.conn),
            "a store migrated from schema version 17 must match one created fresh"
        );
    }

    // The code full-text rebuild re-indexes what a 1.1 index already holds,
    // in place: chunks and vectors survive, and the new columns (path,
    // docstring, camelCase parts) are searchable without a reindex.
    #[test]
    fn a_populated_schema_17_index_keeps_its_chunks_and_gains_the_new_full_text_index() {
        register_sqlite_vec();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(&format!(
                "BEGIN;\n{}\nPRAGMA user_version = {INITIAL_SCHEMA_VERSION};\nCOMMIT;",
                include_str!("../../migrations/index_001_initial.sql")
            ))
            .unwrap();
            conn.execute_batch(
                r#"INSERT INTO files (path, language, hash, indexed_at) VALUES
                       ('src/billing/InvoiceDetails.rs', 'rust', 'h', 0);
                   INSERT INTO chunks (file_id, node_type, name, start_line, end_line, content, metadata)
                   VALUES (1, 'function', 'prorateCharge', 1, 3,
                           'fn prorateCharge() { LinearRagSearch::go() }',
                           '{"docstring":"/// Splits a subscription fee","parent_scope":null}');"#,
            )
            .unwrap();
        }

        let db = Database::open(&path).expect("open must migrate, not rebuild");
        assert_eq!(user_version(&db.conn), CURRENT_SCHEMA_VERSION);
        assert_eq!(db.stats().unwrap().chunk_count, 1);
        for q in ["invoice details", "subscription", "prorate", "linear rag"] {
            assert_eq!(
                db.search_text(q, 10).unwrap().len(),
                1,
                "{q:?} must reach the migrated chunk"
            );
        }
    }

    // An upgraded index flags what the embed queue now skips, but keeps the
    // vector a flagged chunk already has.
    #[test]
    fn a_populated_schema_17_index_flags_text_only_chunks_and_keeps_their_vectors() {
        register_sqlite_vec();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(&format!(
                "BEGIN;\n{}\nPRAGMA user_version = {INITIAL_SCHEMA_VERSION};\nCOMMIT;",
                include_str!("../../migrations/index_001_initial.sql")
            ))
            .unwrap();
            conn.execute_batch(
                "INSERT INTO files (path, language, hash, indexed_at) VALUES
                     ('src/lib.rs', 'rust', 'h', 0), ('tests/lib.rs', 'rust', 'h', 0);
                 INSERT INTO chunks (file_id, node_type, name, start_line, end_line, content)
                 VALUES (1, 'function', 'parse', 1, 3, 'fn parse() {}'),
                        (1, 'verbatim', NULL, 4, 9, 'use std::fmt;'),
                        (2, 'function', 'parses', 1, 3, 'fn parses() {}');",
            )
            .unwrap();
        }
        {
            let db = Database::open(&path).expect("open must migrate, not rebuild");
            db.insert_embedding(3, &vec![0.1f32; crate::embeddings::EMBEDDING_DIM])
                .unwrap();
        }

        let db = Database::open(&path).unwrap();
        let stats = db.stats().unwrap();
        assert_eq!(
            (
                stats.chunk_count,
                stats.embedding_count,
                stats.text_only_count
            ),
            (3, 1, 1),
            "the unnamed window is text-only; the embedded test chunk counts as embedded"
        );
        let queued: Vec<i64> = db
            .chunks_missing_embeddings()
            .unwrap()
            .into_iter()
            .map(|row| row.0)
            .collect();
        assert_eq!(queued, vec![1], "only the named product code is queued");
    }

    // ── pass-owed marker ──────────────────────────────────────────────────────

    #[test]
    fn a_pass_owed_marker_round_trips_and_clears() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open");

        assert!(!db.pass_owed("graph_edges_reextract").unwrap());
        db.mark_pass_owed("graph_edges_reextract").unwrap();
        assert!(db.pass_owed("graph_edges_reextract").unwrap());
        db.clear_pass_owed("graph_edges_reextract").unwrap();
        assert!(
            !db.pass_owed("graph_edges_reextract").unwrap(),
            "clearing the marker must retire it, not just overwrite its value"
        );
    }

    // Build something shaped like an index the old ladder wrote: real tables,
    // real rows, stamped at a version this build no longer knows how to read.
    fn legacy_index_at(path: &std::path::Path, stamp: i32) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT, hash TEXT); \
             CREATE TABLE chunks (id INTEGER PRIMARY KEY, file_id INTEGER, content TEXT); \
             CREATE VIRTUAL TABLE chunks_fts USING fts5(content); \
             CREATE VIRTUAL TABLE embeddings USING vec0(\
                 chunk_id INTEGER PRIMARY KEY, embedding FLOAT[768]); \
             CREATE TABLE usage (command TEXT NOT NULL, called_at INTEGER NOT NULL); \
             INSERT INTO files (path, hash) VALUES ('a.rs', 'h'); \
             INSERT INTO chunks (file_id, content) VALUES (1, 'fn a() {{}}'); \
             INSERT INTO usage (command, called_at) VALUES ('search', 11), ('index', 22); \
             PRAGMA user_version = {stamp};"
        ))
        .unwrap();
    }

    #[test]
    fn an_index_from_the_old_ladder_is_rebuilt_at_the_current_schema() {
        register_sqlite_vec();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.db");
        legacy_index_at(&path, super::LAST_LEGACY_SCHEMA_VERSION);

        let db = Database::open(&path).expect("an old index must open, not refuse");

        assert_eq!(user_version(&db.conn), CURRENT_SCHEMA_VERSION);
        assert_eq!(
            db.stats().unwrap().chunk_count,
            0,
            "the old index's derived rows must not survive into a schema that never held them"
        );
        // The vec0 table has to be the current one, not the FLOAT[768] the old
        // file carried: a stale dimension is the failure the deleted upgrade
        // path existed to prevent, and rebuilding has to cover it too.
        let vec_sql: String = db
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'embeddings'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            vec_sql.contains("INT8[896]"),
            "rebuilt vector table kept an old dimension: {vec_sql}"
        );
    }

    #[test]
    fn a_rebuild_carries_usage_across_because_no_reindex_can_reproduce_it() {
        register_sqlite_vec();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.db");
        legacy_index_at(&path, 9);

        let db = Database::open(&path).expect("open");

        let mut stmt = db
            .conn
            .prepare("SELECT command, called_at FROM usage ORDER BY called_at")
            .unwrap();
        let rows: Vec<(String, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            rows,
            vec![("search".to_string(), 11), ("index".to_string(), 22)],
            "usage is the one authored table here; losing it in a rebuild is silent data loss"
        );
    }

    #[test]
    fn a_rebuild_names_the_version_it_replaced_on_the_run_that_replaced_it() {
        register_sqlite_vec();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.db");
        legacy_index_at(&path, 15);

        let db = Database::open(&path).expect("open");

        assert_eq!(
            db.rebuilt_from(),
            Some(15),
            "the run that discarded an index must be able to say so; a tracing::warn! \
             below the CLI's default filter reaches nobody"
        );
    }

    #[test]
    fn a_normal_open_of_a_current_index_reports_nothing() {
        register_sqlite_vec();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.db");

        let db = Database::open(&path).expect("first open");
        assert_eq!(db.rebuilt_from(), None);
        assert_eq!(db.unpopulated_since_rebuild().unwrap(), None);
        drop(db);

        let db = Database::open(&path).expect("second open");
        assert_eq!(db.rebuilt_from(), None);
        assert_eq!(db.unpopulated_since_rebuild().unwrap(), None);
    }

    #[test]
    fn a_rebuilt_index_still_says_it_was_emptied_on_later_opens() {
        register_sqlite_vec();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.db");
        legacy_index_at(&path, 15);
        drop(Database::open(&path).expect("rebuilding open"));

        let db = Database::open(&path).expect("later open");

        assert_eq!(
            db.rebuilt_from(),
            None,
            "a later open rebuilds nothing and must not claim to"
        );
        assert_eq!(
            db.unpopulated_since_rebuild().unwrap(),
            Some(15),
            "the search that meets the emptiness is a later run than the rebuild, so \
             the fact has to outlive the handle that produced it"
        );
    }

    // The tell this replaces was "no files, but usage rows survived". Usage
    // accrues on every `search`, so a store that was init'd over a tree with
    // nothing indexable and then searched wears the same signature without any
    // rebuild ever happening.
    #[test]
    fn an_index_that_was_never_rebuilt_is_not_reported_as_emptied() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open");
        db.record_usage("search");
        db.record_usage("search");

        assert_eq!(db.stats().unwrap().file_count, 0);
        assert_eq!(db.unpopulated_since_rebuild().unwrap(), None);
    }

    #[test]
    fn reindexing_clears_the_rebuild_marker() {
        register_sqlite_vec();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.db");
        legacy_index_at(&path, 15);

        let db = Database::open(&path).expect("open");
        assert_eq!(db.unpopulated_since_rebuild().unwrap(), Some(15));
        db.mark_reindexed().expect("mark reindexed");
        drop(db);

        let db = Database::open(&path).expect("reopen");
        assert_eq!(
            db.unpopulated_since_rebuild().unwrap(),
            None,
            "the marker means `not repopulated since`, so a reindex has to retire it"
        );
    }

    #[test]
    fn an_index_predating_the_stamp_is_rebuilt_rather_than_read_as_fresh() {
        register_sqlite_vec();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.db");
        // user_version 0 with tables present: the shape every pre-stamp release
        // wrote. Reading this as "brand new" would run the schema over live
        // tables and fail on the first CREATE.
        legacy_index_at(&path, 0);

        let db = Database::open(&path).expect("open");
        assert_eq!(user_version(&db.conn), CURRENT_SCHEMA_VERSION);
        assert_eq!(db.stats().unwrap().file_count, 0);
    }

    #[test]
    fn an_index_from_a_newer_build_is_refused_rather_than_rebuilt() {
        register_sqlite_vec();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.db");
        legacy_index_at(&path, CURRENT_SCHEMA_VERSION + 1);

        let msg = match Database::open(&path) {
            Ok(_) => panic!("a future index must not be silently discarded"),
            Err(e) => e.to_string(),
        };
        assert!(
            msg.contains("newer than this build"),
            "wrong refusal for a future index: {msg}"
        );
        // Refused, not damaged.
        let conn = Connection::open(&path).unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM chunks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "a refused open must leave the index untouched");
    }

    #[test]
    fn a_rebuild_leaves_no_stale_sidecar_behind() {
        register_sqlite_vec();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.db");
        legacy_index_at(&path, 12);
        // A -wal from the old file. Left in place it would be replayed into the
        // new database, which is a different schema entirely.
        std::fs::write(path.with_extension("db-wal"), b"stale").unwrap();

        let db = Database::open(&path).expect("open");
        assert_eq!(user_version(&db.conn), CURRENT_SCHEMA_VERSION);
        assert_eq!(db.stats().unwrap().chunk_count, 0);
    }

    /// Model provenance round-trips through index_meta, and a mismatch is a hard
    /// error while an absent model is backfilled.
    #[test]
    fn embedding_model_stamp_reject_and_backfill() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open");
        // Absent → backfilled (no error).
        assert!(db.embedding_model().unwrap().is_none());
        db.ensure_embedding_model("F2LLM-v2-330M@896")
            .expect("first stamp");
        assert_eq!(
            db.embedding_model().unwrap().as_deref(),
            Some("F2LLM-v2-330M@896")
        );
        // Same model → no-op.
        db.ensure_embedding_model("F2LLM-v2-330M@896")
            .expect("same model is fine");
        // Different model → hard error instructing a re-index.
        let err = db
            .ensure_embedding_model("some-other-model@896")
            .expect_err("model mismatch must be a hard error");
        let msg = format!("{err:#}");
        assert!(
            msg.to_lowercase().contains("re-index"),
            "message must instruct re-index: {msg}"
        );
    }

    /// Chunker config provenance round-trips like `embedding_model`, but a
    /// mismatch returns the stale value instead of erroring, and the run
    /// keeps going (a chunk-cap change doesn't corrupt the vector space).
    #[test]
    fn chunker_config_stamp_and_warn_on_mismatch() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open");
        // Absent → backfilled (no error).
        assert!(db.chunker_config().unwrap().is_none());
        assert_eq!(
            db.ensure_chunker_config("max_chunk_tokens=512")
                .expect("first stamp"),
            None
        );
        assert_eq!(
            db.chunker_config().unwrap().as_deref(),
            Some("max_chunk_tokens=512")
        );
        // Same config → no-op, no warning.
        assert_eq!(
            db.ensure_chunker_config("max_chunk_tokens=512")
                .expect("same config is fine"),
            None
        );
        // Different config → the stale value comes back for the caller to
        // warn with, not an error, and the stamp is left untouched.
        let recorded = db
            .ensure_chunker_config("max_chunk_tokens=2048")
            .expect("mismatch must not fail")
            .expect("mismatch must surface the recorded value");
        assert_eq!(recorded, "max_chunk_tokens=512");
        assert_eq!(
            db.chunker_config().unwrap().as_deref(),
            Some("max_chunk_tokens=512"),
            "a mismatch must not overwrite the stamp"
        );
    }

    /// A DB stamped under an old chunker config still lets normal
    /// (non-`--force`) indexing proceed: `ensure_chunker_config` never
    /// blocks the caller, it only reports the drift.
    #[test]
    fn chunker_config_mismatch_does_not_block_incremental_indexing() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open");
        db.ensure_chunker_config("max_chunk_tokens=2048")
            .expect("stamp old config");

        // Simulates a build upgraded to the new default: the check reports
        // the drift but returns `Ok`, so a normal `inkentry index` run keeps
        // going (incremental skip-by-hash still applies to unchanged files).
        let warned = db
            .ensure_chunker_config("max_chunk_tokens=512")
            .expect("a config mismatch must be Ok, not an error");
        assert_eq!(warned.as_deref(), Some("max_chunk_tokens=2048"));
    }

    /// `stamp_chunker_config` is the refresh mechanism a `--force` re-index
    /// uses to silence the drift warning: stamp old, detect the mismatch,
    /// force-refresh, then confirm the same config no longer reports drift.
    #[test]
    fn stamp_chunker_config_silences_a_prior_mismatch() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open");
        db.ensure_chunker_config("max_chunk_tokens=2048")
            .expect("stamp old config");

        // Drift is detected before the refresh.
        assert_eq!(
            db.ensure_chunker_config("max_chunk_tokens=512")
                .expect("mismatch must not fail"),
            Some("max_chunk_tokens=2048".to_string()),
            "the old stamp must still be reported as drift before any refresh"
        );

        // A `--force` run re-chunks everything and refreshes the stamp,
        // unconditionally, not just on a first-ever write.
        db.stamp_chunker_config("max_chunk_tokens=512")
            .expect("force refresh");
        assert_eq!(
            db.chunker_config().unwrap().as_deref(),
            Some("max_chunk_tokens=512")
        );

        // The next normal (non-`--force`) run now sees a match: no drift.
        assert_eq!(
            db.ensure_chunker_config("max_chunk_tokens=512")
                .expect("post-refresh check must not fail"),
            None,
            "after the refresh, the same config must no longer be reported as drift"
        );
    }

    fn embedding_count(db: &Database) -> i64 {
        db.conn
            .query_row("SELECT count(*) FROM embeddings", [], |r| r.get(0))
            .unwrap()
    }

    fn chunk_pending(db: &Database, chunk_id: i64) -> i64 {
        db.conn
            .query_row(
                "SELECT embed_pending FROM chunks WHERE id = ?1",
                rusqlite::params![chunk_id],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// A fresh index is stamped with the current summary scheme and marks nothing
    /// pending — the flag never fires on the fresh path.
    #[test]
    fn fresh_open_stamps_summary_scheme_and_marks_nothing_pending() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open fresh");
        assert_eq!(
            db.summary_scheme().unwrap().as_deref(),
            Some(crate::indexer::summariser::SUMMARY_SCHEME)
        );
        assert_eq!(db.refresh_pending_count().unwrap(), 0);
    }

    /// The re-embed flag is cleared in the same transaction as the vector write:
    /// a batch that re-embeds a pending chunk both persists the new vector and
    /// clears `embed_pending`, atomically.
    #[test]
    fn insert_embeddings_clears_embed_pending_in_the_same_transaction() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open");
        let dim = crate::embeddings::EMBEDDING_DIM;
        let file_id = db.upsert_file("a.rs", Some("rust"), "h", 0).unwrap();
        let c = db
            .insert_chunk(file_id, "function", Some("a"), 1, 2, "fn a(){}", None, 4)
            .unwrap();
        db.conn
            .execute(
                "UPDATE chunks SET embed_pending = 1 WHERE id = ?1",
                rusqlite::params![c],
            )
            .unwrap();

        db.insert_embeddings(&[(c, vec![0.1f32; dim])]).unwrap();
        assert_eq!(
            chunk_pending(&db, c),
            0,
            "a re-embed must clear the pending flag alongside the vector write"
        );
    }

    /// The batch insert writes every row of a batch in one call.
    #[test]
    fn insert_embeddings_commits_the_whole_batch() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open");
        let rows = vec![
            (1i64, vec![0.1f32; crate::embeddings::EMBEDDING_DIM]),
            (2i64, vec![0.1f32; crate::embeddings::EMBEDDING_DIM]),
            (3i64, vec![0.1f32; crate::embeddings::EMBEDDING_DIM]),
        ];
        db.insert_embeddings(&rows).expect("batch insert");
        assert_eq!(embedding_count(&db), 3, "all three rows persist");
    }

    /// The batch is a single transaction: if any row fails, none commit. This
    /// is the guarantee the resume story rests on — a process killed while a
    /// batch is being written leaves zero partial rows behind, so
    /// `chunks_missing_embeddings` re-queues the whole batch cleanly. A per-row
    /// autocommit loop would instead leak the rows written before the failure.
    #[test]
    fn insert_embeddings_is_atomic_a_failing_row_rolls_back_the_whole_batch() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open");
        // The second row has the wrong dimension; sqlite-vec rejects it at
        // insert time, aborting the transaction after the first row was staged.
        let rows = vec![
            (1i64, vec![0.1f32; crate::embeddings::EMBEDDING_DIM]),
            (2i64, vec![0.1f32; crate::embeddings::EMBEDDING_DIM - 1]),
        ];
        db.insert_embeddings(&rows)
            .expect_err("a wrong-dimension row must fail the whole batch");
        assert_eq!(
            embedding_count(&db),
            0,
            "an atomic batch leaves zero rows when any row fails; the first, valid \
             row must not survive the aborted transaction"
        );
    }

    /// An empty batch is a deliberate no-op, not an error. `run_embed_phase`
    /// never constructs one today (batches are only built from a non-empty
    /// slice of the work queue), but the boundary must still be safe.
    #[test]
    fn insert_embeddings_empty_batch_is_a_no_op() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open");
        db.insert_embeddings(&[])
            .expect("an empty batch must not error");
        assert_eq!(embedding_count(&db), 0);
    }

    /// A batch of exactly one row commits normally — the boundary case
    /// closest to the old per-row behaviour must not silently regress to a
    /// non-transactional bypass.
    #[test]
    fn insert_embeddings_single_row_batch_commits() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open");
        let rows = vec![(1i64, vec![0.1f32; crate::embeddings::EMBEDDING_DIM])];
        db.insert_embeddings(&rows)
            .expect("single-row batch insert");
        assert_eq!(embedding_count(&db), 1);
    }

    /// Was a bug (see `git blame`/ADR-070): `insert_embedding`'s doc-comment
    /// promises "insert or replace", but plain `INSERT OR REPLACE` against the
    /// `embeddings` vec0 virtual table does not honour the conflict clause —
    /// a second call for the same `chunk_id` raised `UNIQUE constraint
    /// failed` instead of overwriting. This mattered because the run-level
    /// resume test's own comment and the batch engineer's handoff note both
    /// cited OR-REPLACE idempotency as a safety property to lean on. Fixed by
    /// emulating replace with an explicit delete-then-insert (see
    /// `insert_embedding`); this test now pins the fixed, promised behaviour.
    #[test]
    fn insert_embedding_single_row_path_does_not_actually_replace_a_repeated_chunk_id() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open");
        db.insert_embedding(1, &vec![0.1f32; crate::embeddings::EMBEDDING_DIM])
            .expect("first insert");
        db.insert_embedding(1, &vec![0.9f32; crate::embeddings::EMBEDDING_DIM])
            .expect(
                "replacing an already-committed chunk_id must not error — this is the doc-comment's \
                 promised behaviour and currently fails",
            );
        assert_eq!(embedding_count(&db), 1);
    }

    /// Same underlying bug as the test above, exercised through the batch
    /// path this story added: a batch containing the same `chunk_id` twice
    /// (still legitimate input — nothing in `insert_embeddings`'s contract
    /// forbids it) used to hit the identical `UNIQUE constraint failed`
    /// error, because it was the same OR-REPLACE-against-vec0 gap, not
    /// something the transaction wrapper introduced. `insert_embeddings` now
    /// applies the same delete-then-insert-per-row fix inside its batch
    /// transaction, so a repeated id within one batch collapses to a single
    /// last-write-wins row instead of erroring.
    #[test]
    fn insert_embeddings_duplicate_chunk_id_within_one_batch_last_write_wins() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open");
        let rows = vec![
            (1i64, vec![0.1f32; crate::embeddings::EMBEDDING_DIM]),
            (1i64, vec![0.9f32; crate::embeddings::EMBEDDING_DIM]),
        ];
        db.insert_embeddings(&rows)
            .expect("a duplicate chunk_id within a batch must not error");
        assert_eq!(
            embedding_count(&db),
            1,
            "one logical chunk_id must produce exactly one row, not two"
        );
    }

    /// The batch ceiling is 256 chunks (`resolve_batch_ceiling`'s default) —
    /// confirm the transaction wrapper itself has no lower internal limit
    /// (e.g. SQLite's bound statement/variable count) that would make a
    /// full-size real batch behave differently from the small batches every
    /// other test here uses.
    #[test]
    fn insert_embeddings_handles_a_full_size_256_batch() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open");
        let rows: Vec<(i64, Vec<f32>)> = (1i64..=256)
            .map(|id| (id, vec![0.1f32; crate::embeddings::EMBEDDING_DIM]))
            .collect();
        db.insert_embeddings(&rows).expect("full-size batch insert");
        assert_eq!(embedding_count(&db), 256);
    }

    /// The other atomicity test triggers rollback via a sqlite-vec dimension
    /// check, which is an application-level guard, not a generic SQLite
    /// failure. Prove the same "whole batch or nothing" guarantee holds for a
    /// genuine SQLite runtime error too: hold the file's write lock from a
    /// second connection (no `busy_timeout` is configured — see
    /// `Database::open`) so `insert_embeddings`'s own write hits `SQLITE_BUSY`
    /// on the very first row, unrelated to any row's content.
    #[test]
    fn insert_embeddings_rolls_back_on_a_real_sqlite_error_not_just_bad_dimension() {
        register_sqlite_vec();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::open(tmp.path()).expect("open");

        // A second connection takes and holds the file's write lock.
        let locker = Connection::open(tmp.path()).expect("second connection");
        locker
            .execute_batch(
                "BEGIN IMMEDIATE; \
                 INSERT OR REPLACE INTO index_meta (key, value) VALUES ('lock_probe', '1');",
            )
            .expect("acquire the write lock");

        let rows = vec![
            (1i64, vec![0.1f32; crate::embeddings::EMBEDDING_DIM]),
            (2i64, vec![0.1f32; crate::embeddings::EMBEDDING_DIM]),
        ];
        let err = db
            .insert_embeddings(&rows)
            .expect_err("a locked database must surface as a real error, not silently succeed");
        let msg = format!("{err:#}").to_lowercase();
        assert!(
            msg.contains("lock") || msg.contains("busy"),
            "expected a locking error, got: {msg}"
        );

        locker.execute_batch("COMMIT;").expect("release the lock");
        assert_eq!(
            embedding_count(&db),
            0,
            "a batch that fails under lock contention leaves zero rows, the same atomicity \
             guarantee the bad-dimension case exercises"
        );

        // The connection recovers cleanly once the lock is released — this
        // was not a poisoned/half-open transaction.
        db.insert_embeddings(&rows)
            .expect("insert succeeds once the lock is released");
        assert_eq!(embedding_count(&db), 2);
    }

    /// The batch change makes the write transaction live for the whole batch
    /// instead of a single row, so it holds the writer lock longer than the
    /// old per-row autocommit ever did. WAL mode should still let a concurrent
    /// reader (e.g. `inkentry search` running mid-embed) proceed rather than
    /// blocking or erroring — verify this empirically instead of assuming it.
    #[test]
    fn open_batch_transaction_does_not_block_a_concurrent_reader() {
        register_sqlite_vec();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::open(tmp.path()).expect("open");

        // Hold open the same transaction/insert shape `insert_embeddings`
        // uses, uncommitted, mimicking a batch write still in flight.
        let tx = db.conn.unchecked_transaction().expect("begin");
        let blob =
            crate::embeddings::vec_to_int8_blob(&vec![0.2f32; crate::embeddings::EMBEDDING_DIM]);
        tx.execute(
            "INSERT OR REPLACE INTO embeddings (chunk_id, embedding) VALUES (?1, vec_int8(?2))",
            rusqlite::params![1i64, blob],
        )
        .expect("staged write inside the open transaction");

        // A second connection, mimicking a concurrent `inkentry search` reader.
        let reader = Connection::open(tmp.path()).expect("second connection");
        let count: i64 = reader
            .query_row("SELECT count(*) FROM embeddings", [], |r| r.get(0))
            .expect(
                "a concurrent reader must not be blocked or errored by an open, uncommitted \
                 batch write transaction — WAL mode is expected to allow this",
            );
        assert_eq!(
            count, 0,
            "the reader sees the pre-transaction snapshot, not the uncommitted staged row \
             (WAL snapshot isolation)"
        );

        // The real code path a concurrent `inkentry search` takes — a sqlite-vec
        // KNN `MATCH` query, not a plain `SELECT count(*)` — against the same
        // virtual table the open transaction is writing into. `Database` opens
        // its own connection, so build a second `Database` over the reader's
        // (already-migrated) file rather than a raw `Connection`.
        let reader_db = Database {
            conn: reader,
            rebuilt_from: None,
        };
        reader_db
            .search_similar(&vec![0.2f32; crate::embeddings::EMBEDDING_DIM], 5)
            .expect(
                "a concurrent KNN MATCH query against the embeddings vec0 table must not be \
                 blocked or errored by the open writer transaction either — vec0 virtual \
                 tables don't always share ordinary tables' WAL locking behaviour, so this is \
                 checked separately from the plain SELECT above",
            );

        tx.commit().expect("writer commits");
        let count_after: i64 = reader_db
            .conn
            .query_row("SELECT count(*) FROM embeddings", [], |r| r.get(0))
            .expect("reader still works after the writer commits");
        assert_eq!(count_after, 1, "reader's next read sees the committed row");
    }

    /// The run-level resume regression test (`embed_phase.rs`) simulates an
    /// interrupted batch by never calling `insert_embeddings` at all (the
    /// mock server 500s before the batch write would happen) — a weaker
    /// guarantee than the spec's "kill mid-batch" acceptance criterion, since
    /// it never proves anything about a transaction that *was* opened and
    /// *was* partway through writing when the process died.
    ///
    /// This test closes that gap literally: a child process opens the same
    /// on-disk DB, stages every row of a batch inside an open transaction,
    /// then hard-exits via `std::process::exit` — which runs no destructors,
    /// so neither `COMMIT` nor `ROLLBACK` is ever sent, the closest safe
    /// stand-in for a `SIGKILL` mid-commit (a real signal would skip Drop the
    /// same way; unlike an in-process leak, `std::process::exit` still lets
    /// the OS release the file lock, so the parent can reopen cleanly — a
    /// leaked `Connection` in the same process cannot be observed this way,
    /// since the lock would never clear). The child prints a marker after
    /// staging so a filter/argv mismatch can never silently no-op this test
    /// into a false pass.
    #[test]
    fn insert_embeddings_shaped_batch_leaves_nothing_after_a_hard_process_exit() {
        const HELPER_ENV: &str = "INKENTRY_TEST_CRASH_MID_BATCH_DB_PATH";
        const STAGED_MARKER: &str = "INKENTRY_TEST_CRASH_MID_BATCH_STAGED";

        if let Ok(path) = std::env::var(HELPER_ENV) {
            // Child mode: stage a 3-row batch inside an open transaction using
            // the exact insert shape `insert_embeddings` uses, then hard-exit
            // before commit or rollback.
            register_sqlite_vec();
            let db = Database::open(std::path::Path::new(&path)).expect("child open");
            let tx = db.conn.unchecked_transaction().expect("child begin");
            for chunk_id in 1i64..=3 {
                let blob = crate::embeddings::vec_to_int8_blob(&vec![
                    0.3f32;
                    crate::embeddings::EMBEDDING_DIM
                ]);
                tx.execute(
                    "INSERT OR REPLACE INTO embeddings (chunk_id, embedding) VALUES \
                     (?1, vec_int8(?2))",
                    rusqlite::params![chunk_id, blob],
                )
                .expect("child staged write");
            }
            println!("{STAGED_MARKER}");
            std::process::exit(0);
        }

        register_sqlite_vec();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        // Pre-create the schema so the child doesn't race the parent on
        // migrations.
        Database::open(tmp.path()).expect("pre-create schema");

        let exe = std::env::current_exe().expect("current test binary");
        let output = std::process::Command::new(exe)
            .arg("--exact")
            .arg(
                "storage::db::tests::insert_embeddings_shaped_batch_leaves_nothing_after_a_hard_process_exit",
            )
            .arg("--test-threads=1")
            .arg("--nocapture")
            .env(HELPER_ENV, tmp.path())
            .output()
            .expect("spawn the crash-simulation child");

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "the child must hard-exit cleanly (code 0); stdout:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            stdout.contains(STAGED_MARKER),
            "the child must actually reach and execute the staged-write path (guards against \
             a test-name/filter mismatch silently matching zero tests and false-passing); \
             stdout:\n{stdout}"
        );

        let reopened = Database::open(tmp.path()).expect("reopen after the simulated crash");
        assert_eq!(
            embedding_count(&reopened),
            0,
            "a batch abandoned by a hard process exit before commit must leave zero rows — the \
             literal 'kill mid-batch' scenario, not just an in-process Err short-circuit"
        );
    }

    /// Two independent, fully-committed `insert_embedding` calls for the same
    /// `chunk_id` must leave exactly one row holding the *second* vector — the
    /// re-embed-on-content-change idempotency the resume/`index --force` paths
    /// assume. On a `vec0` virtual table plain `INSERT OR REPLACE` silently
    /// fails to do this (the conflict clause isn't honoured), so this pins the
    /// delete-then-insert fix.
    #[test]
    fn insert_embedding_single_row_path_replaces_a_repeated_chunk_id() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open");
        let dim = crate::embeddings::EMBEDDING_DIM;

        let mut first = vec![0f32; dim];
        first[0] = 1.0;
        let mut second = vec![0f32; dim];
        second[10] = 1.0;

        db.insert_embedding(1, &first).expect("first insert");
        db.insert_embedding(1, &second)
            .expect("second insert (replace)");

        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM embeddings WHERE chunk_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "a repeated chunk_id must leave exactly one row");

        let stored: Vec<u8> = db
            .conn
            .query_row(
                "SELECT embedding FROM embeddings WHERE chunk_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            stored,
            crate::embeddings::vec_to_int8_blob(&second),
            "the second insert must overwrite the first (last-write-wins)"
        );
    }

    /// The same duplicate-`chunk_id` sequence inside a single explicit
    /// transaction (mirroring a batch embed that flushes many rows under one
    /// `BEGIN`) must also collapse to one last-write-wins row.
    #[test]
    fn insert_embedding_duplicate_chunk_id_within_one_transaction_last_write_wins() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open");
        let dim = crate::embeddings::EMBEDDING_DIM;

        let mut a = vec![0f32; dim];
        a[1] = 1.0;
        let mut b = vec![0f32; dim];
        b[2] = 1.0;
        let mut c = vec![0f32; dim];
        c[3] = 1.0;

        {
            let tx = db.conn.unchecked_transaction().unwrap();
            db.insert_embedding(7, &a).unwrap();
            db.insert_embedding(7, &b).unwrap();
            db.insert_embedding(7, &c).unwrap();
            tx.commit().unwrap();
        }

        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM embeddings WHERE chunk_id = 7",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 1,
            "duplicate chunk_ids in one batch collapse to one row"
        );

        let stored: Vec<u8> = db
            .conn
            .query_row(
                "SELECT embedding FROM embeddings WHERE chunk_id = 7",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            stored,
            crate::embeddings::vec_to_int8_blob(&c),
            "the last write in the batch must win"
        );
    }

    /// Replacing a `chunk_id` that has never been inserted must be a harmless
    /// no-op DELETE followed by a normal INSERT — not an error. This is the
    /// overwhelmingly common real-world call pattern (indexing a chunk for the
    /// first time), so it must not regress under the delete-then-insert fix.
    #[test]
    fn insert_embedding_of_nonexistent_chunk_id_is_a_harmless_delete_no_op() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open");
        let dim = crate::embeddings::EMBEDDING_DIM;

        let mut vector = vec![0f32; dim];
        vector[3] = 1.0;

        db.insert_embedding(42, &vector)
            .expect("inserting a never-before-seen chunk_id must succeed");

        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM embeddings WHERE chunk_id = 42",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 1,
            "the first insert for a fresh id must land exactly once"
        );

        let stored: Vec<u8> = db
            .conn
            .query_row(
                "SELECT embedding FROM embeddings WHERE chunk_id = 42",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, crate::embeddings::vec_to_int8_blob(&vector));
    }

    /// The strongest test of "joins the existing transaction" vs. "just happens
    /// not to error": call `insert_embedding` for a repeated `chunk_id` from
    /// WITHIN a transaction the caller already opened, then roll that outer
    /// transaction back. If the delete+insert genuinely joined the caller's
    /// transaction (rather than, say, silently nesting a SAVEPOINT that
    /// commits independently), rolling back the outer transaction must undo
    /// both the delete and the insert, restoring the pre-transaction row
    /// exactly.
    #[test]
    fn insert_embedding_joins_callers_transaction_and_rolls_back_with_it() {
        register_sqlite_vec();
        let db = Database::open(std::path::Path::new(":memory:")).expect("open");
        let dim = crate::embeddings::EMBEDDING_DIM;

        let mut first = vec![0f32; dim];
        first[0] = 1.0;
        db.insert_embedding(1, &first)
            .expect("seed row (autocommit)");

        let mut second = vec![0f32; dim];
        second[1] = 1.0;

        {
            let tx = db
                .conn
                .unchecked_transaction()
                .expect("caller opens an outer transaction");
            assert!(
                !db.conn.is_autocommit(),
                "precondition: connection must be mid-transaction, exercising the \
                 is_autocommit() guard's join branch rather than its own-BEGIN branch"
            );

            // Must not attempt a nested BEGIN (vec0/SQLite would reject it) —
            // simply not erroring here already covers that. The real test is
            // below: did it join *this* transaction, or silently commit on its
            // own?
            db.insert_embedding(1, &second)
                .expect("replacing inside the caller's open transaction must not nest a BEGIN");

            tx.rollback().expect("roll back the outer transaction");
        }

        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM embeddings WHERE chunk_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 1,
            "rollback must not leave the row deleted — the DELETE half of the \
             replace was part of the outer transaction and must roll back with it"
        );

        let stored: Vec<u8> = db
            .conn
            .query_row(
                "SELECT embedding FROM embeddings WHERE chunk_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            stored,
            crate::embeddings::vec_to_int8_blob(&first),
            "rollback must restore the pre-transaction (first) vector — if the \
             delete+insert had committed independently of the caller's \
             transaction, the row would still hold `second` here"
        );
    }

    /// The `embeddings` table runs in WAL mode (`Database::open`). A repeated
    /// `chunk_id` replace is delete-then-insert; if those two statements were
    /// not wrapped in one atomic transaction, a concurrent reader (e.g. a
    /// search query racing an index refresh) could observe a window with zero
    /// rows for that id between the DELETE committing and the INSERT
    /// committing. Drive many replaces on one connection while a second,
    /// independent connection continuously polls the row count, and assert
    /// the reader never observes zero.
    #[test]
    fn insert_embedding_replace_has_no_zero_row_window_visible_to_a_concurrent_reader() {
        register_sqlite_vec();
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let path = tmp.path().to_path_buf();
        let db = Database::open(&path).expect("open file-backed db (WAL mode)");
        let dim = crate::embeddings::EMBEDDING_DIM;

        let mut seed = vec![0f32; dim];
        seed[0] = 1.0;
        db.insert_embedding(1, &seed).expect("seed row");

        let reader = Connection::open(&path).expect("independent reader connection");
        reader
            .execute_batch("PRAGMA busy_timeout = 5000;")
            .expect("reader busy timeout");

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let saw_zero = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let iterations_observed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stop_reader = stop.clone();
        let saw_zero_reader = saw_zero.clone();
        let iterations_reader = iterations_observed.clone();

        let reader_thread = std::thread::spawn(move || {
            while !stop_reader.load(std::sync::atomic::Ordering::Relaxed) {
                let count: i64 = reader
                    .query_row(
                        "SELECT COUNT(*) FROM embeddings WHERE chunk_id = 1",
                        [],
                        |r| r.get(0),
                    )
                    .expect("reader query must not error under WAL");
                iterations_reader.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if count == 0 {
                    saw_zero_reader.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        });

        let mut v = vec![0f32; dim];
        for i in 0..500 {
            v[i % dim] = 1.0;
            db.insert_embedding(1, &v).expect("replace");
        }

        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        reader_thread.join().expect("reader thread must not panic");

        assert!(
            iterations_observed.load(std::sync::atomic::Ordering::Relaxed) > 0,
            "sanity check: the reader must actually have raced the writer"
        );
        assert!(
            !saw_zero.load(std::sync::atomic::Ordering::Relaxed),
            "a concurrent WAL reader must never observe zero rows for chunk_id=1 \
             mid-replace — the delete+insert must commit atomically as one \
             transaction, not as two independently-visible statements"
        );
    }
}
